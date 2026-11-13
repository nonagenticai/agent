//! B2 T2 schema (migrations 0044–0047): the design ledger, releases, release conflicts and B2's working rows. RLS,
//! policy and grants of every B2 table, release and snapshot immutability (binding the table owner too), one current
//! release per job, tenant isolation of the draft ledger, and the order the migrations apply in. Real Postgres 18
//! (one container for this binary, a fresh database per test).
use crate::common;

use agent_api::tenancy::begin_scoped;
use common::classes::{Class, class_of};
use common::{TestEnv, insert_tenant, sqlstate};
use sqlx::PgPool;
use uuid::Uuid;

const ALL: &[&str] = &["SELECT", "INSERT", "UPDATE", "DELETE"];
const SIU: &[&str] = &["SELECT", "INSERT", "UPDATE"];
const SI: &[&str] = &["SELECT", "INSERT"];

/// Every B2 table, the table privileges `agent_app` holds on it, and (when it holds no table UPDATE) the columns it
/// may UPDATE, sorted (plan B2 §3).
const B2_TABLES: [(&str, &[&str], &[&str]); 14] = [
    ("draft_ledger", ALL, &[]),
    ("release_baseline", SI, &[]),
    (
        "job_release",
        SI,
        &[
            "applied_by",
            "last_applied_at",
            "readiness",
            "released_at",
            "released_by",
            "snapshot_kinds",
            "state",
        ],
    ),
    ("release_snapshot", SI, &[]),
    ("release_bundled_draft", SI, &[]),
    (
        "published_change",
        SI,
        &["restored_to_draft", "rolled_back"],
    ),
    ("release_rollback", SI, &[]),
    ("release_conflict", SIU, &[]),
    ("job_target", SIU, &[]),
    ("job_target_history", SI, &[]),
    ("job_overlay_row", ALL, &[]),
    ("job_setting_override", ALL, &[]),
    ("business_rule", ALL, &[]),
    ("business_rule_history", SI, &[]),
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

/// A table's `tenant_id` column: NOT NULL, and its default expression.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct TenantColumn {
    not_null: bool,
    default_expr: Option<String>,
}

const R1: &str = "cfg-2026-10-20.1";
const R2: &str = "cfg-2026-10-20.2";

fn sha(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

/// The name of the constraint a database error reports (a trigger sets it with `RAISE … USING CONSTRAINT`).
fn constraint(e: &sqlx::Error) -> Option<String> {
    e.as_database_error()
        .and_then(|d| d.constraint())
        .map(str::to_string)
}

async fn new_job(db: &PgPool, tenant: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, $2, '[]', 'test') RETURNING id",
    )
    .bind(tenant)
    .bind(key)
    .fetch_one(db)
    .await
    .unwrap()
}

/// A release row in `state`; any state but `approved` carries `released_*`. Snapshot kinds `{languages,setting}`.
async fn insert_release(
    db: &PgPool,
    tenant: Uuid,
    job: Uuid,
    release: &str,
    state: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, released_by, released_at,
               snapshot_kinds)
           VALUES ($1, $2, $3, $4, $5, '{"sections": []}', '{}', '[]', '{}', '{}', '[]', $6, 'u-rm', now(),
               CASE WHEN $6 = 'approved' THEN NULL ELSE 'u-rm' END,
               CASE WHEN $6 = 'approved' THEN NULL ELSE now() END,
               ARRAY['languages', 'setting'])"#,
    )
    .bind(tenant)
    .bind(job)
    .bind(release)
    .bind(sha('a'))
    .bind(sha('b'))
    .bind(state)
    .execute(db)
    .await
    .map(|_| ())
}

/// A tenant with one job and one approved release `R1`.
async fn released_job() -> (TestEnv, Uuid, Uuid) {
    let e = common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    let j = new_job(&e.owner, t, "faq_agent").await;
    insert_release(&e.owner, t, j, R1, "approved")
        .await
        .unwrap();
    (e, t, j)
}

/// One stored snapshot row of `R1` and one baseline row, both for the place `languages`.
async fn insert_snapshots(db: &PgPool, tenant: Uuid, job: Uuid) {
    sqlx::query(
        r#"INSERT INTO release_snapshot (tenant_id, job_id, release_id, key, kind, ref, snap)
           VALUES ($1, $2, $3, 'languages', 'languages', '{}', '{"v": ["en"]}')"#,
    )
    .bind(tenant)
    .bind(job)
    .bind(R1)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO release_baseline (tenant_id, job_id, key, kind, ref, snap)
           VALUES ($1, $2, 'languages', 'languages', '{}', '{"v": ["en"]}')"#,
    )
    .bind(tenant)
    .bind(job)
    .execute(db)
    .await
    .unwrap();
}

