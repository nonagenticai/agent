//! PK schema (migrations 0125–0127, plan PK §3): RLS, the `tenant_isolation` policy, the tenant default and the grants
//! of the new tenant tables (pattern tests/b3/schema.rs), tenant isolation, the `release_pack_leaf` immutability
//! trigger, the CHECKs, the widened `layer_row` kind and `job_overlay_row` object CHECKs, `write_revision` taking a
//! `knowledge_pack` row in a template layer and refusing it in a profile layer, and the production migration order
//! (main ≤0122, then 0125–0127). Real Postgres 18 (one container for this binary, a fresh database per test).
use crate::common;

use crate::common::classes::{Class, class_of};
use crate::common::{insert_tenant, sqlstate};
use agent_api::audit::Actor;
use agent_api::error::ApiError;
use agent_api::platform::db::rows;
use agent_api::platform_db::begin_platform;
use agent_api::tenancy::begin_scoped;
use agent_model::kinds::{Kind, Op};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// Every new PK table and the table privileges `agent_app` holds on it.
const PK_TABLES: &[(&str, &[&str])] = &[
    // 0126: an observed cache (delete + re-insert, never updated in place)
    ("platform_pack_leaf", &["SELECT", "INSERT", "DELETE"]),
    // 0126: frozen with the release
    ("release_pack_leaf", &["SELECT", "INSERT"]),
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

const RELEASE: &str = "cfg-2026-10-21.1";

fn sha(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

/// The message of a database error (a trigger's RAISE text).
fn db_message(e: &sqlx::Error) -> String {
    e.as_database_error()
        .map(|d| d.message().to_string())
        .unwrap_or_default()
}

/// A tenant with one job and one approved release [`RELEASE`]; (tenant, job).
async fn tenant_with_release(owner: &PgPool) -> (Uuid, Uuid) {
    let t = Uuid::new_v4();
    insert_tenant(owner, t, &format!("PK schema {t}")).await;
    let job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'support', 'Support', '[]', 'test') \
         RETURNING id",
    )
    .bind(t)
    .fetch_one(owner)
    .await
    .unwrap();
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, snapshot_kinds)
           VALUES ($1, $2, $3, $4, $5, '{"sections": []}', '{}', '[]', '{}', '{}', '[]', 'approved', 'u-rm', now(),
               ARRAY['languages'])"#,
    )
    .bind(t)
    .bind(job)
    .bind(RELEASE)
    .bind(sha('a'))
    .bind(sha('b'))
    .execute(owner)
    .await
    .unwrap();
    (t, job)
}

/// One `release_pack_leaf` row as the owner; the insert's result.
async fn insert_release_leaf(
    owner: &PgPool,
    tenant: Uuid,
    job: Uuid,
    pin: &str,
) -> Result<Uuid, sqlx::Error> {
    let leaf = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO release_pack_leaf (tenant_id, job_id, release_id, pin, leaf_id, embed_model) \
         VALUES ($1, $2, $3, $4, $5, 'bge-m3')",
    )
    .bind(tenant)
    .bind(job)
    .bind(RELEASE)
    .bind(pin)
    .bind(leaf)
    .execute(owner)
    .await
    .map(|_| leaf)
}

/// One `platform_pack_leaf` row as the owner; the insert's result.
async fn insert_pack_leaf(
    owner: &PgPool,
    tenant: Uuid,
    pack_key: &str,
    version: i32,
    chunks: i64,
) -> Result<Uuid, sqlx::Error> {
    let leaf = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO platform_pack_leaf (tenant_id, pack_key, version, leaf_id, parent_id, path, embed_model, \
                                         chunk_count) \
         VALUES ($1, $2, $3, $4, $5, ARRAY['retail', 'faq'], 'bge-m3', $6)",
    )
    .bind(tenant)
    .bind(pack_key)
    .bind(version)
    .bind(leaf)
    .bind(Uuid::new_v4())
    .bind(chunks)
    .execute(owner)
    .await
    .map(|_| leaf)
}

