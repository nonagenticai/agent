//! B12 schema (migrations 0060, 0061, 0063, 0067, 0120, 0121): tenant and job isolation, the per-job id counters,
//! one open handover item per case, append-only timelines, the widened channel CHECK, provider-retry dedupe and
//! the edge's cross-tenant binding lookup. Real Postgres 18 (one container for this binary).
use crate::common;

use agent_api::tenancy::begin_scoped;
use common::{insert_tenant, sqlstate};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

async fn insert_job(db: &PgPool, tenant: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, $2, '[]', 'test') RETURNING id",
    )
    .bind(tenant)
    .bind(key)
    .fetch_one(db)
    .await
    .unwrap()
}

/// A transaction as `agent_app`, scoped to `tenant` and (when given) `job`.
async fn scoped(db: &PgPool, tenant: Uuid, job: Option<Uuid>) -> Transaction<'static, Postgres> {
    let mut tx = begin_scoped(db, tenant).await.unwrap();
    if let Some(job) = job {
        sqlx::query("SELECT set_config('app.current_job', $1, true)")
            .bind(job.to_string())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx
}

async fn insert_conversation(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    conv: &str,
    channel: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel) VALUES ($1, $2, $3, 'prod', $4)",
    )
    .bind(tenant)
    .bind(job)
    .bind(conv)
    .bind(channel)
    .execute(db)
    .await
    .map(|_| ())
}

async fn insert_case(db: &PgPool, tenant: Uuid, job: Uuid, case: &str, conv: &str) {
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env) \
         VALUES ($1, $2, $3, $4, 'web_chat', 'prod')",
    )
    .bind(tenant)
    .bind(job)
    .bind(case)
    .bind(conv)
    .execute(db)
    .await
    .unwrap();
}

async fn insert_item(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    id: &str,
    case: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO agent_handover_items (tenant_id, job_id, id, case_id, conversation_id, env, reason) \
         VALUES ($1, $2, $3, $4, 'conv-1', 'prod', 'customer_asked')",
    )
    .bind(tenant)
    .bind(job)
    .bind(id)
    .bind(case)
    .execute(db)
    .await
    .map(|_| ())
}

/// Two tenants, each with one job, a conversation `conv-1`, a case `CS-1` and an open item `HO-1`.
async fn two_tenants(owner: &PgPool) -> ((Uuid, Uuid), (Uuid, Uuid)) {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(owner, a, "Alpha").await;
    insert_tenant(owner, b, "Beta").await;
    let ja = insert_job(owner, a, "care").await;
    let jb = insert_job(owner, b, "care").await;
    for (t, j) in [(a, ja), (b, jb)] {
        insert_conversation(owner, t, j, "conv-1", "web_chat")
            .await
            .unwrap();
        insert_case(owner, t, j, "CS-1", "conv-1").await;
        insert_item(owner, t, j, "HO-1", "CS-1").await.unwrap();
    }
    ((a, ja), (b, jb))
}

#[tokio::test]
async fn tenant_a_sees_only_its_conversations_and_handover_items() {
    let e = common::env().await;
    let ((a, _ja), (b, jb)) = two_tenants(&e.owner).await;
    let mut tx = scoped(&e.state.db, a, None).await;
    for table in ["conversation", "agent_handover_items", "agent_case"] {
        let all: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        let theirs: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE tenant_id = $1"
        )))
        .bind(b)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        assert_eq!((all, theirs), (1, 0), "{table}");
    }
    // and cannot write into B either
    let err = sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel) \
         VALUES ($1, $2, 'conv-x', 'prod', 'web_chat')",
    )
    .bind(b)
    .bind(jb)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
}