async fn insert_ledger(db: &PgPool, tenant: Uuid, job: Uuid, key: &str) {
    sqlx::query(
        r#"INSERT INTO draft_ledger (tenant_id, job_id, key, kind, ref, area, page, label, before, after, by_sub)
           VALUES ($1, $2, $3, 'setting', '{"key": "stance"}', 'settings', 'J3', 'Stance edited', NULL,
               '{"v": "Hello"}', 'u-designer')"#,
    )
    .bind(tenant)
    .bind(job)
    .bind(key)
    .execute(db)
    .await
    .unwrap();
}

#[tokio::test]
async fn every_b2_table_has_rls_and_tenant_policy() {
    let e = common::env().await;
    // B1's `jobs` (0033) is the reference: every B2 table copies its policy and tenant default exactly.
    let (ref_qual, ref_check): (String, String) = sqlx::query_as(
        "SELECT qual, with_check FROM pg_policies \
          WHERE schemaname = 'public' AND tablename::text = 'jobs' AND policyname::text = 'tenant_isolation'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let ref_default: String = sqlx::query_scalar(
        "SELECT pg_get_expr(d.adbin, d.adrelid) FROM pg_attrdef d \
           JOIN pg_attribute a ON a.attrelid = d.adrelid AND a.attnum = d.adnum \
          WHERE d.adrelid = 'public.jobs'::regclass AND a.attname::text = 'tenant_id'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let want_policy = [Policy {
        name: "tenant_isolation".to_string(),
        permissive: "PERMISSIVE".to_string(),
        cmd: "ALL".to_string(),
        qual: Some(ref_qual),
        with_check: Some(ref_check),
    }];
    let want_tenant = Some(TenantColumn {
        not_null: true,
        default_expr: Some(ref_default),
    });
    let mut problems = Vec::new();
    for (t, privs, update_cols) in B2_TABLES {
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
                "{t}: rls={rls} force={force}, want RLS on, not forced (as 0001)"
            ));
        }
        let policies: Vec<Policy> = sqlx::query_as(
            "SELECT policyname::text AS name, permissive, cmd, qual, with_check FROM pg_policies \
                  WHERE schemaname = 'public' AND tablename::text = $1 ORDER BY 1",
        )
        .bind(t)
        .fetch_all(&e.owner)
        .await
        .unwrap();
        if policies != want_policy {
            problems.push(format!(
                "{t}: policies {policies:?}, want exactly {want_policy:?}"
            ));
        }
        let tenant_col: Option<TenantColumn> = sqlx::query_as(
            "SELECT a.attnotnull AS not_null, pg_get_expr(d.adbin, d.adrelid) AS default_expr \
               FROM pg_attribute a \
               LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
              WHERE a.attrelid = ('public.' || $1)::regclass AND a.attname::text = 'tenant_id' \
                AND NOT a.attisdropped",
        )
        .bind(t)
        .fetch_optional(&e.owner)
        .await
        .unwrap();
        if tenant_col != want_tenant {
            problems.push(format!(
                "{t}: tenant_id {tenant_col:?}, want {want_tenant:?}"
            ));
        }
        // J-scope: a composite foreign key starting (tenant_id, job_id), to jobs or to a release of the job.
        let job_fk: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_constraint c \
               WHERE c.conrelid = ('public.' || $1)::regclass AND c.contype = 'f' \
                 AND c.confrelid IN ('public.jobs'::regclass, 'public.job_release'::regclass) \
                 AND (SELECT array_agg(a.attname::text ORDER BY k.n) \
                        FROM unnest(c.conkey) WITH ORDINALITY AS k(attnum, n) \
                        JOIN pg_attribute a ON a.attrelid = c.conrelid AND a.attnum = k.attnum)[1:2] \
                     = ARRAY['tenant_id', 'job_id'])",
        )
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        if !job_fk {
            problems.push(format!("{t}: no (tenant_id, job_id) foreign key"));
        }
        for p in ["SELECT", "INSERT", "UPDATE", "DELETE"] {
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
        if !privs.contains(&"UPDATE") {
            let mut cols: Vec<String> = sqlx::query_scalar(
                "SELECT attname::text FROM pg_attribute \
                  WHERE attrelid = ('public.' || $1)::regclass AND attnum > 0 AND NOT attisdropped \
                    AND has_column_privilege('agent_app'::name, attrelid, attnum, 'UPDATE')",
            )
            .bind(t)
            .fetch_all(&e.owner)
            .await
            .unwrap();
            cols.sort();
            let want: Vec<String> = update_cols.iter().map(|c| c.to_string()).collect();
            if cols != want {
                problems.push(format!("{t}: agent_app may UPDATE {cols:?}, want {want:?}"));
            }
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[tokio::test]
async fn job_release_immutable_binds_owner() {
    let (e, _, _) = released_job().await;
    // the owner pool owns the table: RLS and the column grants do not bind it, only the trigger can
    let owns: bool = sqlx::query_scalar(
        "SELECT pg_get_userbyid(relowner) = current_user FROM pg_class WHERE oid = 'public.job_release'::regclass",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(owns, "the owner pool owns job_release");
    for sql in [
        "UPDATE job_release SET hash = 'sha256:' || repeat('c', 64)",
        "UPDATE job_release SET bundle_hash = 'sha256:' || repeat('c', 64)",
        r#"UPDATE job_release SET bundle = '{"sections": [1]}'"#,
        "UPDATE job_release SET approved_by = 'u-other'",
        "UPDATE job_release SET rollbacks_seen = 1",
    ] {
        let err = sqlx::query(sql).execute(&e.owner).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{sql}: {err}");
        assert_eq!(
            constraint(&err).as_deref(),
            Some("job_release_immutable"),
            "{sql}: {err}"
        );
    }
    let (hash, bundle_hash): (String, String) =
        sqlx::query_as("SELECT hash, bundle_hash FROM job_release")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        (hash, bundle_hash),
        (sha('a'), sha('b')),
        "the row is unchanged"
    );
}

#[tokio::test]
async fn job_release_mutable_columns_update() {
    let (e, t, j) = released_job().await;
    // apply, as the runtime role: the lifecycle columns move and the job points at the release
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let n = sqlx::query(
        r#"UPDATE job_release SET state = 'current', released_by = 'u-rm', released_at = now(),
               applied_by = 'u-rm', last_applied_at = now(), readiness = '{"ready": true, "missing": []}',
               snapshot_kinds = snapshot_kinds || ARRAY['overlay']
           WHERE release_id = $1"#,
    )
    .bind(R1)
    .execute(&mut *tx)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(n, 1);
    sqlx::query("UPDATE jobs SET current_release_id = $1 WHERE id = $2")
        .bind(R1)
        .bind(j)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    // a later apply moves it on; `applied_*` change every time
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let n = sqlx::query(
        "UPDATE job_release SET state = 'applied_earlier', applied_by = 'u-rm2', last_applied_at = now() \
          WHERE release_id = $1",
    )
    .bind(R1)
    .execute(&mut *tx)
    .await
    .unwrap()
    .rows_affected();
    assert_eq!(n, 1);
    tx.commit().await.unwrap();
    let (state, applied_by, kinds): (String, String, Vec<String>) =
        sqlx::query_as("SELECT state, applied_by, snapshot_kinds FROM job_release")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        (state.as_str(), applied_by.as_str()),
        ("applied_earlier", "u-rm2")
    );
    assert_eq!(kinds, ["languages", "setting", "overlay"]);
    // `released_*` are set once: granted, but the trigger refuses a second value
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let err = sqlx::query("UPDATE job_release SET released_at = now() + interval '1 day'")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
    assert_eq!(constraint(&err).as_deref(), Some("job_release_immutable"));
    drop(tx);
    // a column outside the grant is refused to the runtime role before any trigger
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let err = sqlx::query("UPDATE job_release SET hash = 'sha256:' || repeat('c', 64)")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
}

#[tokio::test]
async fn snapshot_kinds_append_only() {
    let (e, _, _) = released_job().await;
    // dropping a kind, swapping one for another, emptying it
    let shrink: [&[&str]; 3] = [&["languages"], &["languages", "route"], &[]];
    for kinds in shrink {
        let err = sqlx::query("UPDATE job_release SET snapshot_kinds = $1")
            .bind(kinds)
            .execute(&e.owner)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{kinds:?}: {err}");
        assert_eq!(
            constraint(&err).as_deref(),
            Some("job_release_immutable"),
            "{kinds:?}: {err}"
        );
    }
    // a superset (any order) is the coverage backfill appending a newly registered kind
    let grow: [&[&str]; 2] = [
        &["setting", "languages"],
        &["setting", "languages", "route"],
    ];
    for kinds in grow {
        let n = sqlx::query("UPDATE job_release SET snapshot_kinds = $1")
            .bind(kinds)
            .execute(&e.owner)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(n, 1, "{kinds:?}");
    }
    let kinds: Vec<String> = sqlx::query_scalar("SELECT snapshot_kinds FROM job_release")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(kinds, ["setting", "languages", "route"]);
}

#[tokio::test]
async fn release_snapshot_update_refused() {
    let (e, t, j) = released_job().await;
    insert_snapshots(&e.owner, t, j).await;
    for sql in [
        r#"UPDATE release_snapshot SET snap = '{"v": ["en", "de"]}'"#,
        "UPDATE release_snapshot SET snap = NULL",
        "UPDATE release_snapshot SET kind = 'setting'",
        r#"UPDATE release_baseline SET snap = '{"v": ["en", "de"]}'"#,
        "UPDATE release_baseline SET taken_at = now()",
    ] {
        let err = sqlx::query(sql).execute(&e.owner).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{sql}: {err}");
        assert_eq!(
            constraint(&err).as_deref(),
            Some("release_snapshot_immutable"),
            "{sql}: {err}"
        );
    }
    // the runtime role holds no UPDATE at all
    for sql in [
        r#"UPDATE release_snapshot SET snap = '{"v": ["de"]}'"#,
        r#"UPDATE release_baseline SET snap = '{"v": ["de"]}'"#,
    ] {
        let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
        let err = sqlx::query(sql).execute(&mut *tx).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{sql}: {err}");
    }
    let snaps: Vec<serde_json::Value> = sqlx::query_scalar(
        "SELECT snap FROM release_snapshot UNION ALL SELECT snap FROM release_baseline",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        snaps,
        [
            serde_json::json!({"v": ["en"]}),
            serde_json::json!({"v": ["en"]})
        ]
    );
}

#[tokio::test]
async fn one_current_release_per_job() {
    let (e, t, j) = released_job().await;
    sqlx::query(
        "UPDATE job_release SET state = 'current', released_by = 'u-rm', released_at = now()",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    let err = insert_release(&e.owner, t, j, R2, "current")
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
    assert_eq!(
        constraint(&err).as_deref(),
        Some("job_release_one_current"),
        "{err}"
    );
    // approved beside the current one is fine; so is a current release of another job
    insert_release(&e.owner, t, j, R2, "approved")
        .await
        .unwrap();
    let other = new_job(&e.owner, t, "other_agent").await;
    insert_release(&e.owner, t, other, R1, "current")
        .await
        .expect("one current per job, not per tenant");
    // moving `current` from R1 to R2 in one transaction holds the index
    let mut tx = e.owner.begin().await.unwrap();
    sqlx::query(
        "UPDATE job_release SET state = 'applied_earlier' WHERE job_id = $1 AND release_id = $2",
    )
    .bind(j)
    .bind(R1)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE job_release SET state = 'current', released_by = 'u-rm', released_at = now() \
          WHERE job_id = $1 AND release_id = $2",
    )
    .bind(j)
    .bind(R2)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let current: Vec<String> = sqlx::query_scalar(
        "SELECT release_id FROM job_release WHERE job_id = $1 AND state = 'current'",
    )
    .bind(j)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(current, [R2]);
}

#[tokio::test]
async fn agent_app_cannot_delete_snapshots() {
    let (e, t, j) = released_job().await;
    insert_snapshots(&e.owner, t, j).await;
    let tables = [
        "release_snapshot",
        "release_baseline",
        "job_release",
        "release_bundled_draft",
        "published_change",
        "release_rollback",
    ];
    for table in tables {
        let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
        // control: the runtime role reads it
        sqlx::query(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|err| panic!("{table}: agent_app reads: {err}"));
        let err = sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{table}: {err}");
    }
    let left: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM release_snapshot), (SELECT count(*) FROM release_baseline), \
                (SELECT count(*) FROM job_release)",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(left, (1, 1, 1));
}

