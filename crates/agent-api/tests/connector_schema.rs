//! B7 T4 + T8 schema: the platform connector template tables (0091: CHECKs, the immutability guard, one draft per
//! key, role grants, the insert-only boot seeder) and the tenant connector tables (0093: instance CHECKs and the
//! template-exists trigger, tenant isolation, the definer-only `tenant_data_key`). Real Postgres 18 (one container
//! for this binary, a fresh database per test). Fixture templates use `t8_*` keys, never seeded ones.
use crate::common;

use agent_api::error::ApiError;
use agent_api::platform_db::begin_platform;
use agent_api::tenancy::begin_scoped;
use common::{insert_tenant, sqlstate};
use sqlx::PgPool;
use uuid::Uuid;

/// The message of a database error (the trigger's RAISE text).
fn db_message(e: &sqlx::Error) -> String {
    e.as_database_error()
        .map(|d| d.message().to_string())
        .unwrap_or_default()
}

/// The error is the 0091 guard's `template_immutable`, which the API maps to 409.
fn assert_immutable(r: Result<sqlx::postgres::PgQueryResult, sqlx::Error>, what: &str) {
    let e = r.expect_err(what);
    assert_eq!(sqlstate(&e).as_deref(), Some("P0001"), "{what}: {e}");
    assert_eq!(db_message(&e), "template_immutable", "{what}");
    assert_eq!(ApiError::from(e).code(), "template_immutable", "{what}");
}

async fn insert_template(
    db: &PgPool,
    key: &str,
    version: &str,
    status: &str,
    transport: &str,
    auth_kind: &str,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query(
        "INSERT INTO connector_template (key, version, label, vendor, system_class, wave, transport, auth_kind, status) \
         VALUES ($1, $2, 'T8', 'Vendor', 'commerce', 'v1', $3, $4, $5)",
    )
    .bind(key)
    .bind(version)
    .bind(transport)
    .bind(auth_kind)
    .bind(status)
    .execute(db)
    .await
}

async fn template(db: &PgPool, key: &str, version: &str, status: &str) {
    insert_template(db, key, version, status, "rest", "api_key_header")
        .await
        .unwrap();
}

async fn insert_op(
    db: &PgPool,
    key: &str,
    version: &str,
    verb: &str,
    side_effect: &str,
    timeout_ms: i32,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query(
        "INSERT INTO connector_template_op (key, version, object, verb, variant, side_effect, method, path, timeout_ms) \
         VALUES ($1, $2, 'order', $3, '*', $4, 'GET', '/orders', $5)",
    )
    .bind(key)
    .bind(version)
    .bind(verb)
    .bind(side_effect)
    .bind(timeout_ms)
    .execute(db)
    .await
}

async fn exec(
    db: &PgPool,
    sql: &'static str,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query(sql).execute(db).await
}

#[tokio::test]
async fn checks_reject_bad_transport_auth_status_side_effect_and_timeout() {
    let e = common::env().await;
    let db = &e.owner;
    for (transport, auth, status) in [
        ("ftp", "none", "draft"),
        ("rest", "magic", "draft"),
        ("rest", "none", "live"),
    ] {
        let err = insert_template(db, "t8_bad", "1", status, transport, auth)
            .await
            .expect_err("refused");
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some("23514"),
            "{transport}/{auth}/{status}"
        );
    }
    for transport in [
        "rest", "graphql", "platform", "stub", "soap", "graph", "ldap", "mcp", "table",
    ] {
        let key = format!("t8_{transport}");
        insert_template(db, &key, "1", "draft", transport, "none")
            .await
            .unwrap_or_else(|e| panic!("{transport}: {e}"));
    }
    for auth in [
        "api_key_header",
        "basic",
        "bearer_static",
        "oauth2_client_credentials",
        "bundle_token",
    ] {
        let key = format!("t8_auth_{auth}");
        insert_template(db, &key, "1", "draft", "rest", auth)
            .await
            .unwrap_or_else(|e| panic!("{auth}: {e}"));
    }
    template(db, "t8_ops", "1", "draft").await;
    for (side_effect, timeout) in [("Q", 1000), ("R", 99), ("R", 30001)] {
        let err = insert_op(db, "t8_ops", "1", "list", side_effect, timeout)
            .await
            .expect_err("refused");
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some("23514"),
            "{side_effect}/{timeout}"
        );
    }
    for (verb, side_effect, timeout) in [
        ("a", "R", 100),
        ("b", "W", 30000),
        ("c", "X", 5000),
        ("d", "P", 5000),
    ] {
        insert_op(db, "t8_ops", "1", verb, side_effect, timeout)
            .await
            .unwrap_or_else(|e| panic!("{verb}: {e}"));
    }
    // an operation needs its template
    let err = insert_op(db, "t8_none", "1", "list", "R", 1000)
        .await
        .expect_err("no parent");
    assert_eq!(sqlstate(&err).as_deref(), Some("23503"));
}