#[tokio::test]
async fn job_scope_hides_the_other_jobs_rows() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    let j1 = insert_job(&e.owner, a, "care").await;
    let j2 = insert_job(&e.owner, a, "sales").await;
    insert_conversation(&e.owner, a, j1, "conv-1", "web_chat")
        .await
        .unwrap();
    insert_conversation(&e.owner, a, j2, "conv-2", "teams")
        .await
        .unwrap();
    let mut tx = scoped(&e.state.db, a, Some(j1)).await;
    let seen: Vec<String> = sqlx::query_scalar("SELECT conversation_id FROM conversation")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, vec!["conv-1".to_string()]);
    let err = sqlx::query(
        "INSERT INTO conversation (job_id, conversation_id, env, channel) VALUES ($1, 'conv-3', 'prod', 'slack')",
    )
    .bind(j2)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    drop(tx);
    // unscoped by job: the tenant sees both
    let mut tx = scoped(&e.state.db, a, None).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM conversation")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn case_and_handover_ids_count_per_job() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    let j1 = insert_job(&e.owner, a, "care").await;
    let j2 = insert_job(&e.owner, a, "sales").await;
    let next = |job: Uuid, f: &'static str| {
        let db = e.state.db.clone();
        async move {
            let mut tx = scoped(&db, a, Some(job)).await;
            let id: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT {f}()")))
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            tx.commit().await.unwrap();
            id
        }
    };
    assert_eq!(next(j1, "agent_next_case_id").await, "CS-1");
    assert_eq!(next(j1, "agent_next_case_id").await, "CS-2");
    assert_eq!(next(j2, "agent_next_case_id").await, "CS-1");
    assert_eq!(next(j1, "agent_next_case_id").await, "CS-3");
    assert_eq!(next(j1, "agent_next_handover_id").await, "HO-1");
    assert_eq!(next(j1, "agent_next_handover_id").await, "HO-2");
    assert_eq!(next(j2, "agent_next_handover_id").await, "HO-1");
    // another tenant starts at 1 too
    let b = Uuid::new_v4();
    insert_tenant(&e.owner, b, "Beta").await;
    let jb = insert_job(&e.owner, b, "care").await;
    let mut tx = scoped(&e.state.db, b, Some(jb)).await;
    let id: String = sqlx::query_scalar("SELECT agent_next_case_id()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(id, "CS-1");
    drop(tx);
    // without a job scope there is no counter to advance
    let mut tx = scoped(&e.state.db, a, None).await;
    let err = sqlx::query_scalar::<_, String>("SELECT agent_next_case_id()")
        .fetch_one(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23502"), "{err}");
}

#[tokio::test]
async fn one_open_handover_item_per_case() {
    let e = common::env().await;
    let ((a, ja), _) = two_tenants(&e.owner).await;
    let err = insert_item(&e.owner, a, ja, "HO-2", "CS-1")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
    // a closed state needs closed_at, and closing frees the case for a new item
    let err = sqlx::query(
        "UPDATE agent_handover_items SET state = 'resolved' WHERE tenant_id = $1 AND id = 'HO-1'",
    )
    .bind(a)
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
    sqlx::query(
        "UPDATE agent_handover_items SET state = 'resolved', closed_at = now() WHERE tenant_id = $1 AND id = 'HO-1'",
    )
    .bind(a)
    .execute(&e.owner)
    .await
    .unwrap();
    insert_item(&e.owner, a, ja, "HO-2", "CS-1")
        .await
        .expect("a new item once the open one is closed");
}

#[tokio::test]
async fn timelines_are_append_only_for_the_app_role() {
    let e = common::env().await;
    let ((a, ja), _) = two_tenants(&e.owner).await;
    let mut tx = scoped(&e.state.db, a, Some(ja)).await;
    sqlx::query("INSERT INTO case_event (case_id, kind, body) VALUES ('CS-1', 'state', '{\"to\":\"open\"}')")
        .execute(&mut *tx)
        .await
        .expect("insert a case event");
    sqlx::query("INSERT INTO agent_handover_events (item_id, actor, to_state) VALUES ('HO-1', 'agent', 'queued')")
        .execute(&mut *tx)
        .await
        .expect("insert a handover event");
    tx.commit().await.unwrap();
    for stmt in [
        "UPDATE case_event SET kind = 'alert'",
        "DELETE FROM case_event",
        "UPDATE agent_handover_events SET note = 'x'",
        "DELETE FROM agent_handover_events",
    ] {
        let mut tx = scoped(&e.state.db, a, Some(ja)).await;
        let err = sqlx::query(stmt).execute(&mut *tx).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{stmt}: {err}");
    }
}

#[tokio::test]
async fn channel_check_rejects_unknown_channels() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    let j = insert_job(&e.owner, a, "care").await;
    let err = insert_conversation(&e.owner, a, j, "conv-fax", "fax")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
    assert_eq!(
        err.as_database_error().and_then(|d| d.constraint()),
        Some("conversation_channel_check")
    );
    for ch in [
        "staff", "telegram", "web_chat", "teams", "slack", "whatsapp", "email", "voice", "api",
    ] {
        insert_conversation(&e.owner, a, j, &format!("conv-{ch}"), ch)
            .await
            .unwrap_or_else(|err| panic!("{ch}: {err}"));
    }
    let named: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint WHERE contype = 'c' AND conname::text = ANY ($1)",
    )
    .bind(vec![
        "agent_case_channel_check",
        "conversation_channel_check",
        // 0190 dropped `agent_handover_items.channel` (its view derives it from the conversation)
        "case_csat_channel_check",
        "channel_conversation_refs_channel_check",
    ])
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        named, 4,
        "every channel CHECK carries its <table>_channel_check name"
    );
}

