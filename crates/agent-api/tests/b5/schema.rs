//! B5 schema (T2: 0140, 0142; T3: 0143–0145): RLS (`tenant_isolation` + restrictive `job_scope`, copied from 0060's
//! `agent_case`), the tenant default, the exact `agent_app` / `agent_runtime` grants of every B5 table, tenant and
//! job isolation, the CHECKs and the migration order. Real Postgres 18 (one container for the `b5` binary, a fresh
//! database per test).
use std::borrow::Cow;
use std::collections::BTreeSet;

use crate::common;

use crate::common::classes::{Class, class_of};
use crate::common::{insert_tenant, runtime_pool, sqlstate};
use agent_api::tenancy::begin_scoped;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// Every B5 table, the table privileges `agent_app` holds on it and those `agent_runtime` holds (plan Data model
/// `0145`, R7).
const B5_TABLES: &[(&str, &[&str], &[&str])] = &[
    // 0140 (T2)
    (
        "ontology_entity",
        &["SELECT", "INSERT", "UPDATE", "DELETE"],
        &["SELECT"],
    ),
    (
        "ontology_entity_refinement",
        &["SELECT", "INSERT", "UPDATE", "DELETE"],
        &["SELECT"],
    ),
    (
        "ontology_relation",
        &["SELECT", "INSERT", "UPDATE", "DELETE"],
        &["SELECT"],
    ),
    // 0142 (T2): the history is append-only and not the runtime's
    (
        "procedure_version",
        &["SELECT", "INSERT", "UPDATE", "DELETE"],
        &["SELECT"],
    ),
    ("procedure_history", &["SELECT", "INSERT"], &[]),
    // 0143 (T3): the gate log is append-only for the runtime; agent_app deletes for the purge
    (
        "job_gate_question",
        &["SELECT", "INSERT", "UPDATE", "DELETE"],
        &["SELECT"],
    ),
    (
        "job_gate_option_refinement",
        &["SELECT", "INSERT", "UPDATE", "DELETE"],
        &["SELECT"],
    ),
    (
        "job_gate_rule_refinement",
        &["SELECT", "INSERT", "UPDATE", "DELETE"],
        &["SELECT"],
    ),
    (
        "gate_evaluation",
        &["SELECT", "INSERT", "DELETE"],
        &["SELECT", "INSERT"],
    ),
    ("gate_answer", &["SELECT", "INSERT"], &["SELECT", "INSERT"]),
    // 0144 (T3)
    (
        "toolset_narrowing",
        &["SELECT", "INSERT", "UPDATE", "DELETE"],
        &["SELECT"],
    ),
];

/// The design tables the runtime reads (Test env working rows) and never writes.
const DESIGN_TABLES: [&str; 8] = [
    "ontology_entity",
    "ontology_entity_refinement",
    "ontology_relation",
    "procedure_version",
    "job_gate_question",
    "job_gate_option_refinement",
    "job_gate_rule_refinement",
    "toolset_narrowing",
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
    insert_tenant(owner, t, &format!("B5 {label} {t}")).await;
    let j = insert_job(owner, t, "care").await;
    (t, j)
}

/// A transaction on `db`, scoped to `tenant` and (when given) `job`.
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

/// Runs `sql` in its own scoped transaction on `db` and commits; the SQLSTATE on failure.
async fn run(db: &PgPool, tenant: Uuid, job: Uuid, sql: &str) -> Result<(), Option<String>> {
    let mut tx = scoped(db, tenant, Some(job)).await;
    match sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(&mut *tx)
        .await
    {
        Ok(_) => {
            tx.commit().await.unwrap();
            Ok(())
        }
        Err(e) => Err(sqlstate(&e)),
    }
}

/// `run` that must succeed.
async fn ok(db: &PgPool, tenant: Uuid, job: Uuid, sql: &str) {
    if let Err(code) = run(db, tenant, job, sql).await {
        panic!("{sql}: failed with {code:?}");
    }
}

/// `run` that must fail with `code`.
async fn refused(db: &PgPool, tenant: Uuid, job: Uuid, sql: &str, code: &str) {
    assert_eq!(
        run(db, tenant, job, sql).await,
        Err(Some(code.to_owned())),
        "{sql}"
    );
}

