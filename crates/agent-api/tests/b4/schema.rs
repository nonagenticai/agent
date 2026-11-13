//! B4 schema (migrations 0062, …): RLS (`tenant_isolation` + restrictive `job_scope`, copied from 0060's
//! `agent_case`), the tenant default and the exact `agent_app` grants of every B4 table, tenant isolation and the
//! CHECKs. Each B4 migration task appends its tables to [`B4_TABLES`] and its cases below. Real Postgres 18 (one
//! container for the `b4` binary, a fresh database per test).
use crate::common;

use crate::common::classes::{Class, class_of};
use crate::common::conv;
use crate::common::{insert_tenant, sqlstate};
use agent_api::conversations::store;
use agent_api::design::CaseDesign;
use agent_api::tenancy::begin_scoped;
use axum::http::StatusCode;
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Every B4 table and the table privileges `agent_app` holds on it (plan Data model, §0 R2).
const B4_TABLES: &[(&str, &[&str])] = &[
    // 0062 (T2): the runner writes calls as agent_runtime (0065); agent_app inserts the manual-handover call (T12)
    // and an operator's check (T13). Neither is ever updated or deleted by agent_app.
    ("operation_call", &["SELECT", "INSERT"]),
    ("operation_call_check", &["SELECT", "INSERT"]),
    // 0064 (T3): the usage ledger is append-only; the drainer updates only the outbox's delivery columns
    // (column grant, so no table-level UPDATE).
    ("usage_event", &["SELECT", "INSERT"]),
    ("metering_outbox", &["SELECT", "INSERT"]),
    // 0069 (T16): the working row of ledger kind handover_policy, written only through design::track / its target
    ("handover_policy", &["SELECT", "INSERT", "UPDATE", "DELETE"]),
];

/// One row of `pg_policies` for a table.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct Policy {
    name: String,
    permissive: String,
    cmd: String,
    qual: Option<String>,
    with_check: Option<String>,
}