#[tokio::test]
async fn provider_retry_is_a_duplicate_message() {
    let e = common::env().await;
    let ((a, ja), _) = two_tenants(&e.owner).await;
    let mut tx = scoped(&e.state.db, a, Some(ja)).await;
    let insert = "INSERT INTO conversation_message (conversation_id, role, text, at_ms, external_id) \
                  VALUES ('conv-1', 'customer', 'hi', 1, $1)";
    sqlx::query(insert)
        .bind("wamid.1")
        .execute(&mut *tx)
        .await
        .unwrap();
    // no external id: never deduplicated
    for _ in 0..2 {
        sqlx::query(insert)
            .bind(None::<String>)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    let err = sqlx::query(insert)
        .bind("wamid.1")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
}

#[tokio::test]
async fn csat_rating_is_one_to_five_with_its_answer_time() {
    let e = common::env().await;
    let ((a, ja), _) = two_tenants(&e.owner).await;
    let mut tx = scoped(&e.state.db, a, Some(ja)).await;
    sqlx::query("INSERT INTO case_csat (case_id, conversation_id, channel, env) VALUES ('CS-1', 'conv-1', 'web_chat', 'prod')")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    for (rating, answered, code) in [(Some(6_i16), true, "23514"), (Some(4), false, "23514")] {
        let mut tx = scoped(&e.state.db, a, Some(ja)).await;
        let err = sqlx::query(
            "UPDATE case_csat SET rating = $1, answered_at = CASE WHEN $2 THEN now() END, \
                    state = CASE WHEN $2 THEN 'answered' ELSE state END WHERE case_id = 'CS-1'",
        )
        .bind(rating)
        .bind(answered)
        .execute(&mut *tx)
        .await
        .unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some(code), "{rating:?}: {err}");
    }
    let mut tx = scoped(&e.state.db, a, Some(ja)).await;
    sqlx::query(
        "UPDATE case_csat SET rating = 4, answered_at = now(), state = 'answered' WHERE case_id = 'CS-1'",
    )
        .execute(&mut *tx)
        .await
        .expect("record the answer");
    // only the answer columns may change
    let err = sqlx::query("UPDATE case_csat SET channel = 'teams' WHERE case_id = 'CS-1'")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
}

