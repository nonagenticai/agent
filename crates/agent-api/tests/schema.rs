//! B1 schema (migrations 0032, 0033, 0037): constraints, triggers, definer functions, upgrades from 0030 and the
//! anti-drift class registry. Real Postgres 18 (one container for this binary).
use crate::common;

use agent_api::error::ApiError;
use agent_api::tenancy::begin_scoped;
use common::classes::{CLASSES, Class, class_of};
use common::{insert_tenant, insert_user, sqlstate};
use sqlx::PgPool;
use uuid::Uuid;

/// A fresh database migrated only up to 0030 (the schema before B1), plus its owner pool.
async fn at_0030() -> (String, PgPool) {
    let url = common::fresh_database().await;
    let owner = PgPool::connect(&url).await.expect("owner pool");
    sqlx::migrate!("./migrations")
        .run_to(30, &owner)
        .await
        .expect("migrate to 0030");
    (url, owner)
}

async fn insert_entitlement(db: &PgPool, tenant: Uuid, sub: &str, status: &str, lic: Option<&str>) {
    sqlx::query(
        "INSERT INTO member_entitlement (tenant_id, user_sub, status, license_id) VALUES ($1, $2, $3, $4)",
    )
    .bind(tenant)
    .bind(sub)
    .bind(status)
    .bind(lic)
    .execute(db)
    .await
    .unwrap();
}

#[tokio::test]
async fn upgrade_from_0030_with_rows_sets_residency_eu() {
    let (url, owner) = at_0030().await;
    let t = Uuid::new_v4();
    sqlx::query("INSERT INTO tenants (id, name, updated_by) VALUES ($1, 'Legacy', 'test')")
        .bind(t)
        .execute(&owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, tenant_role, updated_by) VALUES ('u-legacy', $1, 'tenant_admin', 'test')",
    )
    .bind(t)
    .execute(&owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO signup_approvals (email, name, source, intake_client, status, approved_tenant_id, \
                                       decided_at, decided_by, decided_via) \
         VALUES ('a@b.co', 'A', 'web', 'agent-signup-intake', 'approved', $1, now(), 'u-super', 'ui')",
    )
    .bind(t)
    .execute(&owner)
    .await
    .unwrap();
    owner.close().await;
    let e = common::env_for(url).await;
    let (name, residency, status, industry): (String, String, String, String) =
        sqlx::query_as("SELECT name, residency, status, industry FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        (
            name.as_str(),
            residency.as_str(),
            status.as_str(),
            industry.as_str()
        ),
        ("Legacy", "eu", "active", "unassigned")
    );
    let (roles, origin, disabled): (Vec<String>, String, Option<chrono::DateTime<chrono::Utc>>) =
        sqlx::query_as("SELECT roles, origin, disabled_at FROM users WHERE sub = 'u-legacy'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        roles,
        common::role_keys("tenant_admin"),
        "a D55 admin keeps everything (0039)"
    );
    assert_eq!(origin, "add_by_sub");
    assert!(disabled.is_none());
    let tid: Option<Uuid> = sqlx::query_scalar(
        "SELECT approved_tenant_id FROM signup_approvals WHERE email = 'a@b.co'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(tid, Some(t));
}

#[tokio::test]
async fn upgrade_copies_tenant_license_rows_as_active_entitlements() {
    let (url, owner) = at_0030().await;
    let t = Uuid::new_v4();
    sqlx::query("INSERT INTO tenants (id, name, updated_by) VALUES ($1, 'Licensed', 'test')")
        .bind(t)
        .execute(&owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO tenant_license (tenant_id, license_id, subject_id, holder_sub, token_encrypted) \
         VALUES ($1, 'lic-legacy-7', 'subject-legacy', 'u-holder', 'v1:c2VhbGVk')",
    )
    .bind(t)
    .execute(&owner)
    .await
    .unwrap();
    // up to just before 0151 (which admits `v2:` tokens only): 0037 copied the row as it was
    sqlx::migrate!("./migrations")
        .run_to(150, &owner)
        .await
        .expect("migrate to 0150");
    let row: (String, String, String, String, String) = sqlx::query_as(
        "SELECT user_sub, status, license_id, license_subject_id, token_encrypted \
           FROM member_entitlement WHERE tenant_id = $1",
    )
    .bind(t)
    .fetch_one(&owner)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            "u-holder".to_string(),
            "active".to_string(),
            "lic-legacy-7".to_string(),
            "subject-legacy".to_string(),
            "v1:c2VhbGVk".to_string()
        )
    );
    // 0151's guard: a remaining `v1:` token fails the migration loudly (naming the count, never the value) and
    // nothing of it applies
    let err = sqlx::migrate!("./migrations")
        .run(&owner)
        .await
        .expect_err("0151 refuses a v1 token")
        .to_string();
    assert!(
        err.contains("licence_v1_tokens_remain") && err.contains("0 tenant and 1 member"),
        "{err}"
    );
    assert!(!err.contains("c2VhbGVk"), "{err}");
    let kept: String =
        sqlx::query_scalar("SELECT token_encrypted FROM member_entitlement WHERE tenant_id = $1")
            .bind(t)
            .fetch_one(&owner)
            .await
            .unwrap();
    assert_eq!(kept, "v1:c2VhbGVk", "the token is not lost");
    // once it is resealed (here: replaced by a v2 value) the upgrade completes
    sqlx::query(
        "UPDATE member_entitlement SET token_encrypted = 'v2:k1:c2VhbGVk' WHERE tenant_id = $1",
    )
    .bind(t)
    .execute(&owner)
    .await
    .unwrap();
    owner.close().await;
    let e = common::env_for(url).await;
    let gone: bool = sqlx::query_scalar("SELECT to_regclass('public.tenant_license') IS NULL")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert!(gone, "tenant_license is dropped");
    let (lic, revoke_pending): (Option<String>, Vec<String>) =
        sqlx::query_as("SELECT licence_id, licence_revoke_pending FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert!(
        lic.is_none(),
        "the tenant licence is minted by the tick, not the migration"
    );
    assert!(revoke_pending.is_empty());
}