#[tokio::test]
async fn active_version_is_immutable() {
    let e = common::env().await;
    let db = &e.owner;
    template(db, "t8_shop", "1", "draft").await;
    insert_op(db, "t8_shop", "1", "read", "R", 1000)
        .await
        .unwrap();
    // a draft is freely editable, and is published by a status change
    exec(
        db,
        "UPDATE connector_template SET label = 'Shop' WHERE key = 't8_shop'",
    )
    .await
    .unwrap();
    exec(
        db,
        "UPDATE connector_template_op SET path = '/v2/orders' WHERE key = 't8_shop'",
    )
    .await
    .unwrap();
    exec(
        db,
        "UPDATE connector_template SET status = 'active', published_by = 'u-sa', published_at = now() \
         WHERE key = 't8_shop'",
    )
    .await
    .unwrap();
    // results stay writable
    for sql in [
        "UPDATE connector_template SET t1 = '{\"status\": \"green\", \"at\": null}' WHERE key = 't8_shop'",
        "UPDATE connector_template SET t2 = '{\"status\": \"red\", \"at\": null}' WHERE key = 't8_shop'",
        "UPDATE connector_template SET drift = '{\"vendor_version\": \"2\", \"note\": \"n\", \"pr\": \"p\"}' \
         WHERE key = 't8_shop'",
    ] {
        exec(db, sql).await.unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // design does not
    for sql in [
        "UPDATE connector_template SET label = 'Other' WHERE key = 't8_shop'",
        "UPDATE connector_template SET transport = 'graphql' WHERE key = 't8_shop'",
        "UPDATE connector_template SET published_by = 'u-other' WHERE key = 't8_shop'",
        "UPDATE connector_template SET status = 'draft' WHERE key = 't8_shop'",
        "UPDATE connector_template SET status = 'retired', label = 'x' WHERE key = 't8_shop'",
        "DELETE FROM connector_template WHERE key = 't8_shop'",
        "UPDATE connector_template_op SET path = '/v3/orders' WHERE key = 't8_shop'",
        "DELETE FROM connector_template_op WHERE key = 't8_shop'",
    ] {
        assert_immutable(exec(db, sql).await, sql);
    }
    assert_immutable(
        insert_op(db, "t8_shop", "1", "list", "R", 1000).await,
        "add an op",
    );
    // active → retired, and back (0228: super_admin restores a retired version)
    exec(
        db,
        "UPDATE connector_template SET status = 'retired' WHERE key = 't8_shop'",
    )
    .await
    .unwrap();
    assert_immutable(
        exec(
            db,
            "UPDATE connector_template SET status = 'retired', label = 'x' WHERE key = 't8_shop'",
        )
        .await,
        "a retired version's design stays fixed",
    );
    assert_immutable(
        exec(
            db,
            "UPDATE connector_template SET status = 'draft' WHERE key = 't8_shop'",
        )
        .await,
        "retired → draft",
    );
    exec(
        db,
        "UPDATE connector_template SET status = 'active' WHERE key = 't8_shop'",
    )
    .await
    .expect("retired → active restores");
    exec(
        db,
        "UPDATE connector_template SET status = 'retired' WHERE key = 't8_shop'",
    )
    .await
    .unwrap();
    exec(db, "UPDATE connector_template SET t1 = '{\"status\": \"not_run\", \"at\": null}' WHERE key = 't8_shop'")
        .await
        .expect("a retired version's results stay writable");
}

#[tokio::test]
async fn one_draft_per_key() {
    let e = common::env().await;
    let db = &e.owner;
    template(db, "t8_draft", "1", "active").await;
    template(db, "t8_draft", "2", "draft").await;
    let err = insert_template(db, "t8_draft", "3", "draft", "rest", "none")
        .await
        .expect_err("a second draft");
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"));
    template(db, "t8_draft", "3", "retired").await;
    template(db, "t8_other", "1", "draft").await;
}

#[tokio::test]
async fn agent_app_cannot_write_platform_templates() {
    let e = common::env().await;
    template(&e.owner, "t8_role", "1", "draft").await;
    insert_op(&e.owner, "t8_role", "1", "read", "R", 1000)
        .await
        .unwrap();
    let app = &e.state.db;
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM connector_template_op WHERE key = 't8_role'")
            .fetch_one(app)
            .await
            .expect("agent_app reads");
    assert_eq!(n, 1);
    for r in [
        insert_template(app, "t8_app", "1", "draft", "rest", "none").await,
        exec(
            app,
            "UPDATE connector_template SET t1 = '{}' WHERE key = 't8_role'",
        )
        .await,
        exec(app, "DELETE FROM connector_template WHERE key = 't8_role'").await,
        insert_op(app, "t8_role", "1", "list", "R", 1000).await,
        exec(
            app,
            "UPDATE connector_template_op SET path = '/x' WHERE key = 't8_role'",
        )
        .await,
        exec(
            app,
            "DELETE FROM connector_template_op WHERE key = 't8_role'",
        )
        .await,
    ] {
        let err = r.expect_err("agent_app may not write");
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    }
    // agent_platform inserts, updates the lifecycle/result columns, and edits draft operations
    let mut tx = begin_platform(app).await.unwrap();
    sqlx::query(
        "INSERT INTO connector_template (key, version, label, vendor, system_class, wave, transport, auth_kind) \
         VALUES ('t8_plat', '1', 'P', 'V', 'c', 'v1', 'rest', 'none')",
    )
    .execute(&mut *tx)
    .await
    .expect("agent_platform inserts a template");
    sqlx::query(
        "INSERT INTO connector_template_op (key, version, object, verb, variant, side_effect, method, path, timeout_ms) \
         VALUES ('t8_plat', '1', 'order', 'read', '*', 'R', 'GET', '/o', 1000)",
    )
    .execute(&mut *tx)
    .await
    .expect("agent_platform inserts an operation");
    sqlx::query("UPDATE connector_template_op SET path = '/orders' WHERE key = 't8_plat'")
        .execute(&mut *tx)
        .await
        .expect("agent_platform edits a draft operation");
    sqlx::query(
        "UPDATE connector_template SET status = 'active', published_by = 'u-sa', published_at = now() \
         WHERE key = 't8_plat'",
    )
    .execute(&mut *tx)
    .await
    .expect("agent_platform publishes");
    tx.commit().await.unwrap();
    let mut tx = begin_platform(app).await.unwrap();
    let err = sqlx::query("UPDATE connector_template SET label = 'x' WHERE key = 't8_plat'")
        .execute(&mut *tx)
        .await
        .expect_err("label is not an agent_platform column");
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"));
}

#[allow(clippy::too_many_arguments)]
async fn insert_instance(
    db: &PgPool,
    tenant: Uuid,
    id: &str,
    scope: &str,
    template_key: &str,
    env: &str,
    stub: bool,
    platform: bool,
) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
    sqlx::query(
        "INSERT INTO connector_instance (tenant_id, id, label, template_scope, template_key, template_version, env, \
                                         stub, platform, created_by) \
         VALUES ($1, $2, 'Shop', $3, $4, '1', $5, $6, $7, 'u-admin')",
    )
    .bind(tenant)
    .bind(id)
    .bind(scope)
    .bind(template_key)
    .bind(env)
    .bind(stub)
    .bind(platform)
    .execute(db)
    .await
}