/// 0208: the state CHECK ties `state` to its timestamps, `asked_by_sub` is for staff only and a comment is 1..=1000
/// characters, asked for and answered.
#[tokio::test]
async fn csat_lifecycle_checks_hold() {
    let e = common::env().await;
    let ((a, ja), _) = two_tenants(&e.owner).await;
    // (case, SET list on top of the defaults: state 'asked', asked_by 'staff', asked_at now()), expected SQLSTATE
    let cases: &[(&str, &str, Option<&str>)] = &[
        // each state with exactly its timestamps
        ("CS-1", "state = 'asked'", None),
        (
            "CS-2",
            "state = 'awaiting_confirmation', asked_at = NULL",
            None,
        ),
        ("CS-3", "state = 'dismissed', dismissed_at = now()", None),
        (
            "CS-4",
            "state = 'answered', rating = 4, answered_at = now()",
            None,
        ),
        // a state with the wrong timestamps
        ("CS-5", "state = 'awaiting_confirmation'", Some("23514")),
        ("CS-6", "state = 'asked', asked_at = NULL", Some("23514")),
        ("CS-7", "state = 'dismissed'", Some("23514")),
        (
            "CS-8",
            "state = 'asked', dismissed_at = now()",
            Some("23514"),
        ),
        ("CS-9", "state = 'answered', rating = 4", Some("23514")),
        (
            "CS-10",
            "state = 'asked', rating = 4, answered_at = now()",
            Some("23514"),
        ),
        ("CS-11", "state = 'expired'", Some("23514")),
        // asked_by
        ("CS-12", "asked_by = 'staff', asked_by_sub = 'u-1'", None),
        ("CS-13", "asked_by = 'agent'", None),
        (
            "CS-14",
            "asked_by = 'agent', asked_by_sub = 'u-1'",
            Some("23514"),
        ),
        ("CS-15", "asked_by = 'customer'", Some("23514")),
        // comment: only on an answered row, asked for, 1..=1000 characters
        (
            "CS-16",
            "state = 'answered', rating = 1, answered_at = now(), comment_asked_at = now(), \
             comment = 'slow', comment_at = now()",
            None,
        ),
        (
            "CS-17",
            "state = 'answered', rating = 1, answered_at = now(), comment_asked_at = now(), \
             comment = repeat('x', 1000), comment_at = now()",
            None,
        ),
        (
            "CS-18",
            "state = 'answered', rating = 1, answered_at = now(), comment_asked_at = now(), \
             comment = repeat('x', 1001), comment_at = now()",
            Some("23514"),
        ),
        (
            "CS-19",
            "state = 'answered', rating = 1, answered_at = now(), comment_asked_at = now(), \
             comment = '', comment_at = now()",
            Some("23514"),
        ),
        (
            "CS-20",
            "state = 'answered', rating = 1, answered_at = now(), comment_asked_at = now(), \
             comment = 'x'",
            Some("23514"),
        ),
        (
            "CS-21",
            "state = 'answered', rating = 1, answered_at = now(), comment = 'x', comment_at = now()",
            Some("23514"),
        ),
        (
            "CS-22",
            "state = 'asked', comment_asked_at = now()",
            Some("23514"),
        ),
    ];
    for (label, set, want) in cases {
        // `two_tenants` already holds CS-1: the table's `CS-n` labels are offset into a free range
        let n: u32 = label.trim_start_matches("CS-").parse().unwrap();
        let case_id = format!("CS-{}", n + 100);
        let case = case_id.as_str();
        insert_case(&e.owner, a, ja, case, "conv-1").await;
        sqlx::query(
            "INSERT INTO case_csat (tenant_id, job_id, case_id, conversation_id, channel, env) \
             VALUES ($1, $2, $3, 'conv-1', 'web_chat', 'prod')",
        )
        .bind(a)
        .bind(ja)
        .bind(case)
        .execute(&e.owner)
        .await
        .unwrap();
        let upd = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE case_csat SET {set} WHERE case_id = $1"
        )))
        .bind(case)
        .execute(&e.owner)
        .await;
        match (want, upd) {
            (None, Ok(_)) => {}
            (Some(code), Err(err)) => assert_eq!(
                sqlstate(&err).as_deref(),
                Some(*code),
                "{case} {set}: {err}"
            ),
            (None, Err(err)) => panic!("{case} {set} should be allowed: {err}"),
            (Some(code), Ok(_)) => panic!("{case} {set} should fail with {code}"),
        }
    }
}