async fn policies_of(owner: &PgPool, table: &str) -> Vec<Policy> {
    sqlx::query_as(
        "SELECT policyname::text AS name, permissive, cmd, qual, with_check FROM pg_policies \
          WHERE schemaname = 'public' AND tablename::text = $1 ORDER BY 1",
    )
    .bind(table.to_owned())
    .fetch_all(owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn every_b4_table_has_rls_policies_default_and_grants() {
    let e = common::env().await;
    // B12's `agent_case` (0060) is the reference: tenant_isolation + RESTRICTIVE job_scope.
    let want_policies = policies_of(&e.owner, "agent_case").await;
    assert_eq!(
        want_policies
            .iter()
            .map(|p| (p.name.as_str(), p.permissive.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("job_scope", "RESTRICTIVE"),
            ("tenant_isolation", "PERMISSIVE")
        ],
        "reference policies of agent_case"
    );
    let ref_default: String = sqlx::query_scalar(
        "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
           JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
          WHERE d.adrelid = 'public.jobs'::regclass AND a.attname::text = 'tenant_id'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let mut problems = Vec::new();
    for &(t, privs) in B4_TABLES {
        if class_of(t) != Some(Class::Tenant) {
            problems.push(format!("{t}: not Class::Tenant in tests/common/classes.rs"));
        }
        let (rls, force): (bool, bool) = sqlx::query_as(
            "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE oid = ('public.' || $1)::regclass",
        )
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        if !rls || force {
            problems.push(format!(
                "{t}: rls={rls} force={force}, want RLS on, not forced"
            ));
        }
        let policies = policies_of(&e.owner, t).await;
        if policies != want_policies {
            problems.push(format!(
                "{t}: policies {policies:?}, want {want_policies:?}"
            ));
        }
        let tenant_col: Option<(bool, Option<String>)> = sqlx::query_as(
            "SELECT a.attnotnull, pg_get_expr(d.adbin, d.adrelid) \
               FROM pg_attribute a \
               LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
              WHERE a.attrelid = ('public.' || $1)::regclass AND a.attname::text = 'tenant_id' \
                AND NOT a.attisdropped",
        )
        .bind(t)
        .fetch_optional(&e.owner)
        .await
        .unwrap();
        if tenant_col != Some((true, Some(ref_default.clone()))) {
            problems.push(format!(
                "{t}: tenant_id {tenant_col:?}, want NOT NULL DEFAULT {ref_default}"
            ));
        }
        for p in ["SELECT", "INSERT", "UPDATE", "DELETE", "TRUNCATE"] {
            for role in ["agent_app", "agent_platform"] {
                let has: bool = sqlx::query_scalar(
                    "SELECT has_table_privilege($1::text::name, 'public.' || $2, $3)",
                )
                .bind(role)
                .bind(t)
                .bind(p)
                .fetch_one(&e.owner)
                .await
                .unwrap();
                let want = role == "agent_app" && privs.contains(&p);
                if has != want {
                    problems.push(format!("{t}: {role} {p} is {has}, want {want}"));
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

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

/// A tenant (unique name) with one job; `(tenant, job)`.
async fn tenant_with_job(owner: &PgPool, label: &str) -> (Uuid, Uuid) {
    let t = Uuid::new_v4();
    insert_tenant(owner, t, &format!("B4 {label} {t}")).await;
    let j = insert_job(owner, t, "care").await;
    (t, j)
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

fn new_call_id() -> String {
    format!("opc_{}", Uuid::now_v7())
}

/// A finished `ok` prod call of `order.read` with `key` (None = a read without a key), as the owner.
async fn insert_call(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    call_id: &str,
    key: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO operation_call (tenant_id, job_id, call_id, operation, route, env, state, finished_at, \
                                     initiator, idempotency_key) \
         VALUES ($1, $2, $3, 'order.read', 'instance', 'prod', 'ok', now(), 'agent@orchestrator', $4)",
    )
    .bind(tenant)
    .bind(job)
    .bind(call_id.to_owned())
    .bind(key.map(str::to_owned))
    .execute(db)
    .await
    .map(|_| ())
}

#[tokio::test]
async fn idempotency_key_unique_per_job() {
    let e = common::env().await;
    let (t, j1) = tenant_with_job(&e.owner, "idem").await;
    let j2 = insert_job(&e.owner, t, "sales").await;
    insert_call(&e.owner, t, j1, &new_call_id(), Some("step-1"))
        .await
        .unwrap();
    let err = insert_call(&e.owner, t, j1, &new_call_id(), Some("step-1"))
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
    // the same key in another job of the tenant is another call
    insert_call(&e.owner, t, j2, &new_call_id(), Some("step-1"))
        .await
        .expect("same key, other job");
    // reads carry no key: any number of them
    for _ in 0..2 {
        insert_call(&e.owner, t, j1, &new_call_id(), None)
            .await
            .expect("keyless read");
    }
    // an empty key is not "no key"
    let err = insert_call(&e.owner, t, j1, &new_call_id(), Some(""))
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
}

#[tokio::test]
async fn rls_hides_other_tenant_calls() {
    let e = common::env().await;
    let (a, ja) = tenant_with_job(&e.owner, "rls-a").await;
    let (b, jb) = tenant_with_job(&e.owner, "rls-b").await;
    let ja2 = insert_job(&e.owner, a, "sales").await;
    let (ca, ca2, cb) = (new_call_id(), new_call_id(), new_call_id());
    insert_call(&e.owner, a, ja, &ca, None).await.unwrap();
    insert_call(&e.owner, a, ja2, &ca2, None).await.unwrap();
    insert_call(&e.owner, b, jb, &cb, None).await.unwrap();
    for (t, j, c) in [(a, ja, &ca), (b, jb, &cb)] {
        sqlx::query(
            "INSERT INTO operation_call_check (tenant_id, job_id, call_id, verdict, by_sub) \
             VALUES ($1, $2, $3, 'landed', 'op-1')",
        )
        .bind(t)
        .bind(j)
        .bind(c.clone())
        .execute(&e.owner)
        .await
        .unwrap();
    }

    // tenant A, no job scope: A's two calls, A's one check, nothing of B
    let mut tx = scoped(&e.state.db, a, None).await;
    let mut seen: Vec<String> = sqlx::query_scalar("SELECT call_id FROM operation_call")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    seen.sort();
    let mut want = vec![ca.clone(), ca2];
    want.sort();
    assert_eq!(seen, want);
    let checks: Vec<String> = sqlx::query_scalar("SELECT call_id FROM operation_call_check")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(checks, vec![ca.clone()]);
    // and cannot write into B
    let err = sqlx::query(
        "INSERT INTO operation_call (tenant_id, job_id, call_id, operation, route, env, initiator) \
         VALUES ($1, $2, $3, 'order.read', 'builtin', 'prod', 'agent@orchestrator')",
    )
    .bind(b)
    .bind(jb)
    .bind(new_call_id())
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    drop(tx);

    // tenant A scoped to job ja: only that job's call; the defaults fill tenant and job
    let mut tx = scoped(&e.state.db, a, Some(ja)).await;
    let seen: Vec<String> = sqlx::query_scalar("SELECT call_id FROM operation_call")
        .fetch_all(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, vec![ca]);
    let own = new_call_id();
    sqlx::query(
        "INSERT INTO operation_call (call_id, operation, route, env, initiator) \
         VALUES ($1, 'case.create{handover}', 'fallback', 'prod', 'sub-1')",
    )
    .bind(own.clone())
    .execute(&mut *tx)
    .await
    .expect("agent_app inserts a pending call in its own job");
    // agent_app never rewrites a call (the runner does, as agent_runtime)
    let err = sqlx::query(
        "UPDATE operation_call SET state = 'error', finished_at = now() WHERE call_id = $1",
    )
    .bind(own)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
}

#[tokio::test]
async fn operation_call_check_pk_answers_already_checked() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "check").await;
    let c = new_call_id();
    sqlx::query(
        "INSERT INTO operation_call (tenant_id, job_id, call_id, operation, route, env, state, finished_at, \
                                     initiator, idempotency_key) \
         VALUES ($1, $2, $3, 'refund.create', 'instance', 'prod', 'unknown_outcome', now(), 'agent@runner', 'k-1')",
    )
    .bind(t)
    .bind(j)
    .bind(c.clone())
    .execute(&e.owner)
    .await
    .unwrap();
    let check = |verdict: &'static str| {
        let db = e.state.db.clone();
        let c = c.clone();
        async move {
            let mut tx = scoped(&db, t, Some(j)).await;
            sqlx::query(
                "INSERT INTO operation_call_check (call_id, verdict, note, by_sub) VALUES ($1, $2, 'seen upstream', 'op-1')",
            )
            .bind(c)
            .bind(verdict)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<(), sqlx::Error>(())
        }
    };
    check("landed").await.expect("first check");
    let err = check("not_landed").await.unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
    // the stored verdict is the first one
    let v: String = sqlx::query_scalar(
        "SELECT verdict FROM operation_call_check WHERE tenant_id = $1 AND call_id = $2",
    )
    .bind(t)
    .bind(c.clone())
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(v, "landed");
    // a check needs a known call and a known verdict
    let err = sqlx::query(
        "INSERT INTO operation_call_check (tenant_id, job_id, call_id, verdict, by_sub) VALUES ($1, $2, $3, 'landed', 'op-1')",
    )
    .bind(t)
    .bind(j)
    .bind(new_call_id())
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23503"), "{err}");
    let c2 = new_call_id();
    insert_call(&e.owner, t, j, &c2, None).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO operation_call_check (tenant_id, job_id, call_id, verdict, by_sub) VALUES ($1, $2, $3, 'maybe', 'op-1')",
    )
    .bind(t)
    .bind(j)
    .bind(c2)
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
}

#[tokio::test]
async fn operation_call_checks_reject_bad_rows() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "checks").await;
    // (route, env, state, finished, release_id, draft_rev, case_id) → expected SQLSTATE (None = accepted)
    type Row = (
        &'static str,
        &'static str,
        &'static str,
        bool,
        Option<&'static str>,
        Option<i64>,
        Option<&'static str>,
    );
    let cases: &[(Row, Option<&str>)] = &[
        (
            ("instance", "prod", "ok", true, Some("rel-1"), None, None),
            None,
        ),
        (("instance", "prod", "ok", true, None, None, None), None), // a never-released job's baseline
        (
            ("builtin", "test", "pending", false, None, Some(3), None),
            None,
        ),
        (
            ("fallback", "test", "denied", true, None, Some(0), None),
            None,
        ),
        (("deep_link", "prod", "error", true, None, None, None), None),
        (
            (
                "not_offered",
                "prod",
                "unknown_outcome",
                true,
                None,
                None,
                None,
            ),
            None,
        ),
        (("mcp", "prod", "ok", true, None, None, None), Some("23514")), // unknown route
        (
            ("instance", "prod", "done", true, None, None, None),
            Some("23514"),
        ), // unknown state
        (
            ("instance", "staging", "ok", true, None, None, None),
            Some("23514"),
        ), // unknown env
        (
            ("instance", "prod", "ok", false, None, None, None),
            Some("23514"),
        ), // finished without finished_at
        (
            ("instance", "prod", "pending", true, None, None, None),
            Some("23514"),
        ), // pending with finished_at
        (
            ("instance", "test", "ok", true, None, None, None),
            Some("23514"),
        ), // test without draft_rev
        (
            ("instance", "test", "ok", true, Some("rel-1"), Some(2), None),
            Some("23514"),
        ), // both
        (
            ("instance", "prod", "ok", true, None, Some(2), None),
            Some("23514"),
        ), // prod with draft_rev
        (
            ("instance", "prod", "ok", true, None, None, Some("CS-404")),
            Some("23503"),
        ), // unknown case
    ];
    let mut problems = Vec::new();
    for (i, &((route, env, state, finished, release, rev, case), want)) in cases.iter().enumerate()
    {
        let got = sqlx::query(
            "INSERT INTO operation_call (tenant_id, job_id, call_id, operation, route, env, state, finished_at, \
                                         release_id, draft_rev, case_id, initiator) \
             VALUES ($1, $2, $3, 'order.read', $4, $5, $6, CASE WHEN $7 THEN now() END, $8, $9, $10, 'agent@runner')",
        )
        .bind(t)
        .bind(j)
        .bind(new_call_id())
        .bind(route)
        .bind(env)
        .bind(state)
        .bind(finished)
        .bind(release)
        .bind(rev)
        .bind(case)
        .execute(&e.owner)
        .await
        .err()
        .and_then(|err| sqlstate(&err));
        if got.as_deref() != want {
            problems.push(format!(
                "case {i} ({route}, {env}, {state}): got {got:?}, want {want:?}"
            ));
        }
    }
    // the call id is `opc_<uuid>`
    let err = sqlx::query(
        "INSERT INTO operation_call (tenant_id, job_id, call_id, operation, route, env, initiator) \
         VALUES ($1, $2, 'call-1', 'order.read', 'builtin', 'prod', 'agent@runner')",
    )
    .bind(t)
    .bind(j)
    .execute(&e.owner)
    .await
    .unwrap_err();
    if sqlstate(&err).as_deref() != Some("23514") {
        problems.push(format!("bad call id: {err}"));
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

// ---- 0068 (T2b): the design a case runs on — release FK + env CHECK (Q-B4-1), `conversation_message.grounding` gone

/// The tables 0068 stamps: each carries `release_id` / `draft_rev`, an FK to `job_release` and the env CHECK.
const STAMPED: [&str; 3] = ["agent_case", "agent_handover_items", "case_csat"];

/// `(env, release_id, draft_rev)` of a stamped row.
type Stamp<'a> = (&'static str, Option<&'a str>, Option<i64>);

/// A released (`current`) release `id` of `job`, as the owner (shape of `tests/b2_schema.rs`).
async fn insert_release(db: &PgPool, tenant: Uuid, job: Uuid, id: &str) {
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, released_by, released_at,
               snapshot_kinds)
           VALUES ($1, $2, $3, $4, $4, '{"sections": []}', '{}', '[]', '{}', '{}', '[]', 'current', 'u-rm', now(),
               'u-rm', now(), ARRAY['languages', 'setting'])"#,
    )
    .bind(tenant)
    .bind(job)
    .bind(id.to_owned())
    .bind(format!("sha256:{}", "a".repeat(64)))
    .execute(db)
    .await
    .unwrap();
}

/// Make `release` the live release of `job` (`jobs.current_release_id`), as the owner.
async fn set_live(db: &PgPool, job: Uuid, release: &str) {
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(job)
        .bind(release.to_owned())
        .execute(db)
        .await
        .unwrap();
}

/// One row of `table` stamped `s` for case `CS-<n>` (conversation `conv-<n>`), as the owner. An item or a CSAT
/// request first gets its case, validly stamped in the same env.
async fn insert_stamped(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    n: u32,
    table: &str,
    s: Stamp<'_>,
) -> Result<(), sqlx::Error> {
    const CASE: &str = "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, \
                                                release_id, draft_rev) \
                        VALUES ($1, $2, $3, $4, 'web_chat', $5, $6, $7)";
    let sql: &'static str = match table {
        "agent_case" => CASE,
        "agent_handover_items" => {
            "INSERT INTO agent_handover_items (tenant_id, job_id, id, case_id, conversation_id, env, \
                                               reason, release_id, draft_rev) \
             VALUES ($1, $2, 'HO-' || substr($3, 4), $3, $4, $5, 'customer_asked', $6, $7)"
        }
        "case_csat" => {
            "INSERT INTO case_csat (tenant_id, job_id, case_id, conversation_id, channel, env, release_id, draft_rev) \
             VALUES ($1, $2, $3, $4, 'web_chat', $5, $6, $7)"
        }
        other => panic!("not a stamped table: {other}"),
    };
    let (env, release, rev) = s;
    if table != "agent_case" {
        let parent_rev = (env == "test").then_some(0_i64);
        sqlx::query(CASE)
            .bind(tenant)
            .bind(job)
            .bind(format!("CS-{n}"))
            .bind(format!("conv-{n}"))
            .bind(env)
            .bind(None::<String>)
            .bind(parent_rev)
            .execute(db)
            .await
            .expect("the parent case");
    }
    sqlx::query(sql)
        .bind(tenant)
        .bind(job)
        .bind(format!("CS-{n}"))
        .bind(format!("conv-{n}"))
        .bind(env)
        .bind(release.map(str::to_owned))
        .bind(rev)
        .execute(db)
        .await
        .map(|_| ())
}

/// `(SQLSTATE, constraint)` of a refused write; `None` when it was stored.
fn refusal(r: Result<(), sqlx::Error>) -> Option<(String, String)> {
    let e = r.err()?;
    let d = e
        .as_database_error()
        .unwrap_or_else(|| panic!("not a database error: {e}"));
    Some((
        d.code().map(|c| c.into_owned()).unwrap_or_default(),
        d.constraint().unwrap_or_default().to_owned(),
    ))
}

/// Insert `stamp` into every stamped table (case numbers from `*n` up) and collect each outcome that differs from
/// `want(table)`.
async fn expect_each(
    db: &PgPool,
    (tenant, job): (Uuid, Uuid),
    n: &mut u32,
    stamp: Stamp<'_>,
    want: impl Fn(&str) -> Option<(String, String)>,
    problems: &mut Vec<String>,
) {
    for table in STAMPED {
        *n += 1;
        let got = refusal(insert_stamped(db, tenant, job, *n, table, stamp).await);
        let want = want(table);
        if got != want {
            problems.push(format!("{table} {stamp:?}: got {got:?}, want {want:?}"));
        }
    }
}

fn check_violation(table: &str) -> Option<(String, String)> {
    Some(("23514".into(), format!("{table}_env_design_check")))
}

/// A `store::Conv` of `env` on web chat (`agent_case` has no FK to `conversation`).
fn conv_of(id: &str, env: &str) -> store::Conv {
    store::Conv {
        id: id.to_owned(),
        channel: "web_chat".into(),
        env: env.into(),
        customer_id: Some("cust-1".into()),
        customer_name: None,
        initiator_sub: None,
        identity_source: None,
        customer_email: None,
        identity_strength: None,
    }
}

/// `(release_id, draft_rev)` of case `case_id` of `tenant`.
async fn case_stamp(db: &PgPool, tenant: Uuid, case_id: &str) -> (Option<String>, Option<i64>) {
    sqlx::query_as(
        "SELECT release_id, draft_rev FROM agent_case WHERE tenant_id = $1 AND case_id = $2",
    )
    .bind(tenant)
    .bind(case_id.to_owned())
    .fetch_one(db)
    .await
    .unwrap()
}

#[tokio::test]
async fn prod_case_carries_release_or_baseline_null() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "prod-stamp").await;
    let rel = "cfg-2026-10-21.1";
    insert_release(&e.owner, t, j, rel).await;
    let mut n = 0;
    let mut problems = Vec::new();
    // a never-released job's prod row runs its baseline (release NULL, Q-B4-1); a released one names its release
    for stamp in [("prod", None, None), ("prod", Some(rel), None)] {
        expect_each(&e.owner, (t, j), &mut n, stamp, |_| None, &mut problems).await;
    }
    assert!(problems.is_empty(), "{problems:#?}");

    // B12's writer, as agent_app in the job's transaction: the design is read in that transaction
    let (t2, j2) = tenant_with_job(&e.owner, "prod-writer").await;
    let mut tx = scoped(&e.state.db, t2, Some(j2)).await;
    let c1 = conv_of("conv-a", "prod");
    let d = store::design_of(&mut tx, j2, "care", &c1).await.unwrap();
    assert_eq!(d, CaseDesign::Live { release_id: None }, "never released");
    let case1 = store::create_case(&mut tx, j2, &c1, "Where is my order?", &d)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(case_stamp(&e.owner, t2, &case1).await, (None, None));

    insert_release(&e.owner, t2, j2, rel).await;
    set_live(&e.owner, j2, rel).await;
    let mut tx = scoped(&e.state.db, t2, Some(j2)).await;
    let c2 = conv_of("conv-b", "prod");
    let d = store::design_of(&mut tx, j2, "care", &c2).await.unwrap();
    assert_eq!(
        d,
        CaseDesign::Live {
            release_id: Some(rel.into())
        }
    );
    let case2 = store::case_for(&mut tx, j2, &c2, "Refund please", &d)
        .await
        .unwrap();
    // the open case of a conversation keeps the design it was created with (first plan wins)
    let again = store::case_for(&mut tx, j2, &c1, "still there?", &d)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(again, case1);
    assert_eq!(
        case_stamp(&e.owner, t2, &case2).await,
        (Some(rel.into()), None)
    );
    assert_eq!(case_stamp(&e.owner, t2, &case1).await, (None, None));
}