#[tokio::test]
async fn tenant_isolation_on_draft_ledger() {
    let e = common::env().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_tenant(&e.owner, b, "Beta").await;
    let job_a = new_job(&e.owner, a, "faq_agent").await;
    let job_b = new_job(&e.owner, b, "faq_agent").await;
    insert_ledger(&e.owner, a, job_a, "setting:stance").await;
    insert_ledger(&e.owner, b, job_b, "setting:stance").await;
    insert_ledger(&e.owner, b, job_b, "setting:actor").await;

    let mut tx = begin_scoped(&e.state.db, a).await.unwrap();
    let (all, theirs): (i64, i64) =
        sqlx::query_as("SELECT count(*), count(*) FILTER (WHERE tenant_id = $1) FROM draft_ledger")
            .bind(b)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!((all, theirs), (1, 0));
    let changed = sqlx::query("UPDATE draft_ledger SET label = 'hijacked' WHERE tenant_id = $1")
        .bind(b)
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    let removed = sqlx::query("DELETE FROM draft_ledger WHERE tenant_id = $1")
        .bind(b)
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!((changed, removed), (0, 0));
    // a row without tenant_id lands in the scoped tenant
    let tenant: Uuid = sqlx::query_scalar(
        r#"INSERT INTO draft_ledger (job_id, key, kind, ref, area, page, label, after, by_sub)
           VALUES ($1, 'languages', 'languages', '{}', 'settings', 'J3', 'Languages', '{"v": ["en", "de"]}',
               'u-designer')
           RETURNING tenant_id"#,
    )
    .bind(job_a)
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    assert_eq!(tenant, a);
    tx.commit().await.unwrap();
    // and cannot write a row into B
    let mut tx = begin_scoped(&e.state.db, a).await.unwrap();
    let err = sqlx::query(
        r#"INSERT INTO draft_ledger (tenant_id, job_id, key, kind, ref, area, page, label, by_sub)
           VALUES ($1, $2, 'languages', 'languages', '{}', 'settings', 'J3', 'Languages', 'u-designer')"#,
    )
    .bind(b)
    .bind(job_b)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    drop(tx);
    let labels: Vec<String> =
        sqlx::query_scalar("SELECT label FROM draft_ledger WHERE tenant_id = $1 ORDER BY key")
            .bind(b)
            .fetch_all(&e.owner)
            .await
            .unwrap();
    assert_eq!(labels, ["Stance edited", "Stance edited"]);
}