/// One row in every B5 table of `job` (as agent_app, scoped).
async fn seed_rows(db: &PgPool, t: Uuid, j: Uuid) {
    for sql in [
        "INSERT INTO ontology_entity (name, label, scope, created_by) VALUES ('Parcel', 'Parcel', 'generic', 'u')",
        "INSERT INTO ontology_entity_refinement (name, hidden, base_hash, updated_by) VALUES ('Customer', true, 'h', 'u')",
        "INSERT INTO ontology_relation (subject, predicate, object, cardinality, created_by) \
         VALUES ('Parcel', 'sentBy', 'Customer', 'n:1', 'u')",
        "INSERT INTO procedure_version (name, version, status, context_space, description, steps, created_by) \
         VALUES ('refund', 2, 'PUBLISHED', 'refund', 'd', '[]', 'u')",
        "INSERT INTO procedure_history (name, version, actor, action, ok) VALUES ('refund', 2, 'u', 'create', true)",
        "INSERT INTO job_gate_question (key, point, question, options, fail, rule, action, updated_by) \
         VALUES ('TQ1', 'P1', 'Is the customer upset?', '{}', 'open', '{}', 'block', 'u')",
        "INSERT INTO job_gate_option_refinement (gate, option, hidden, base_hash, updated_by) \
         VALUES ('GT', 'other', true, 'h', 'u')",
        "INSERT INTO job_gate_rule_refinement (gate, threshold_value, base_hash, updated_by) VALUES ('GT', 0.6, 'h', 'u')",
        "INSERT INTO toolset_narrowing (role, activity, op, updated_by) VALUES ('worker', 'refund', 'order.refund', 'u')",
        "INSERT INTO gate_evaluation (point, env, conversation_id, outcome, decision) \
         VALUES ('P1', 'test', 'conv-1', 'pass', 'allow')",
        "INSERT INTO gate_answer (evaluation_id, gate, fired) SELECT max(id), 'TQ1', false FROM gate_evaluation",
    ] {
        ok(db, t, j, sql).await;
    }
}

async fn count(db: &PgPool, tenant: Uuid, job: Option<Uuid>, table: &str) -> i64 {
    let mut tx = scoped(db, tenant, job).await;
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(&mut *tx)
        .await
        .unwrap()
}