#[tokio::test]
async fn connector_bindings_lookup_reads_every_tenant() {
    let e = common::env().await;
    let ((a, ja), (b, jb)) = two_tenants(&e.owner).await;
    for (t, j, ext, enabled) in [
        (a, ja, "wk_alpha", true),
        (b, jb, "wk_beta", true),
        (b, jb, "wk_beta_off", false),
    ] {
        sqlx::query(
            "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, created_by, enabled, verified_at) \
             VALUES ($1, $2, 'web_chat', 'customer', $3, 'test', $4, now())",
        )
        .bind(t)
        .bind(j)
        .bind(ext)
        .bind(enabled)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    // the channel account is unique platform-wide
    let err = sqlx::query(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, created_by, verified_at) \
         VALUES ($1, $2, 'web_chat', 'customer', 'wk_alpha', 'test', now())",
    )
    .bind(b)
    .bind(jb)
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");

    let mut tx = scoped(&e.state.db, a, None).await;
    let own: i64 = sqlx::query_scalar("SELECT count(*) FROM connector_bindings")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        own, 1,
        "RLS: the table itself shows only the tenant's binding"
    );
    let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
        "SELECT tenant_id, job_key, external_id FROM agent_connector_bindings() ORDER BY external_id",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            (a, "care".to_string(), "wk_alpha".to_string()),
            (b, "care".to_string(), "wk_beta".to_string()),
        ],
        "enabled bindings of both tenants"
    );
    // the app role may delete its own binding
    let n = sqlx::query("DELETE FROM connector_bindings WHERE external_id = 'wk_alpha'")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn worker_refs_name_an_item_customer_refs_do_not() {
    let e = common::env().await;
    let ((a, ja), _) = two_tenants(&e.owner).await;
    let mut tx = scoped(&e.state.db, a, Some(ja)).await;
    let insert = "INSERT INTO channel_conversation_refs (conversation_id, side, item_id, channel, external_id) \
                  VALUES ('conv-1', $1, $2, 'teams', $3)";
    sqlx::query(insert)
        .bind("worker")
        .bind(Some("HO-1"))
        .bind("19:thread")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(insert)
        .bind("customer")
        .bind(None::<String>)
        .bind("a:conv")
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = sqlx::query(insert)
        .bind("customer")
        .bind(Some("HO-1"))
        .bind("b:conv")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
}

// ---- 0183: the known customer of a web chat -------------------------------------------------------------------

async fn web_chat_binding(db: &PgPool, tenant: Uuid, job: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, created_by, verified_at) \
         VALUES ($1, $2, 'web_chat', 'customer', $3, 'test', now()) RETURNING id",
    )
    .bind(tenant)
    .bind(job)
    .bind(key)
    .fetch_one(db)
    .await
    .unwrap()
}

/// One session row; `customer` = (ref, email, identified_at).
async fn insert_session(
    db: &PgPool,
    (tenant, job, binding): (Uuid, Uuid, Uuid),
    customer: (Option<&str>, Option<&str>, bool),
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO web_chat_session (tenant_id, job_id, binding_id, token_hash, origin, ip_hash, expires_at, \
                                       customer_ref, customer_email, identified_at) \
         VALUES ($1, $2, $3, sha256(gen_random_uuid()::text::bytea), 'https://shop.example', \
                 sha256('ip'::bytea), now() + interval '1 day', $4, $5, CASE WHEN $6 THEN now() END)",
    )
    .bind(tenant)
    .bind(job)
    .bind(binding)
    .bind(customer.0)
    .bind(customer.1)
    .bind(customer.2)
    .execute(db)
    .await
    .map(|_| ())
}

#[tokio::test]
async fn web_chat_session_known_customer_checks() {
    let e = common::env().await;
    let ((a, ja), _) = two_tenants(&e.owner).await;
    let k = (a, ja, web_chat_binding(&e.owner, a, ja, "wk_known").await);
    // anonymous, identified, identified with an email: fine
    insert_session(&e.owner, k, (None, None, false))
        .await
        .unwrap();
    insert_session(&e.owner, k, (Some("cust-1001"), None, true))
        .await
        .unwrap();
    insert_session(
        &e.owner,
        k,
        (Some("cust-1001"), Some("a@shop.example"), true),
    )
    .await
    .unwrap();
    let long_ref = "r".repeat(201);
    let long_email = "e".repeat(321);
    let bad: [(Option<&str>, Option<&str>, bool); 7] = [
        (Some("cust-1"), None, false), // a ref without its identification time
        (None, None, true),            // an identification time without a ref
        (None, Some("a@shop.example"), false), // an email without a customer
        (None, Some("a@shop.example"), true), // ... even with a time
        (Some("  "), None, true),      // blank ref
        (Some(&long_ref), None, true), // ref over 200
        (Some("cust-1"), Some(&long_email), true), // email over 320
    ];
    for (i, case) in bad.iter().enumerate() {
        let err = insert_session(&e.owner, k, *case).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "case {i}: {err}");
    }
    let err = insert_session(&e.owner, k, (Some("cust-1"), Some(" "), true))
        .await
        .unwrap_err();
    assert_eq!(
        sqlstate(&err).as_deref(),
        Some("23514"),
        "blank email: {err}"
    );
}