async fn tenant_with_template(e: &common::TestEnv) -> Uuid {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, &format!("T8 {t}")).await;
    if sqlx::query_scalar::<_, i64>("SELECT count(*) FROM connector_template WHERE key = 't8_sys'")
        .fetch_one(&e.owner)
        .await
        .unwrap()
        == 0
    {
        template(&e.owner, "t8_sys", "1", "active").await;
    }
    t
}

#[tokio::test]
async fn stub_only_on_test() {
    let e = common::env().await;
    let t = tenant_with_template(&e).await;
    for env in ["prod", "all"] {
        let err = insert_instance(
            &e.owner,
            t,
            "stubby",
            "platform",
            "t8_sys",
            env,
            true,
            env == "all",
        )
        .await
        .expect_err("a stub outside test");
        assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{env}");
    }
    insert_instance(
        &e.owner, t, "stubby", "platform", "t8_sys", "test", true, false,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn platform_iff_all() {
    let e = common::env().await;
    let t = tenant_with_template(&e).await;
    for (env, platform) in [("all", false), ("prod", true), ("test", true)] {
        let err = insert_instance(
            &e.owner, t, "plat", "platform", "t8_sys", env, false, platform,
        )
        .await
        .expect_err("platform ⇔ all");
        assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{env}/{platform}");
    }
    insert_instance(
        &e.owner, t, "plat", "platform", "t8_sys", "all", false, true,
    )
    .await
    .unwrap();
    insert_instance(
        &e.owner,
        t,
        "shop-prod",
        "platform",
        "t8_sys",
        "prod",
        false,
        false,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn instance_id_is_a_slug() {
    let e = common::env().await;
    let t = tenant_with_template(&e).await;
    let long = format!("a{}", "b".repeat(41));
    for id in ["ab", "Shop", "1shop", "shop!", "shop shop", long.as_str()] {
        let err = insert_instance(&e.owner, t, id, "platform", "t8_sys", "prod", false, false)
            .await
            .expect_err("not a slug");
        assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{id}");
    }
    for id in ["abc", "shop_eu-2", "s12"] {
        insert_instance(&e.owner, t, id, "platform", "t8_sys", "prod", false, false)
            .await
            .unwrap_or_else(|e| panic!("{id}: {e}"));
    }
}

#[tokio::test]
async fn instance_template_must_exist_in_scope() {
    let e = common::env().await;
    let t = tenant_with_template(&e).await;
    let err = insert_instance(
        &e.owner,
        t,
        "ghost",
        "platform",
        "t8_missing",
        "prod",
        false,
        false,
    )
    .await
    .expect_err("no such platform template");
    assert_eq!(sqlstate(&err).as_deref(), Some("P0001"));
    assert_eq!(db_message(&err), "connector_template_not_found");
    assert_eq!(ApiError::from(err).code(), "connector_template_not_found");
    insert_instance(
        &e.owner, t, "real", "platform", "t8_sys", "prod", false, false,
    )
    .await
    .unwrap();
    let err =
        sqlx::query("UPDATE connector_instance SET template_key = 't8_missing' WHERE id = 'real'")
            .execute(&e.owner)
            .await
            .expect_err("re-pin to a missing template");
    assert_eq!(db_message(&err), "connector_template_not_found");
    // tenant scope (B7 T11): the same tenant's tenant_connector_template (0092)
    let err = insert_instance(
        &e.owner,
        t,
        "mine",
        "tenant",
        "t8_private",
        "prod",
        false,
        false,
    )
    .await
    .expect_err("no such tenant template");
    assert_eq!(db_message(&err), "connector_template_not_found");
    sqlx::query(
        "INSERT INTO tenant_connector_template (tenant_id, key, version, label, kind, auth_kind, created_by) \
         VALUES ($1, 't8_private', '1', 'Mine', 'custom', 'api_key_header', 'u')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    insert_instance(
        &e.owner,
        t,
        "mine",
        "tenant",
        "t8_private",
        "prod",
        false,
        false,
    )
    .await
    .unwrap();
}

/// One row in each of the four tenant tables for `tenant`'s instance `shop`.
async fn tenant_rows(db: &PgPool, tenant: Uuid) {
    insert_instance(
        db, tenant, "shop", "platform", "t8_sys", "prod", false, false,
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO connector_credential (tenant_id, instance_id, ciphertext, nonce, credential_version, \
                                           dek_version, fingerprint, updated_by) \
         VALUES ($1, 'shop', decode(repeat('ab', 32), 'hex'), decode(repeat('cd', 12), 'hex'), 1, 1, '0011aabb', 'u')",
    )
    .bind(tenant)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO connector_contract_result (tenant_id, instance_id, op, env, side_effect, outcome) \
         VALUES ($1, 'shop', 'order.read', 'test', 'R', 'green')",
    )
    .bind(tenant)
    .execute(db)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO connector_sample (tenant_id, instance_id, op, body) VALUES ($1, 'shop', 'order.read', '{}')",
    )
    .bind(tenant)
    .execute(db)
    .await
    .unwrap();
}

#[tokio::test]
async fn tenant_b_cannot_see_tenant_a_rows() {
    let e = common::env().await;
    let a = tenant_with_template(&e).await;
    let b = tenant_with_template(&e).await;
    tenant_rows(&e.owner, a).await;
    for table in [
        "connector_instance",
        "connector_credential",
        "connector_contract_result",
        "connector_sample",
    ] {
        for (tenant, want) in [(a, 1_i64), (b, 0)] {
            let mut tx = begin_scoped(&e.state.db, tenant).await.unwrap();
            let n: i64 =
                sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                    .fetch_one(&mut *tx)
                    .await
                    .unwrap();
            assert_eq!(n, want, "{table} as {tenant}");
            let column = if table == "connector_instance" {
                "label"
            } else {
                "instance_id"
            };
            let changed = sqlx::query(sqlx::AssertSqlSafe(format!(
                "UPDATE {table} SET {column} = {column} WHERE true"
            )))
            .execute(&mut *tx)
            .await
            .unwrap();
            assert_eq!(
                changed.rows_affected(),
                want as u64,
                "{table} updates only its own tenant's rows"
            );
            tx.rollback().await.unwrap();
        }
    }
    // B cannot write a row into A
    let mut tx = begin_scoped(&e.state.db, b).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO connector_instance (tenant_id, id, label, template_scope, template_key, template_version, env, \
                                         created_by) VALUES ($1, 'sneak', 'S', 'platform', 't8_sys', '1', 'prod', 'u')",
    )
    .bind(a)
    .execute(&mut *tx)
    .await
    .expect_err("another tenant's row");
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"));
}

#[tokio::test]
async fn agent_app_cannot_select_tenant_data_key() {
    let e = common::env().await;
    let t = tenant_with_template(&e).await;
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let err = sqlx::query("SELECT * FROM tenant_data_key")
        .execute(&mut *tx)
        .await
        .expect_err("definer-only");
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"));
    tx.rollback().await.unwrap();
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let err = sqlx::query("SELECT agent_tenant_dek_destroy($1)")
        .bind(t)
        .execute(&mut *tx)
        .await
        .expect_err("destroy is agent_platform's");
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"));
    tx.rollback().await.unwrap();
    // the caller's own tenant through the definer functions
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let put: bool = sqlx::query_scalar("SELECT agent_tenant_dek_put('\\x01'::bytea, 'k1', 1)")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert!(put);
    let again: bool = sqlx::query_scalar("SELECT agent_tenant_dek_put('\\x02'::bytea, 'k1', 1)")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert!(!again, "insert-only: the first key stays");
    let wrapped: Vec<u8> = sqlx::query_scalar("SELECT wrapped_dek FROM agent_tenant_dek()")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(wrapped, vec![1u8]);
    tx.commit().await.unwrap();
    let other = tenant_with_template(&e).await;
    let mut tx = begin_scoped(&e.state.db, other).await.unwrap();
    let rows: Vec<(Option<Vec<u8>>,)> =
        sqlx::query_as("SELECT wrapped_dek FROM agent_tenant_dek()")
            .fetch_all(&mut *tx)
            .await
            .unwrap();
    assert!(rows.is_empty(), "another tenant's key is not visible");
}