#[tokio::test]
async fn b5_creates_no_platform_tables() {
    let e = common::env().await;
    // B12's `agent_case` (0060) is the reference: tenant_isolation + RESTRICTIVE job_scope.
    let want_policies = policies_of(&e.owner, "agent_case").await;
    let ref_default: String = sqlx::query_scalar(
        "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
           JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
          WHERE d.adrelid = 'public.jobs'::regclass AND a.attname::text = 'tenant_id'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let mut problems = Vec::new();
    for &(t, app, runtime) in B5_TABLES {
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
            for (role, granted) in [
                ("agent_app", app),
                ("agent_runtime", runtime),
                ("agent_platform", &[][..]),
            ] {
                let has: bool = sqlx::query_scalar(
                    "SELECT has_table_privilege($1::text::name, 'public.' || $2, $3)",
                )
                .bind(role)
                .bind(t)
                .bind(p)
                .fetch_one(&e.owner)
                .await
                .unwrap();
                let want = granted.contains(&p);
                if has != want {
                    problems.push(format!("{t}: {role} {p} is {has}, want {want}"));
                }
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[tokio::test]
async fn rls_isolates_tenants_and_jobs() {
    let e = common::env().await;
    let (a, ja) = tenant_with_job(&e.owner, "rls a").await;
    let jb = insert_job(&e.owner, a, "sales").await;
    let (b, _) = tenant_with_job(&e.owner, "rls b").await;
    seed_rows(&e.state.db, a, ja).await;
    for &(t, _, _) in B5_TABLES {
        assert_eq!(
            count(&e.state.db, a, Some(ja), t).await,
            1,
            "{t}: job A sees its row"
        );
        assert_eq!(
            count(&e.state.db, a, None, t).await,
            1,
            "{t}: tenant A unscoped sees its row"
        );
        assert_eq!(
            count(&e.state.db, a, Some(jb), t).await,
            0,
            "{t}: job B sees nothing of job A"
        );
        assert_eq!(
            count(&e.state.db, b, None, t).await,
            0,
            "{t}: tenant B sees nothing of tenant A"
        );
    }
    // a write for job A under job B's scope is refused by job_scope's WITH CHECK
    let sql = format!(
        "INSERT INTO toolset_narrowing (job_id, role, activity, op, updated_by) \
         VALUES ('{ja}', 'worker', 'refund', 'order.read', 'u')"
    );
    refused(&e.state.db, a, jb, &sql, "42501").await;
}

#[tokio::test]
async fn one_published_per_name() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "published").await;
    let insert = |v: i32, status: &str| {
        format!(
            "INSERT INTO procedure_version (name, version, status, context_space, description, steps, created_by) \
             VALUES ('refund', {v}, '{status}', 'refund', 'd', '[]', 'u')"
        )
    };
    ok(&e.state.db, t, j, &insert(1, "PUBLISHED")).await;
    ok(&e.state.db, t, j, &insert(2, "DRAFT")).await;
    refused(&e.state.db, t, j, &insert(3, "PUBLISHED"), "23505").await;
    // another job may publish the same name
    let j2 = insert_job(&e.owner, t, "sales").await;
    ok(&e.state.db, t, j2, &insert(1, "PUBLISHED")).await;
}

#[tokio::test]
async fn procedure_version_is_integer_ge_1() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "version").await;
    let ty: String = sqlx::query_scalar(
        "SELECT data_type FROM information_schema.columns \
          WHERE table_schema = 'public' AND table_name = 'procedure_version' AND column_name = 'version'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(ty, "integer");
    refused(
        &e.state.db,
        t,
        j,
        "INSERT INTO procedure_version (name, version, context_space, description, steps, created_by) \
         VALUES ('refund', 0, 'refund', 'd', '[]', 'u')",
        "23514",
    )
    .await;
    refused(
        &e.state.db,
        t,
        j,
        "INSERT INTO procedure_version (name, version, status, context_space, description, steps, created_by) \
         VALUES ('refund', 1, 'LIVE', 'refund', 'd', '[]', 'u')",
        "23514",
    )
    .await;
}

#[tokio::test]
async fn procedure_history_is_append_only() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "history").await;
    ok(
        &e.state.db,
        t,
        j,
        "INSERT INTO procedure_history (name, version, actor, action, from_status, to_status, ok, reason) \
         VALUES ('refund', 2, 'u', 'transition', 'DRAFT', 'PUBLISHED', false, 'invalid_transition')",
    )
    .await;
    refused(
        &e.state.db,
        t,
        j,
        "UPDATE procedure_history SET ok = true",
        "42501",
    )
    .await;
    refused(&e.state.db, t, j, "DELETE FROM procedure_history", "42501").await;
    refused(
        &e.state.db,
        t,
        j,
        "INSERT INTO procedure_history (name, version, actor, action, ok) VALUES ('refund', 2, 'u', 'edit', true)",
        "23514",
    )
    .await;
}

#[tokio::test]
async fn entity_name_check() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "entity").await;
    for (name, accepted) in [
        ("Parcel", true),
        ("P2", true),
        ("parcel", false),
        ("Par-cel", false),
        ("", false),
    ] {
        let sql = format!(
            "INSERT INTO ontology_entity (name, label, scope, created_by) VALUES ('{name}', 'L', 'generic', 'u')"
        );
        if accepted {
            ok(&e.state.db, t, j, &sql).await;
        } else {
            refused(&e.state.db, t, j, &sql, "23514").await;
        }
    }
    refused(
        &e.state.db,
        t,
        j,
        "INSERT INTO ontology_entity (name, label, scope, created_by) VALUES ('Box', 'L', 'global', 'u')",
        "23514",
    )
    .await;
    refused(
        &e.state.db,
        t,
        j,
        "INSERT INTO ontology_entity (name, label, scope, created_by) VALUES ('Box', '', 'generic', 'u')",
        "23514",
    )
    .await;
}

#[tokio::test]
async fn relation_predicate_check() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "relation").await;
    let insert = |p: &str, card: &str| {
        format!(
            "INSERT INTO ontology_relation (subject, predicate, object, cardinality, created_by) \
             VALUES ('Parcel', '{p}', 'Customer', '{card}', 'u')"
        )
    };
    ok(&e.state.db, t, j, &insert("sentBy", "n:1")).await;
    refused(&e.state.db, t, j, &insert("SentBy", "n:1"), "23514").await;
    refused(&e.state.db, t, j, &insert("sent_by", "n:1"), "23514").await;
    refused(&e.state.db, t, j, &insert("ownedBy", "many"), "23514").await;
}