#[tokio::test]
async fn test_case_needs_draft_rev() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "test-stamp").await;
    let mut n = 0;
    let mut problems = Vec::new();
    expect_each(
        &e.owner,
        (t, j),
        &mut n,
        ("test", None, None),
        check_violation,
        &mut problems,
    )
    .await;
    expect_each(
        &e.owner,
        (t, j),
        &mut n,
        ("test", None, Some(0)),
        |_| None,
        &mut problems,
    )
    .await;
    assert!(problems.is_empty(), "{problems:#?}");

    // the writer stamps the job's current draft_rev
    sqlx::query("UPDATE jobs SET draft_rev = 7 WHERE id = $1")
        .bind(j)
        .execute(&e.owner)
        .await
        .unwrap();
    let mut tx = scoped(&e.state.db, t, Some(j)).await;
    let c = conv_of("conv-t", "test");
    let d = store::design_of(&mut tx, j, "care", &c).await.unwrap();
    assert_eq!(d, CaseDesign::Draft { draft_rev: 7 });
    let case = store::create_case(&mut tx, j, &c, "Try this", &d)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(case_stamp(&e.owner, t, &case).await, (None, Some(7)));
}

#[tokio::test]
async fn prod_and_draft_rev_together_rejected() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "both").await;
    let rel = "cfg-2026-10-21.1";
    insert_release(&e.owner, t, j, rel).await;
    let mut n = 0;
    let mut problems = Vec::new();
    for stamp in [
        ("prod", None, Some(1)),      // prod runs no draft
        ("prod", Some(rel), Some(1)), // never both
        ("test", Some(rel), Some(1)), // test runs no release
    ] {
        expect_each(
            &e.owner,
            (t, j),
            &mut n,
            stamp,
            check_violation,
            &mut problems,
        )
        .await;
    }
    assert!(problems.is_empty(), "{problems:#?}");
    // a writer handed the other env's design is refused, not silently mis-stamped
    let mut tx = scoped(&e.state.db, t, Some(j)).await;
    let err = store::create_case(
        &mut tx,
        j,
        &conv_of("conv-x", "prod"),
        "hi",
        &CaseDesign::Draft { draft_rev: 1 },
    )
    .await
    .unwrap_err();
    let agent_api::error::ApiError::Internal(inner) = &err else {
        panic!("want the CHECK's 500, got {err:?}");
    };
    assert_eq!(
        inner.downcast_ref::<sqlx::Error>().and_then(sqlstate),
        Some("23514".to_string()),
        "{err:?}"
    );
}