#[tokio::test]
async fn every_pk_table_has_rls_policy_default_and_grants() {
    let e = common::env().await;
    // B1's `jobs` (0033) is the reference: every PK table copies its policy and tenant default exactly.
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
    let mut problems = Vec::new();
    for &(t, privs) in PK_TABLES {
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
    }
    // the added columns: supersedes NOT NULL DEFAULT '{}'; platform_pack.state / excluded_by nullable text
    let cols: Vec<(String, String, bool, Option<String>)> = sqlx::query_as(
        "SELECT c.relname::text || '.' || a.attname::text, format_type(a.atttypid, a.atttypmod), a.attnotnull, \
                pg_get_expr(d.adbin, d.adrelid) \
           FROM pg_attribute a \
           JOIN pg_class c ON c.oid = a.attrelid \
           LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
          WHERE c.relnamespace = 'public'::regnamespace AND NOT a.attisdropped \
            AND (c.relname::text, a.attname::text) IN \
                (('source', 'supersedes'), ('knowledge_upload', 'supersedes'), \
                 ('platform_pack', 'state'), ('platform_pack', 'excluded_by')) \
          ORDER BY 1",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    let want: Vec<(String, String, bool, Option<String>)> = vec![
        (
            "knowledge_upload.supersedes".into(),
            "text[]".into(),
            true,
            Some("'{}'::text[]".into()),
        ),
        (
            "platform_pack.excluded_by".into(),
            "text".into(),
            false,
            None,
        ),
        ("platform_pack.state".into(), "text".into(), false, None),
        (
            "source.supersedes".into(),
            "text[]".into(),
            true,
            Some("'{}'::text[]".into()),
        ),
    ];
    if cols != want {
        problems.push(format!("added columns {cols:?}, want {want:?}"));
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[tokio::test]
async fn pack_tables_are_tenant_isolated() {
    let e = common::env().await;
    let (a, job_a) = tenant_with_release(&e.owner).await;
    let b = Uuid::new_v4();
    insert_tenant(&e.owner, b, &format!("PK iso B {b}")).await;
    insert_pack_leaf(&e.owner, a, "industry/retail", 2, 10)
        .await
        .unwrap();
    insert_release_leaf(&e.owner, a, job_a, "industry/retail@2")
        .await
        .unwrap();

    // tenant B sees none of A's rows, and cannot write one for A
    let mut tx = begin_scoped(&e.state.db, b).await.unwrap();
    for sql in [
        "SELECT count(*) FROM platform_pack_leaf",
        "SELECT count(*) FROM release_pack_leaf",
    ] {
        let seen: i64 = sqlx::query_scalar(sql).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(seen, 0, "agent_app in tenant B: {sql}");
    }
    let err = sqlx::query(
        "INSERT INTO platform_pack_leaf (tenant_id, pack_key, version, leaf_id, path) \
         VALUES ($1, 'base', 1, $2, '{}')",
    )
    .bind(a)
    .bind(Uuid::new_v4())
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    drop(tx);

    // tenant A sees its own; agent_app may not UPDATE either table, nor DELETE the snapshot
    let mut tx = begin_scoped(&e.state.db, a).await.unwrap();
    for sql in [
        "SELECT count(*) FROM platform_pack_leaf",
        "SELECT count(*) FROM release_pack_leaf",
    ] {
        let seen: i64 = sqlx::query_scalar(sql).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(seen, 1, "agent_app in tenant A: {sql}");
    }
    drop(tx);
    for sql in [
        "UPDATE platform_pack_leaf SET chunk_count = 1",
        "UPDATE release_pack_leaf SET embed_model = 'x'",
        "DELETE FROM release_pack_leaf",
    ] {
        let mut tx = begin_scoped(&e.state.db, a).await.unwrap();
        let err = sqlx::query(sql).execute(&mut *tx).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{sql}: {err}");
    }
    // the cache may be deleted and re-inserted by agent_app
    let mut tx = begin_scoped(&e.state.db, a).await.unwrap();
    let gone = sqlx::query("DELETE FROM platform_pack_leaf")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(gone.rows_affected(), 1);
    sqlx::query(
        "INSERT INTO platform_pack_leaf (pack_key, version, leaf_id, path) VALUES ('base', 1, $1, '{}')",
    )
    .bind(Uuid::new_v4())
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn release_pack_leaf_is_immutable_for_the_owner_too() {
    let e = common::env().await;
    let (t, job) = tenant_with_release(&e.owner).await;
    let leaf = insert_release_leaf(&e.owner, t, job, "industry/retail@2")
        .await
        .unwrap();
    for sql in [
        "UPDATE release_pack_leaf SET embed_model = 'other' WHERE leaf_id = $1",
        "UPDATE release_pack_leaf SET pin = 'industry/retail@3' WHERE leaf_id = $1",
        "DELETE FROM release_pack_leaf WHERE leaf_id = $1",
    ] {
        let err = sqlx::query(sql)
            .bind(leaf)
            .execute(&e.owner)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{sql}: {err}");
        assert_eq!(db_message(&err), "release_snapshot_immutable", "{sql}");
    }
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM release_pack_leaf WHERE leaf_id = $1")
        .bind(leaf)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(left, 1);
    // the snapshot hangs off a release of the job: no release, no row
    let err = sqlx::query(
        "INSERT INTO release_pack_leaf (tenant_id, job_id, release_id, pin, leaf_id) \
         VALUES ($1, $2, 'cfg-2026-10-21.9', 'base@1', $3)",
    )
    .bind(t)
    .bind(job)
    .bind(Uuid::new_v4())
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23503"), "{err}");
}

#[tokio::test]
async fn pk_checks_reject_bad_rows() {
    let e = common::env().await;
    let (t, job) = tenant_with_release(&e.owner).await;

    // release_pack_leaf.pin is one `key@version` pin (0054 knowledge_pack_pins_valid)
    for pin in [
        "base@1",
        "industry_dept/retail/hr@999999999",
        "a/b/c/d/e/f@7",
    ] {
        insert_release_leaf(&e.owner, t, job, pin)
            .await
            .unwrap_or_else(|err| panic!("{pin}: {err}"));
    }
    for pin in [
        "industry/retail",
        "Industry/retail@1",
        "base@0",
        "base@1000000000",
        "a/b/c/d/e/f/g@1",
        "base@1 ",
    ] {
        let err = insert_release_leaf(&e.owner, t, job, pin)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{pin:?}: {err}");
    }

    // platform_pack_leaf: pack key shape, version range, chunk count
    insert_pack_leaf(&e.owner, t, "industry/retail", 1, 0)
        .await
        .unwrap();
    for (key, version, chunks) in [
        ("Industry/retail", 1, 0),
        ("industry/", 1, 0),
        ("base", 0, 0),
        ("base", 1_000_000_000, 0),
        ("base", 1, -1),
    ] {
        let err = insert_pack_leaf(&e.owner, t, key, version, chunks)
            .await
            .unwrap_err();
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some("23514"),
            "{key} {version} {chunks}: {err}"
        );
    }

    // platform_pack: state and excluded_by
    sqlx::query(
        "INSERT INTO platform_pack (tenant_id, pack_key, version, root_id, listed) VALUES ($1, 'base', 1, $2, true)",
    )
    .bind(t)
    .bind(Uuid::new_v4())
    .execute(&e.owner)
    .await
    .unwrap();
    for (state, excluded_by, ok) in [
        (Some("published"), None, true),
        (Some("retired"), Some("industry"), true),
        (Some("draft"), Some("industry_dept/retail"), true),
        (Some("missing"), None, true),
        (None, None, true),
        (Some("gone"), None, false),
        (None, Some("Industry"), false),
        (None, Some("industry/"), false),
    ] {
        let r = sqlx::query(
            "UPDATE platform_pack SET state = $2, excluded_by = $3 WHERE tenant_id = $1 AND pack_key = 'base'",
        )
        .bind(t)
        .bind(state)
        .bind(excluded_by)
        .execute(&e.owner)
        .await;
        match (ok, r) {
            (true, r) => {
                r.unwrap_or_else(|err| panic!("{state:?} {excluded_by:?}: {err}"));
            }
            (false, Ok(_)) => panic!("{state:?} {excluded_by:?}: the CHECK admitted it"),
            (false, Err(err)) => {
                assert_eq!(
                    sqlstate(&err).as_deref(),
                    Some("23514"),
                    "{state:?} {excluded_by:?}: {err}"
                )
            }
        }
    }

    // supersedes on source and knowledge_upload: ≤32 keys, each 1–128 characters, not blank
    let store: Uuid = sqlx::query_scalar(
        "INSERT INTO knowledge_store (tenant_id, id, key, label, kind, region, updated_by) \
         VALUES ($1, $2, 'faq.pk', 'FAQ', 'leaf', 'eu', 'test') RETURNING id",
    )
    .bind(t)
    .bind(Uuid::new_v4())
    .fetch_one(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO source (tenant_id, job_id, id, name, type, target_store_id, language, updated_by) \
         VALUES ($1, $2, $3, 'Help', 'website', $4, 'nl', 'test')",
    )
    .bind(t)
    .bind(job)
    .bind(Uuid::new_v4())
    .bind(store)
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO knowledge_upload (tenant_id, id, store_id, language, created_by) \
         VALUES ($1, $2, $3, 'nl', 'test')",
    )
    .bind(t)
    .bind(Uuid::new_v4())
    .bind(store)
    .execute(&e.owner)
    .await
    .unwrap();
    let keys = |n: usize| -> Vec<String> { (0..n).map(|i| format!("topic.{i}")).collect() };
    let cases: Vec<(Vec<String>, bool)> = vec![
        (Vec::new(), true),
        (keys(32), true),
        (vec!["k".repeat(128)], true),
        (keys(33), false),
        (vec!["k".repeat(129)], false),
        (vec![String::new()], false),
        (vec!["   ".to_string()], false),
    ];
    for sql in [
        "UPDATE source SET supersedes = $2 WHERE tenant_id = $1",
        "UPDATE knowledge_upload SET supersedes = $2 WHERE tenant_id = $1",
    ] {
        for (value, ok) in &cases {
            let r = sqlx::query(sql).bind(t).bind(value).execute(&e.owner).await;
            let n = value.len();
            match (*ok, r) {
                (true, r) => {
                    r.unwrap_or_else(|err| panic!("{sql} ({n} keys): {err}"));
                }
                (false, Ok(_)) => panic!("{sql} ({n} keys): the CHECK admitted it"),
                (false, Err(err)) => {
                    assert_eq!(
                        sqlstate(&err).as_deref(),
                        Some("23514"),
                        "{sql} ({n} keys): {err}"
                    )
                }
            }
        }
    }
}

#[tokio::test]
async fn overlay_object_check_takes_knowledge_pack() {
    let e = common::env().await;
    let (t, job) = tenant_with_release(&e.owner).await;
    let insert = "INSERT INTO job_overlay_row (tenant_id, job_id, object, key, kind, updated_by) \
                  VALUES ($1, $2, $3, 'support|faq|industry/retail', 'tombstone', 'test')";
    sqlx::query(insert)
        .bind(t)
        .bind(job)
        .bind("knowledge_pack")
        .execute(&e.owner)
        .await
        .unwrap();
    let err = sqlx::query(insert)
        .bind(t)
        .bind(job)
        .bind("knowledge_packs")
        .execute(&e.owner)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
}

/// `write_revision` in its own platform transaction, committed when it succeeds.
async fn write(
    db: &PgPool,
    layer: &str,
    key: &str,
    op: Op,
    fields: Value,
) -> Result<rows::Row, ApiError> {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    let r = rows::write_revision(
        &mut tx,
        &Actor::system("service:agent-api-pk-test"),
        layer,
        Kind::KnowledgePack,
        key,
        op,
        fields,
        "pk-author",
    )
    .await;
    if r.is_ok() {
        tx.commit().await.expect("commit the revision");
    }
    r
}

/// (status, code) of a refusal.
fn refusal<T: std::fmt::Debug>(r: Result<T, ApiError>) -> (u16, &'static str) {
    let e = r.expect_err("expected a refusal");
    let (status, code) = e.status_and_code();
    (status.as_u16(), code)
}

#[tokio::test]
async fn layer_rows_take_knowledge_pack_in_template_layers_only() {
    let e = common::env().await;
    let n = Uuid::new_v4().simple().to_string();
    let job = format!("job/pk_{n}");
    let industry = format!("industry/pk_{n}");
    let profile = format!("profile/pk_{n}");
    sqlx::query("INSERT INTO layer (key, scope, job, family) VALUES ($1, 'job', $2, $1)")
        .bind(&job)
        .bind(format!("pk_{n}"))
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query("INSERT INTO layer (key, scope, industry, family) VALUES ($1, 'industry', $2, $1)")
        .bind(&industry)
        .bind(format!("pk_{n}"))
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query("INSERT INTO layer (key, scope, rule, family) VALUES ($1, 'profile', $2, $3)")
        .bind(&profile)
        .bind(format!("pk_{n}"))
        .bind(&industry)
        .execute(&e.owner)
        .await
        .unwrap();
    // the kind CHECK (0125) and the agent-model validator take it in job and industry layers (a `*` kind: D107 checks
    // a named kind against its declarations, KH T3)
    // the kind CHECK (0125) and the agent-model validator take it in job and industry layers
    let row = write(
        &e.state.db,
        &job,
        "support|*|industry/retail",
        Op::Add,
        json!({"version": 2}),
    )
    .await
    .unwrap();
    assert_eq!(row.kind, "knowledge_pack");
    assert_eq!(
        row.fields,
        json!({"version": 2}),
        "fields are {{version}} only, no summary"
    );
    write(
        &e.state.db,
        &industry,
        "*|*|industry/retail",
        Op::Add,
        json!({"version": 1}),
    )
    .await
    .unwrap();
    let tomb = write(
        &e.state.db,
        &job,
        "support|*|industry/retail",
        Op::Tombstone,
        json!({}),
    )
    .await
    .unwrap();
    assert_eq!((tomb.op.as_str(), tomb.rev), ("tombstone", 2));

    // a profile layer holds only ontology rows
    assert_eq!(
        refusal(
            write(
                &e.state.db,
                &profile,
                "*|*|base",
                Op::Add,
                json!({"version": 1})
            )
            .await
        ),
        (422, "kind_not_allowed_in_layer")
    );
    // key rule: `*` only for a whole stage/kind part (`*|<kind>` is allowed since KH T3); the pack key shape
    for key in [
        "*|fa q|base",
        "support|faq|Base",
        "support|faq",
        "sup port|faq|base",
    ] {
        assert_eq!(
            refusal(write(&e.state.db, &job, key, Op::Add, json!({"version": 1})).await),
            (422, "invalid_key"),
            "{key}"
        );
    }
    for fields in [
        json!({"version": 0}),
        json!({"version": 1, "summary": "x"}),
        json!({}),
    ] {
        assert_eq!(
            refusal(write(&e.state.db, &job, "support|*|base", Op::Add, fields.clone()).await),
            (422, "invalid_row"),
            "{fields}"
        );
    }

    // the table CHECK refuses an unknown kind even for the owner
    let err = sqlx::query(
        "INSERT INTO layer_row (layer_id, kind, key, op, fields, rev, is_current, author, fields_hash) \
         SELECT id, 'knowledge_packs', 'k', 'add', '{}', 1, true, 'test', $2 FROM layer WHERE key = $1",
    )
    .bind(&job)
    .bind(sha('c'))
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
}

/// Production: main's migrations are applied, then PK's 0125–0127 land. Two first states, since PK may ship before
/// or after B13: ≤0122 without B13's 0110, 0112–0115 and 0124 (B13 then lands with PK, out of number order as
/// `tests/b13_migration_order.rs` models), and everything ≤0124 (B13 already live). PK references neither. B4's
/// 0062/0064–0066/0068/0123/0128 are in neither first state: production applies them after PK
/// (`tests/b4/migration_order.rs`), so here they land with PK in the second run.
#[tokio::test]
async fn pk_migrations_apply_on_top_of_0122() {
    const B13: [i64; 6] = [110, 112, 113, 114, 115, 124];
    const B4: [i64; 7] = [62, 64, 65, 66, 68, 123, 128];
    for with_b13 in [false, true] {
        let url = common::fresh_database().await;
        let owner = PgPool::connect(&url).await.expect("owner pool");
        let mut before = sqlx::migrate!("./migrations");
        before.migrations = std::borrow::Cow::Owned(
            before
                .iter()
                .filter(|m| {
                    !B4.contains(&m.version)
                        && if with_b13 {
                            m.version <= 124
                        } else {
                            m.version <= 122 && !B13.contains(&m.version)
                        }
                })
                .cloned()
                .collect(),
        );
        before
            .run(&owner)
            .await
            .unwrap_or_else(|e| panic!("first state (B13 {with_b13}): {e}"));
        let pk_before: Vec<i64> =
            sqlx::query_scalar("SELECT version FROM _sqlx_migrations WHERE version >= 125")
                .fetch_all(&owner)
                .await
                .unwrap();
        assert!(pk_before.is_empty(), "{pk_before:?}");
        let b13_before: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM _sqlx_migrations WHERE success AND version = ANY ($1)",
        )
        .bind(B13.to_vec())
        .fetch_one(&owner)
        .await
        .unwrap();
        assert_eq!(b13_before, if with_b13 { B13.len() as i64 } else { 0 });
        sqlx::migrate!("./migrations")
            .run(&owner)
            .await
            .unwrap_or_else(|e| panic!("then the full set (B13 {with_b13}): {e}"));
        let after: Vec<i64> = sqlx::query_scalar(
            "SELECT version FROM _sqlx_migrations WHERE success AND (version BETWEEN 125 AND 127 \
                                                                     OR version = ANY ($1)) ORDER BY 1",
        )
        .bind(B13.to_vec())
        .fetch_all(&owner)
        .await
        .unwrap();
        assert_eq!(after, [110, 112, 113, 114, 115, 124, 125, 126, 127]);
    }
}