#[tokio::test]
async fn entity_attributes_must_be_array() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "attributes").await;
    ok(
        &e.state.db,
        t,
        j,
        "INSERT INTO ontology_entity (name, label, scope, attributes, created_by) \
         VALUES ('Parcel', 'Parcel', 'generic', '[{\"name\": \"weight\", \"data_type\": \"number\", \"pii\": false, \"synonyms\": []}]', 'u')",
    )
    .await;
    refused(
        &e.state.db,
        t,
        j,
        "INSERT INTO ontology_entity (name, label, scope, attributes, created_by) \
         VALUES ('Box', 'Box', 'generic', '{\"weight\": 1}', 'u')",
        "23514",
    )
    .await;
}

#[tokio::test]
async fn tenant_question_is_noul_only() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "noul").await;
    let insert = |key: &str, ty: &str| {
        format!(
            "INSERT INTO job_gate_question (key, point, type, question, options, fail, rule, action, updated_by) \
             VALUES ('{key}', 'P1', '{ty}', 'Is the customer upset?', '{{}}', 'closed', '{{}}', 'handover', 'u')"
        )
    };
    ok(&e.state.db, t, j, &insert("TQ1", "noul")).await;
    refused(&e.state.db, t, j, &insert("TQ2", "choice"), "23514").await;
    refused(&e.state.db, t, j, &insert("TQ3", "score"), "23514").await;
    // the key is UPPER, 2–8 characters
    refused(&e.state.db, t, j, &insert("tq4", "noul"), "23514").await;
    refused(&e.state.db, t, j, &insert("T", "noul"), "23514").await;
}

#[tokio::test]
async fn tools_only_on_p2() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "tools").await;
    let insert = |key: &str, point: &str| {
        format!(
            "INSERT INTO job_gate_question (key, point, question, options, fail, rule, action, tools, updated_by) \
             VALUES ('{key}', '{point}', 'Is this refund allowed?', '{{}}', 'open', '{{}}', 'block', \
                     ARRAY['order.refund'], 'u')"
        )
    };
    ok(&e.state.db, t, j, &insert("TP2", "P2")).await;
    refused(&e.state.db, t, j, &insert("TP1", "P1"), "23514").await;
    refused(&e.state.db, t, j, &insert("TR1", "R1"), "23514").await;
}

#[tokio::test]
async fn rule_refinement_holds_threshold_or_rule_not_both() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "rule").await;
    ok(
        &e.state.db,
        t,
        j,
        "INSERT INTO job_gate_rule_refinement (gate, threshold_value, base_hash, updated_by) VALUES ('GA', 0.7, 'h', 'u')",
    )
    .await;
    ok(
        &e.state.db,
        t,
        j,
        "INSERT INTO job_gate_rule_refinement (gate, rule, updated_by) \
         VALUES ('TQ1', '{\"fire_when\": \"gte\", \"threshold\": 0.8, \"fire_values\": null, \"min_confidence\": null}', 'u')",
    )
    .await;
    refused(
        &e.state.db,
        t,
        j,
        "INSERT INTO job_gate_rule_refinement (gate, threshold_value, rule, base_hash, updated_by) \
         VALUES ('GS', 0.5, '{}', 'h', 'u')",
        "23514",
    )
    .await;
}

#[tokio::test]
async fn gate_answer_cascades_with_evaluation() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "cascade").await;
    seed_rows(&e.state.db, t, j).await;
    assert_eq!(count(&e.state.db, t, Some(j), "gate_answer").await, 1);
    ok(&e.state.db, t, j, "DELETE FROM gate_evaluation").await;
    assert_eq!(count(&e.state.db, t, Some(j), "gate_answer").await, 0);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM gate_answer")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(left, 0, "the answers went with their evaluation");
}