#[tokio::test]
async fn blocked_reason_accepts_identity_rotated_and_nothing_unknown() {
    let e = common::env().await;
    let ((a, ja), _) = two_tenants(&e.owner).await;
    let k = (a, ja, web_chat_binding(&e.owner, a, ja, "wk_blk").await);
    insert_session(&e.owner, k, (Some("cust-1"), None, true))
        .await
        .unwrap();
    for reason in ["rotated", "rate_limited", "repeat_text", "identity_rotated"] {
        sqlx::query("UPDATE web_chat_session SET blocked_at = now(), blocked_reason = $1")
            .bind(reason)
            .execute(&e.owner)
            .await
            .unwrap_or_else(|err| panic!("{reason}: {err}"));
    }
    let err = sqlx::query(
        "UPDATE web_chat_session SET blocked_at = now(), blocked_reason = 'identity_gone'",
    )
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
}

#[tokio::test]
async fn jti_claim_is_single_use_per_binding_and_tenant_bound() {
    let e = common::env().await;
    let ((a, ja), (b, jb)) = two_tenants(&e.owner).await;
    let ba = web_chat_binding(&e.owner, a, ja, "wk_a").await;
    let bb = web_chat_binding(&e.owner, b, jb, "wk_b").await;
    let claim = "SELECT agent_webchat_jti_claim($1, $2, now() + interval '5 minutes')";
    let jti = "jti-0123456789abcdef";

    // the first claim wins, a replay and a racing duplicate lose, another jti and another binding are independent
    let mut tx = scoped(&e.state.db, a, Some(ja)).await;
    for (binding, id, want) in [
        (ba, jti, true),
        (ba, jti, false),
        (ba, "jti-fedcba9876543210", true),
    ] {
        let got: bool = sqlx::query_scalar(claim)
            .bind(binding)
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(got, want, "{id}");
    }
    tx.commit().await.unwrap();
    let mut tx = scoped(&e.state.db, b, Some(jb)).await;
    let got: bool = sqlx::query_scalar(claim)
        .bind(bb)
        .bind(jti)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert!(
        got,
        "the same jti on another binding is a different assertion"
    );
    tx.commit().await.unwrap();

    // tenant B can neither burn nor probe tenant A's binding; an unscoped call and bad arguments raise
    let mut tx = scoped(&e.state.db, b, Some(jb)).await;
    let err = sqlx::query_scalar::<_, bool>(claim)
        .bind(ba)
        .bind("jti-0000000000000000")
        .fetch_one(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    let err = sqlx::query_scalar::<_, bool>(claim)
        .bind(ba)
        .bind(jti)
        .fetch_one(&e.state.db)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "unscoped: {err}");
    for short in ["short", &"j".repeat(65)] {
        let mut tx = scoped(&e.state.db, a, Some(ja)).await;
        let err = sqlx::query_scalar::<_, bool>(claim)
            .bind(ba)
            .bind(short)
            .fetch_one(&mut *tx)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("22023"), "{err}");
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM webchat_assertion_jti")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 3, "only the three accepted claims are stored");
}