#[tokio::test]
async fn release_fk_rejects_unknown_release() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "fk").await;
    let other = insert_job(&e.owner, t, "sales").await;
    let rel = "cfg-2026-10-21.1";
    insert_release(&e.owner, t, other, rel).await;
    let fk = |table: &str| Some(("23503".to_string(), format!("{table}_release")));
    let mut n = 0;
    let mut problems = Vec::new();
    // a release nobody cut, and a release of another job of the same tenant
    for stamp in [
        ("prod", Some("cfg-2026-10-21.9"), None),
        ("prod", Some(rel), None),
    ] {
        expect_each(&e.owner, (t, j), &mut n, stamp, fk, &mut problems).await;
    }
    // the same release on its own job is accepted
    expect_each(
        &e.owner,
        (t, other),
        &mut n,
        ("prod", Some(rel), None),
        |_| None,
        &mut problems,
    )
    .await;
    // B12 rows written before 0068 are not re-checked: both constraints are NOT VALID
    let unvalidated: Vec<String> = sqlx::query_scalar(
        "SELECT conname::text FROM pg_constraint \
          WHERE conrelid = ANY (ARRAY['agent_case', 'agent_handover_items', 'case_csat']::regclass[]) \
            AND (conname LIKE '%\\_release' OR conname LIKE '%\\_env\\_design\\_check') AND NOT convalidated \
          ORDER BY 1",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        unvalidated,
        [
            "agent_case_env_design_check",
            "agent_case_release",
            "agent_handover_items_env_design_check",
            "agent_handover_items_release",
            "case_csat_env_design_check",
            "case_csat_release",
        ]
    );
    assert!(problems.is_empty(), "{problems:#?}");
}