#[tokio::test]
async fn entitlement_unique_per_tenant_and_sub() {
    let e = common::env().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_tenant(&e.owner, b, "Beta").await;
    insert_entitlement(&e.owner, a, "u-1", "pending", None).await;
    let err =
        sqlx::query("INSERT INTO member_entitlement (tenant_id, user_sub) VALUES ($1, 'u-1')")
            .bind(a)
            .execute(&e.owner)
            .await
            .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
    // the same subject in another tenant is another row
    insert_entitlement(&e.owner, b, "u-1", "pending", None).await;
}

#[tokio::test]
async fn active_entitlement_requires_license_id() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    let err = sqlx::query(
        "INSERT INTO member_entitlement (tenant_id, user_sub, status) VALUES ($1, 'u-1', 'active')",
    )
    .bind(a)
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
    insert_entitlement(&e.owner, a, "u-1", "active", Some("lic-1")).await;
}

#[tokio::test]
async fn tenant_a_cannot_read_tenant_b_entitlements() {
    let e = common::env().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_tenant(&e.owner, b, "Beta").await;
    insert_entitlement(&e.owner, a, "u-a", "active", Some("lic-a")).await;
    insert_entitlement(&e.owner, b, "u-b", "active", Some("lic-b")).await;
    let mut tx = begin_scoped(&e.state.db, a).await.unwrap();
    let all: i64 = sqlx::query_scalar("SELECT count(*) FROM member_entitlement")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    let theirs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM member_entitlement WHERE tenant_id = $1")
            .bind(b)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
    assert_eq!((all, theirs), (1, 0));
    // and cannot write a row into B either
    let err =
        sqlx::query("INSERT INTO member_entitlement (tenant_id, user_sub) VALUES ($1, 'u-x')")
            .bind(b)
            .execute(&mut *tx)
            .await
            .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
}

#[tokio::test]
async fn app_role_cannot_delete_entitlements() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_entitlement(&e.owner, a, "u-a", "pending", None).await;
    let mut tx = begin_scoped(&e.state.db, a).await.unwrap();
    let err = sqlx::query("DELETE FROM member_entitlement")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    drop(tx);
    // control: the app role may update it (pending -> revoking)
    let mut tx = begin_scoped(&e.state.db, a).await.unwrap();
    let n = sqlx::query("UPDATE member_entitlement SET status = 'revoking' WHERE user_sub = 'u-a'")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn residency_cannot_change() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    let mut tx = begin_scoped(&e.state.db, a).await.unwrap();
    let err = sqlx::query("UPDATE tenants SET residency = 'us' WHERE id = $1")
        .bind(a)
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("P0001"), "{err}");
    assert_eq!(ApiError::from(err).code(), "residency_immutable");
    drop(tx);
    // a new tenant must state it
    let err =
        sqlx::query("INSERT INTO tenants (id, name, updated_by) VALUES ($1, 'NoResidency', 't')")
            .bind(Uuid::new_v4())
            .execute(&e.owner)
            .await
            .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23502"), "{err}");
    let err = sqlx::query(
        "INSERT INTO tenants (id, name, residency, updated_by) VALUES ($1, 'Mars', 'mars', 't')",
    )
    .bind(Uuid::new_v4())
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
}