#[tokio::test]
async fn migrations_in_build_order() {
    let url = common::fresh_database().await;
    let owner = PgPool::connect(&url).await.expect("owner pool");
    let migrator = sqlx::migrate!("./migrations");
    let embedded: Vec<i64> = migrator.iter().map(|m| m.version).collect();
    assert!(
        embedded.windows(2).all(|w| w[0] < w[1]),
        "embedded versions are unique and ordered: {embedded:?}"
    );
    for v in 44..=47 {
        assert!(embedded.contains(&v), "migration {v:04} is embedded");
    }
    // B6a merged first: a database at 0043, then the B2 range on top
    migrator.run_to(43, &owner).await.expect("migrate to 0043");
    let at: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations")
        .fetch_one(&owner)
        .await
        .unwrap();
    assert_eq!(at, 43);
    migrator.run(&owner).await.expect("migrate to head");
    let applied: Vec<(i64, bool)> = sqlx::query_as(
        "SELECT version, success FROM _sqlx_migrations ORDER BY installed_on, version",
    )
    .fetch_all(&owner)
    .await
    .unwrap();
    let versions: Vec<i64> = applied.iter().map(|(v, _)| *v).collect();
    assert!(
        applied.iter().all(|(_, ok)| *ok),
        "every migration succeeded: {applied:?}"
    );
    assert!(
        versions.windows(2).all(|w| w[0] < w[1]),
        "versions strictly increase by installed_on (no late lower version): {versions:?}"
    );
    assert_eq!(
        versions, embedded,
        "every embedded migration applied, in order"
    );
    let b2: Vec<i64> = versions
        .iter()
        .copied()
        .filter(|v| (44..=47).contains(v))
        .collect();
    assert_eq!(b2, [44, 45, 46, 47]);
    let later: bool = sqlx::query_scalar(
        "SELECT (SELECT min(installed_on) FROM _sqlx_migrations WHERE version BETWEEN 44 AND 47) \
              > (SELECT max(installed_on) FROM _sqlx_migrations WHERE version <= 43)",
    )
    .fetch_one(&owner)
    .await
    .unwrap();
    assert!(later, "0044–0047 applied after the 0043 database");
    owner.close().await;
}