#[tokio::test]
async fn purge_deletes_only_old_evaluations_of_the_caller() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "purge").await;
    let (other, oj) = tenant_with_job(&e.owner, "purge other").await;
    for (tenant, job) in [(t, j), (other, oj)] {
        sqlx::query(
            "INSERT INTO gate_evaluation (tenant_id, job_id, at, point, env, conversation_id, outcome, decision) \
             VALUES ($1, $2, now() - interval '91 days', 'P1', 'prod', 'old', 'pass', 'allow'), \
                    ($1, $2, now(), 'P1', 'prod', 'new', 'pass', 'allow')",
        )
        .bind(tenant)
        .bind(job)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let mut tx = scoped(&e.state.db, t, None).await;
    let purged: i64 = sqlx::query_scalar("SELECT job_design_purge_gate_log()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(purged, 1);
    let left: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT tenant_id, conversation_id FROM gate_evaluation ORDER BY 1, 2")
            .fetch_all(&e.owner)
            .await
            .unwrap();
    let mut want = vec![
        (t, "new".to_owned()),
        (other, "new".to_owned()),
        (other, "old".to_owned()),
    ];
    want.sort();
    assert_eq!(left, want, "only the caller's old row is gone");
}

#[tokio::test]
async fn narrowing_op_check() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "narrowing").await;
    for (op, accepted) in [
        ("order.refund", true),
        ("order_line.read", true),
        ("order.refund{full}", true),
        ("Order.refund", false),
        ("order", false),
        ("order.refund{}", false),
        ("order.refund.x", false),
    ] {
        let sql = format!(
            "INSERT INTO toolset_narrowing (role, activity, op, updated_by) VALUES ('worker', 'refund', '{op}', 'u')"
        );
        if accepted {
            ok(&e.state.db, t, j, &sql).await;
        } else {
            refused(&e.state.db, t, j, &sql, "23514").await;
        }
    }
}

#[tokio::test]
async fn functions_revoked_from_public() {
    let e = common::env().await;
    let rows: Vec<(String, bool, bool, bool)> = sqlx::query_as(
        "SELECT r, has_function_privilege(r, 'job_design_purge_gate_log()', 'EXECUTE'), \
                (SELECT prosecdef FROM pg_proc WHERE oid = 'job_design_purge_gate_log()'::regprocedure), \
                EXISTS (SELECT 1 FROM pg_proc p, aclexplode(p.proacl) a \
                         WHERE p.oid = 'job_design_purge_gate_log()'::regprocedure AND a.grantee = 0) \
           FROM unnest(ARRAY['agent_app', 'agent_runtime', 'agent_platform']) r ORDER BY 1",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    for (role, exec, definer, public) in rows {
        assert!(!definer, "job_design_purge_gate_log is SECURITY INVOKER");
        assert!(!public, "no EXECUTE for PUBLIC");
        assert_eq!(exec, role == "agent_app", "{role} EXECUTE");
    }
    // no B5 table or sequence carries a PUBLIC grant
    let public: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace, aclexplode(c.relacl) a \
          WHERE n.nspname = 'public' AND a.grantee = 0 AND c.relname::text = ANY ($1)",
    )
    .bind(
        B5_TABLES
            .iter()
            .map(|(t, _, _)| t.to_string())
            .chain(["procedure_history_id_seq".into(), "gate_evaluation_id_seq".into()])
            .collect::<Vec<_>>(),
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert!(public.is_empty(), "PUBLIC grants on {public:?}");
}

#[tokio::test]
async fn runtime_reads_design_tables_and_writes_only_gate_log() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "runtime").await;
    seed_rows(&e.state.db, t, j).await;
    let rt = runtime_pool(&e.owner).await;
    for table in DESIGN_TABLES {
        assert_eq!(
            count(&rt, t, Some(j), table).await,
            1,
            "{table}: the runtime reads it"
        );
    }
    ok(
        &rt,
        t,
        j,
        "INSERT INTO gate_evaluation (point, env, conversation_id, case_id, outcome, decision, deciding, model, latency_ms) \
         VALUES ('R1', 'prod', 'conv-2', 'CS-1', 'fired', 'handover', ARRAY['TQ1'], 'jev-latest', 440)",
    )
    .await;
    ok(
        &rt,
        t,
        j,
        "INSERT INTO gate_answer (evaluation_id, gate, answer, value, measure, confidence, fired, unsure, action) \
         SELECT max(id), 'TQ1', 'true', 'true', 0.9, 0.9, true, false, 'handover' FROM gate_evaluation",
    )
    .await;
    assert_eq!(count(&rt, t, Some(j), "gate_evaluation").await, 2);
    for table in DESIGN_TABLES {
        refused(
            &rt,
            t,
            j,
            &format!("UPDATE {table} SET tenant_id = tenant_id"),
            "42501",
        )
        .await;
        refused(&rt, t, j, &format!("DELETE FROM {table}"), "42501").await;
    }
    for sql in [
        "INSERT INTO ontology_entity (name, label, scope, created_by) VALUES ('Box', 'Box', 'generic', 'rt')",
        "INSERT INTO toolset_narrowing (role, activity, op, updated_by) VALUES ('worker', 'refund', 'order.read', 'rt')",
        "INSERT INTO procedure_version (name, version, context_space, description, steps, created_by) \
         VALUES ('refund', 9, 'refund', 'd', '[]', 'rt')",
        "UPDATE gate_evaluation SET decision = 'allow'",
        "DELETE FROM gate_evaluation",
        "DELETE FROM gate_answer",
        "SELECT count(*) FROM procedure_history",
        "SELECT job_design_purge_gate_log()",
    ] {
        refused(&rt, t, j, sql, "42501").await;
    }
    rt.close().await;
}