#[tokio::test]
async fn archived_is_terminal() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    // archived needs archived_at, and the other way round
    let err = sqlx::query("UPDATE tenants SET status = 'archived' WHERE id = $1")
        .bind(a)
        .execute(&e.owner)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(a)
        .execute(&e.owner)
        .await
        .unwrap();
    let err = sqlx::query("UPDATE tenants SET status = 'active', archived_at = NULL WHERE id = $1")
        .bind(a)
        .execute(&e.owner)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("P0001"), "{err}");
    assert_eq!(ApiError::from(err).code(), "tenant_archived");
    // the licence columns stay writable after archive (offboard and the tick write them)
    sqlx::query("UPDATE tenants SET licence_revoke_pending = '{lic-9}' WHERE id = $1")
        .bind(a)
        .execute(&e.owner)
        .await
        .unwrap();
}

#[tokio::test]
async fn duplicate_name_case_insensitive_is_unique_violation() {
    let e = common::env().await;
    insert_tenant(&e.owner, Uuid::new_v4(), "Acme Health").await;
    let err = sqlx::query(
        "INSERT INTO tenants (id, name, residency, updated_by) VALUES ($1, '  acme HEALTH ', 'eu', 't')",
    )
    .bind(Uuid::new_v4())
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
}

#[tokio::test]
async fn disabled_member_has_no_auth_row() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_user(&e.owner, "u-1", a, "tenant_user").await;
    let auth = |db: PgPool| async move {
        sqlx::query_scalar::<_, Uuid>("SELECT tenant_id FROM agent_auth_user('u-1')")
            .fetch_optional(&db)
            .await
            .unwrap()
    };
    assert_eq!(auth(e.state.db.clone()).await, Some(a));
    sqlx::query("UPDATE users SET disabled_at = now() WHERE sub = 'u-1'")
        .execute(&e.owner)
        .await
        .unwrap();
    assert_eq!(auth(e.state.db.clone()).await, None);
}