#[tokio::test]
async fn grounding_column_is_gone() {
    let e = common::env().await;
    let cols: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'conversation_message' ORDER BY 1",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert!(
        cols.iter().any(|c| c == "trace") && cols.iter().any(|c| c == "text"),
        "conversation_message is read: {cols:?}"
    );
    assert!(
        !cols.iter().any(|c| c == "grounding"),
        "answer_grounding (0122) is the only grounding store: {cols:?}"
    );
}

#[tokio::test]
async fn b12_customer_channel_case_is_stamped() {
    let c = conv::setup().await;
    let stamps = |table: &'static str| {
        let db = c.e.owner.clone();
        async move {
            let sql: &'static str = match table {
                "agent_case" => {
                    "SELECT case_id || ':' || coalesce(release_id, '-') || ':' || coalesce(draft_rev::text, '-') \
                       FROM agent_case ORDER BY started_at, case_id"
                }
                _ => {
                    "SELECT id || '/' || case_id || ':' || coalesce(release_id, '-') || ':' \
                            || coalesce(draft_rev::text, '-') \
                       FROM agent_handover_items ORDER BY queued_at, id"
                }
            };
            sqlx::query_scalar::<_, String>(sql)
                .fetch_all(&db)
                .await
                .unwrap()
        }
    };
    // a never-released job: the web chat case runs the baseline
    let (_, case_id) = c.customer_case("wc-1", "m1", "Where is my order?").await;
    assert_eq!(case_id, "CS-1");
    assert_eq!(stamps("agent_case").await, ["CS-1:-:-"]);

    // released: a new conversation's case and the item the customer asks for carry the release
    let rel = "cfg-2026-10-21.1";
    insert_release(&c.e.owner, c.t, c.job, rel).await;
    set_live(&c.e.owner, c.job, rel).await;
    let (s, b) = c.inbound("wc-2", "m2", "I want to talk to a human").await;
    assert_eq!(
        (s, b["status"].as_str()),
        (StatusCode::OK, Some("queued")),
        "{b}"
    );
    assert_eq!(
        stamps("agent_case").await,
        ["CS-1:-:-", "CS-2:cfg-2026-10-21.1:-"]
    );
    assert_eq!(
        stamps("agent_handover_items").await,
        ["HO-1/CS-2:cfg-2026-10-21.1:-"]
    );
    // staff hand over the older case: the item is stamped in that transaction, the case keeps its first design
    let (s, b) = c
        .call(
            "POST",
            "/api/jobs/j1/cases/CS-1/handover",
            "u-agent",
            json!({"reason": "angry customer"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        stamps("agent_handover_items").await,
        [
            "HO-1/CS-2:cfg-2026-10-21.1:-",
            "HO-2/CS-1:cfg-2026-10-21.1:-"
        ]
    );
    assert_eq!(
        stamps("agent_case").await,
        ["CS-1:-:-", "CS-2:cfg-2026-10-21.1:-"]
    );
}