async fn applied_versions(owner: &PgPool) -> Vec<i64> {
    sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE success ORDER BY version")
        .fetch_all(owner)
        .await
        .expect("read _sqlx_migrations")
}

/// The B5 tables' columns, constraints, indexes, policies and agent_app / agent_runtime privileges, plus the purge
/// function's ACL, as sorted text rows.
async fn b5_shape(owner: &PgPool) -> BTreeSet<String> {
    let tables: Vec<String> = B5_TABLES.iter().map(|(t, _, _)| t.to_string()).collect();
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT 'col ' || table_name || ' ' || column_name || ' ' || data_type || ' ' || is_nullable \
           FROM information_schema.columns WHERE table_schema = 'public' AND table_name = ANY ($1) \
         UNION ALL SELECT 'con ' || conrelid::regclass::text || ' ' || conname || ' ' || pg_get_constraintdef(oid) \
           FROM pg_constraint WHERE conrelid::regclass::text = ANY ($1) \
         UNION ALL SELECT 'idx ' || indexdef FROM pg_indexes WHERE schemaname = 'public' AND tablename = ANY ($1) \
         UNION ALL SELECT 'pol ' || tablename || ' ' || policyname || ' ' || permissive FROM pg_policies \
          WHERE schemaname = 'public' AND tablename = ANY ($1) \
         UNION ALL SELECT 'priv ' || table_name || ' ' || grantee || ' ' || privilege_type \
           FROM information_schema.role_table_grants \
          WHERE table_schema = 'public' AND table_name = ANY ($1) AND grantee IN ('agent_app', 'agent_runtime') \
         UNION ALL SELECT 'fn ' || coalesce(proacl::text, '') FROM pg_proc WHERE proname = 'job_design_purge_gate_log'",
    )
    .bind(&tables)
    .fetch_all(owner)
    .await
    .expect("read the B5 shape");
    rows.into_iter().collect()
}

#[tokio::test]
async fn fresh_and_production_migration_orders_agree() {
    // production: main's ≤0134 already applied, then the release adds B5's 0140+
    let prod_url = common::fresh_database().await;
    let prod = PgPool::connect(&prod_url).await.expect("owner pool");
    let mut main = sqlx::migrate!("./migrations");
    main.migrations = Cow::Owned(main.iter().filter(|m| m.version <= 134).cloned().collect());
    assert!(
        main.iter().any(|m| m.version == 134),
        "main's 0134 is in the first run"
    );
    main.run(&prod).await.expect("migrate main (≤0134)");
    sqlx::migrate!("./migrations")
        .run(&prod)
        .await
        .expect("then B5's migrations");

    // a fresh database: number order
    let fresh_url = common::fresh_database().await;
    let fresh = PgPool::connect(&fresh_url).await.expect("owner pool");
    sqlx::migrate!("./migrations")
        .run(&fresh)
        .await
        .expect("migrate in number order");

    assert_eq!(
        applied_versions(&prod).await,
        applied_versions(&fresh).await
    );
    let versions = applied_versions(&fresh).await;
    for v in [140, 141, 142, 143, 144, 145] {
        assert!(versions.contains(&v), "B5 migration {v} applied");
    }
    let shape = b5_shape(&fresh).await;
    assert!(!shape.is_empty(), "B5 created its tables");
    assert_eq!(
        b5_shape(&prod).await,
        shape,
        "the B5 schema is the same in both orders"
    );
}