#[tokio::test]
async fn switch_returns_previous() {
    let e = common::env().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_tenant(&e.owner, b, "Beta").await;
    let switch = |t: Uuid| {
        let db = e.state.db.clone();
        async move {
            sqlx::query_scalar::<_, Option<Uuid>>(
                "SELECT agent_switch_active_tenant('u-super', $1)",
            )
            .bind(t)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    assert_eq!(switch(a).await, None, "first switch: no previous");
    assert_eq!(switch(b).await, Some(a));
    assert_eq!(switch(b).await, Some(b), "same tenant: previous is itself");
    // the table itself is definer-only
    let err = sqlx::query("SELECT count(*) FROM super_admin_active")
        .execute(&e.state.db)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
}

#[tokio::test]
async fn every_public_table_has_a_class() {
    let e = common::env().await;
    let tables: Vec<(String, bool)> = sqlx::query_as(
        "SELECT tablename::text, rowsecurity FROM pg_tables \
          WHERE schemaname = 'public' AND tablename <> '_sqlx_migrations' ORDER BY 1",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    let priv_of = |role: &'static str, table: String, p: &'static str| {
        let db = e.owner.clone();
        async move {
            sqlx::query_scalar::<_, bool>(
                "SELECT has_table_privilege($1::text::name, 'public.' || $2, $3)",
            )
            .bind(role)
            .bind(table)
            .bind(p)
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    let mut problems = Vec::new();
    for (t, rls) in &tables {
        let Some(class) = class_of(t) else {
            problems.push(format!("{t}: not in tests/common/classes.rs"));
            continue;
        };
        let policies: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_policies WHERE schemaname = 'public' AND tablename::text = $1",
        )
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        match class {
            Class::Tenant => {
                if !rls || policies == 0 {
                    problems.push(format!("{t}: tenant table without RLS + policy"));
                }
                if !priv_of("agent_app", t.clone(), "SELECT").await {
                    problems.push(format!("{t}: tenant table agent_app cannot read"));
                }
            }
            Class::Platform => {
                for p in ["INSERT", "UPDATE", "DELETE"] {
                    if priv_of("agent_app", t.clone(), p).await {
                        problems.push(format!("{t}: platform table agent_app may {p}"));
                    }
                }
                if !priv_of("agent_platform", t.clone(), "SELECT").await {
                    problems.push(format!("{t}: platform table agent_platform cannot read"));
                }
            }
            Class::Definer => {
                if !rls {
                    problems.push(format!("{t}: definer-only table without RLS"));
                }
                for role in ["agent_app", "agent_platform"] {
                    for p in ["SELECT", "INSERT", "UPDATE", "DELETE"] {
                        if priv_of(role, t.clone(), p).await {
                            problems.push(format!("{t}: definer-only table grants {p} to {role}"));
                        }
                    }
                }
            }
        }
    }
    for (t, _) in CLASSES {
        if !tables.iter().any(|(n, _)| n == t) {
            problems.push(format!("{t}: listed in classes.rs but no such table"));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

async fn insert_job(db: &PgPool, tenant: Uuid, key: &str) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, $2, '[]', 'test') RETURNING id",
    )
    .bind(tenant)
    .bind(key)
    .fetch_one(db)
    .await
}

#[tokio::test]
async fn job_key_unique_per_tenant_not_global() {
    let e = common::env().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_tenant(&e.owner, b, "Beta").await;
    insert_job(&e.owner, a, "faq_agent").await.unwrap();
    let err = insert_job(&e.owner, a, "faq_agent").await.unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
    insert_job(&e.owner, b, "faq_agent")
        .await
        .expect("the same key in another tenant");
    let err = insert_job(&e.owner, a, "Bad-Key").await.unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
}

#[tokio::test]
async fn composite_fk_target_exists() {
    let e = common::env().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_tenant(&e.owner, b, "Beta").await;
    let unique: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_constraint c \
          WHERE c.conrelid = 'jobs'::regclass AND c.contype = 'u' \
            AND (SELECT array_agg(a.attname::text ORDER BY a.attname) FROM pg_attribute a \
                  WHERE a.attrelid = c.conrelid AND a.attnum = ANY (c.conkey)) = ARRAY['id', 'tenant_id']",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(unique, 1, "UNIQUE (tenant_id, id) on jobs");
    let job_a = insert_job(&e.owner, a, "faq_agent").await.unwrap();
    // a row of tenant B cannot reference tenant A's job
    let err = sqlx::query(
        "INSERT INTO template_upgrade_request (tenant_id, job_id, from_pins, to_pins, requested_by) \
         VALUES ($1, $2, '[]', '[]', 'test')",
    )
    .bind(b)
    .bind(job_a)
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23503"), "{err}");
}

#[tokio::test]
async fn one_open_upgrade_request_per_job() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    let job = insert_job(&e.owner, a, "faq_agent").await.unwrap();
    let open = move |db: PgPool| async move {
        sqlx::query(
            "INSERT INTO template_upgrade_request (tenant_id, job_id, from_pins, to_pins, requested_by) \
             VALUES ($1, $2, '[]', '[]', 'test')",
        )
        .bind(a)
        .bind(job)
        .execute(&db)
        .await
    };
    open(e.owner.clone()).await.unwrap();
    let err = open(e.owner.clone()).await.unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
    sqlx::query("UPDATE template_upgrade_request SET status = 'withdrawn'")
        .execute(&e.owner)
        .await
        .unwrap();
    open(e.owner.clone())
        .await
        .expect("a new request once the open one is withdrawn");
}

#[tokio::test]
async fn offering_tags_present_and_idempotent() {
    let e = common::env().await;
    let tags = |db: PgPool| async move {
        sqlx::query_as::<_, (String, i32, String, String)>(
            "SELECT layer, version, key, status FROM job_template_tag ORDER BY layer, version",
        )
        .fetch_all(&db)
        .await
        .unwrap()
    };
    let before = tags(e.owner.clone()).await;
    // no migration or boot seeder creates a template: the four tags are the ones the test offering authored
    // through the platform routes (v1..v4 of the customer care job layer)
    let expected: Vec<(String, i32, String, String)> = (1..=4)
        .map(|v| {
            (
                "job/customer_care_agent".to_string(),
                v,
                "customer_care_agent".to_string(),
                "tagged".to_string(),
            )
        })
        .collect();
    assert_eq!(before, expected);
    // a fresh database with no offering holds no template at all
    let bare = common::env_bare().await;
    assert!(tags(bare.owner.clone()).await.is_empty());
    // authoring the offering again changes nothing
    let again = common::platform::try_offering(&e).await.unwrap();
    assert_eq!(again.tag(), ("job/customer_care_agent".to_string(), 4));
    assert_eq!(
        tags(e.owner.clone()).await,
        before,
        "re-authoring the offering changes nothing"
    );
}

#[tokio::test]
async fn app_role_reads_templates_cannot_write() {
    let e = common::env().await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM job_template_tag")
        .fetch_one(&e.state.db)
        .await
        .expect("agent_app reads the tags");
    assert_eq!(
        n, 4,
        "the test offering's four tags (customer care agent v1..v4)"
    );
    for sql in [
        "INSERT INTO job_template_tag (layer, version, key, label, pins, status, created_by) \
         VALUES ('job/x', 1, 'xx', 'X', '[]', 'tagged', 't')",
        "UPDATE job_template_tag SET label = 'changed'",
        "DELETE FROM job_template_tag",
    ] {
        let err = sqlx::query(sql).execute(&e.state.db).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{sql}: {err}");
    }
}