#[tokio::test]
async fn jti_table_is_definer_only_and_function_is_not_public() {
    let e = common::env().await;
    let ((a, ja), _) = two_tenants(&e.owner).await;
    let ba = web_chat_binding(&e.owner, a, ja, "wk_a").await;
    // agent_app has no grant on the table, in any form
    let mut tx = scoped(&e.state.db, a, Some(ja)).await;
    for sql in [
        "SELECT count(*) FROM webchat_assertion_jti",
        "DELETE FROM webchat_assertion_jti",
    ] {
        let err = sqlx::query(sql).execute(&mut *tx).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{sql}: {err}");
        tx = scoped(&e.state.db, a, Some(ja)).await;
    }
    let err = sqlx::query(
        "INSERT INTO webchat_assertion_jti (binding_id, jti, expires_at) VALUES ($1, 'jti-0123456789abcdef', now())",
    )
    .bind(ba)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    // the runtime role holds neither the table nor EXECUTE on the function
    let rt = common::runtime_pool(&e.owner).await;
    let err = sqlx::query("SELECT count(*) FROM webchat_assertion_jti")
        .execute(&rt)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    let err = sqlx::query("SELECT agent_webchat_jti_claim($1, 'jti-0123456789abcdef', now())")
        .bind(ba)
        .execute(&rt)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    // only agent_app may execute it, the function is a definer with a pinned search_path
    let acl: Vec<(String, bool, Vec<String>)> = sqlx::query_as(
        "SELECT p.proname::text, p.prosecdef, COALESCE(p.proconfig, '{}') FROM pg_proc p \
          WHERE p.proname = 'agent_webchat_jti_claim'",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(acl.len(), 1);
    assert!(acl[0].1, "SECURITY DEFINER");
    assert!(
        acl[0].2.iter().any(|c| c.starts_with("search_path=")),
        "search_path is pinned: {:?}",
        acl[0].2
    );
    let who: Vec<String> = sqlx::query_scalar(
        "SELECT r.rolname::text FROM pg_roles r \
          WHERE r.rolname IN ('agent_app', 'agent_platform', 'agent_runtime') \
            AND has_function_privilege(r.rolname, 'agent_webchat_jti_claim(uuid, text, timestamptz)', 'EXECUTE') \
          ORDER BY 1",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(who, ["agent_app"]);
    let public: bool = sqlx::query_scalar(
        "SELECT has_function_privilege('public', 'agent_webchat_jti_claim(uuid, text, timestamptz)', 'EXECUTE')",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(!public, "REVOKE FROM PUBLIC");
}

#[tokio::test]
async fn jti_expiry_is_clamped_and_swept_five_minutes_after_it() {
    let e = common::env().await;
    let ((a, ja), _) = two_tenants(&e.owner).await;
    let ba = web_chat_binding(&e.owner, a, ja, "wk_a").await;
    // a hostile exp (past, far future) is clamped into [now, now + 1 h]
    let mut tx = scoped(&e.state.db, a, Some(ja)).await;
    for (jti, exp) in [
        ("jti-past-000000000", "now() - interval '1 day'"),
        ("jti-future-00000000", "now() + interval '30 days'"),
    ] {
        sqlx::query_scalar::<_, bool>(sqlx::AssertSqlSafe(format!(
            "SELECT agent_webchat_jti_claim($1, $2, {exp})"
        )))
        .bind(ba)
        .bind(jti)
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let ok: Vec<(String, bool)> = sqlx::query_as(
        "SELECT jti, expires_at >= claimed_at AND expires_at <= claimed_at + interval '1 hour 1 minute' \
           FROM webchat_assertion_jti ORDER BY jti",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(ok.len(), 2);
    assert!(ok.iter().all(|(_, in_range)| *in_range), "{ok:?}");

    // the sweep (as the app role, like the tick) keeps rows lapsed under 5 minutes and drops older ones
    for (jti, ago) in [
        ("jti-past-000000000", "6 minutes"),
        ("jti-future-00000000", "2 minutes"),
    ] {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE webchat_assertion_jti SET expires_at = now() - interval '{ago}' WHERE jti = $1"
        )))
        .bind(jti)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let mut tx = scoped(&e.state.db, a, None).await;
    sqlx::query_scalar::<_, i32>("SELECT agent_channel_counters_sweep()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let left: Vec<String> = sqlx::query_scalar("SELECT jti FROM webchat_assertion_jti")
        .fetch_all(&e.owner)
        .await
        .unwrap();
    assert_eq!(left, ["jti-future-00000000"]);
    // deleting the binding removes its ids
    sqlx::query("DELETE FROM connector_bindings WHERE id = $1")
        .bind(ba)
        .execute(&e.owner)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM webchat_assertion_jti")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}
