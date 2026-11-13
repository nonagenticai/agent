//! The revoked-licence tombstone (B7 T3c, `licence_revocation`) and the re-issue history (R2, `licence_reissue`),
//! migration 0096 (crate::licensing::store). Real Postgres 18 (one container for this binary); Keycloak and licences
//! are the in-process fake.
use crate::common;

use std::borrow::Cow;

use agent_api::audit::Actor;
use agent_api::identity::{IssuedLicence, RevokeOutcome, Stored};
use agent_api::licensing::store::{self, NewReissue, Via};
use agent_api::members::RevokeResult;
use agent_api::platform_db::begin_platform;
use agent_api::tenancy::begin_scoped;
use axum::http::StatusCode;
use common::fakes::{self, Upstream, lock};
use common::{TestEnv, insert_tenant, insert_user, json, person, send, sqlstate, super_admin};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

async fn setup() -> (TestEnv, Upstream, Uuid) {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| fakes::identity_env(&up, v)).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    (e, up, t)
}

/// An active member with an active member licence `lic-<sub>`.
async fn licensed_member(e: &TestEnv, t: Uuid, sub: &str) {
    insert_user(&e.owner, sub, t, "tenant_user").await;
    sqlx::query(
        "INSERT INTO member_entitlement (tenant_id, user_sub, license_id, license_subject_id, status) \
         VALUES ($1, $2, 'lic-' || $2, 'subject-1', 'active')",
    )
    .bind(t)
    .bind(sub)
    .execute(&e.owner)
    .await
    .unwrap();
}

/// `(via, actor, audit_id IS NOT NULL)` of every tombstone of `license_id` on `t`.
async fn tombstones(owner: &PgPool, t: Uuid, license_id: &str) -> Vec<(String, String, bool)> {
    sqlx::query_as(
        "SELECT via, actor, audit_id IS NOT NULL FROM licence_revocation \
          WHERE tenant_id = $1 AND license_id = $2",
    )
    .bind(t)
    .bind(license_id)
    .fetch_all(owner)
    .await
    .unwrap()
}

async fn disable(e: &TestEnv, sub: &str) {
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenant/users/{sub}/disable"),
            Some(&person("u-admin", &[])),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// A store of `license_id` for `sub` (whose entitlement is put back to `pending` first).
async fn store_again(e: &TestEnv, t: Uuid, sub: &str, license_id: &str) -> Stored {
    sqlx::query(
        "UPDATE member_entitlement SET status = 'pending', license_id = NULL, license_subject_id = NULL, \
                token_encrypted = NULL, revoked_at = NULL WHERE tenant_id = $1 AND user_sub = $2",
    )
    .bind(t)
    .bind(sub)
    .execute(&e.owner)
    .await
    .unwrap();
    let issued = IssuedLicence {
        license_id: license_id.into(),
        subject_id: "subject-1".into(),
        token_sealed: "v2:gone:AAAA".into(),
    };
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    let stored = agent_api::identity::store_member_entitlement(&mut tx, t, sub, &issued)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    stored
}

/// holder, member_sub, kind, reason, old_licence_id, new_licence_id, kid, old_revoke, status of one licence_reissue row.
type ReissueRow = (
    String,
    Option<String>,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
);

#[tokio::test]
async fn every_revoke_path_writes_one_tombstone() {
    let (e, _up, t) = setup().await;
    let actor = Actor::system("tombstone-test");

    // member: a disable
    licensed_member(&e, t, "u-kat").await;
    disable(&e, "u-kat").await;
    assert_eq!(
        tombstones(&e.owner, t, "lic-u-kat").await,
        [("member".to_string(), "u-admin".to_string(), true)]
    );

    // tenant: the tenant's own licence
    let r =
        agent_api::tenants::lifecycle::revoke_tenant_licence(&e.state, &actor, t, "lic-own", true)
            .await
            .unwrap();
    assert_eq!(r, RevokeResult::Revoked("lic-own".into()));
    assert_eq!(
        tombstones(&e.owner, t, "lic-own").await,
        [("tenant".to_string(), "tombstone-test".to_string(), true)]
    );

    // drain: a parked id the tick revokes
    sqlx::query("UPDATE tenants SET licence_revoke_pending = '{lic-parked}' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let r = agent_api::tenants::lifecycle::drain_parked(&e.state, &actor, t, "lic-parked", false)
        .await
        .unwrap();
    assert_eq!(r, RevokeResult::Revoked("lic-parked".into()));
    assert_eq!(
        tombstones(&e.owner, t, "lic-parked").await,
        [("drain".to_string(), "tombstone-test".to_string(), true)]
    );

    // lost race: a losing minter's give-back that licences confirmed
    agent_api::members::orphan_if_unrevoked(&e.state, t, "lic-loser", &RevokeOutcome::Revoked)
        .await;
    assert_eq!(
        tombstones(&e.owner, t, "lic-loser").await,
        [("lost_race".to_string(), "agent-api".to_string(), false)]
    );
    // ...and one that did not take writes none (it is parked instead)
    agent_api::members::orphan_if_unrevoked(
        &e.state,
        t,
        "lic-unrevoked",
        &RevokeOutcome::Unavailable("down".into()),
    )
    .await;
    assert!(tombstones(&e.owner, t, "lic-unrevoked").await.is_empty());

    // a second revoke of a tombstoned id writes no second row
    let r =
        agent_api::tenants::lifecycle::revoke_tenant_licence(&e.state, &actor, t, "lic-own", true)
            .await
            .unwrap();
    assert_eq!(r, RevokeResult::Revoked("lic-own".into()));
    assert_eq!(tombstones(&e.owner, t, "lic-own").await.len(), 1);
    let total: i64 =
        sqlx::query_scalar("SELECT count(*) FROM licence_revocation WHERE tenant_id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(total, 4);
}

#[tokio::test]
async fn reissue_writes_a_tombstone_and_a_reissue_row() {
    let (e, up, t) = setup().await;
    sqlx::query("UPDATE tenants SET licence_id = 'lic-tenant', licence_token_encrypted = 'v2:gone:old' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenants/{t}/licence/reissue"),
            Some(&super_admin()),
            json!({"tenant_licence": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["results"][0]["status"], "reissued", "{b}");
    let new = b["results"][0]["new_licence_id"]
        .as_str()
        .unwrap()
        .to_string();
    let ts = tombstones(&e.owner, t, "lic-tenant").await;
    assert_eq!(ts.len(), 1, "{ts:?}");
    assert_eq!(ts[0].0, "reissue");
    assert!(ts[0].2, "linked to its audit row");

    let row: ReissueRow = sqlx::query_as(
            "SELECT holder, member_sub, kind, reason, old_licence_id, new_licence_id, kid, old_revoke, status \
               FROM licence_reissue WHERE tenant_id = $1",
        )
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(
        row,
        (
            "tenant".to_string(),
            None,
            "reissue".to_string(),
            "requested".to_string(),
            Some("lic-tenant".to_string()),
            Some(new.clone()),
            Some(e.state.identity.cipher.current_kid().to_string()),
            "revoked".to_string(),
            "reissued".to_string(),
        )
    );
    // the stored token is v2 under the current kid
    let token: Option<String> =
        sqlx::query_scalar("SELECT licence_token_encrypted FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert!(token.unwrap().starts_with("v2:v1:"));

    let mut ptx = begin_platform(&e.state.db).await.unwrap();
    let last = store::last_reissue(&mut ptx, t)
        .await
        .unwrap()
        .expect("a row");
    assert_eq!(last.new_licence_id.as_deref(), Some(new.as_str()));
    assert_eq!(last.holder, "tenant");
    ptx.commit().await.unwrap();
    assert!(lock(&up.mock).revoked.contains(&"lic-tenant".to_string()));
}

#[tokio::test]
async fn last_reissue_is_the_newest_row_per_tenant() {
    let (e, _up, t) = setup().await;
    let other = Uuid::new_v4();
    let row = |tenant: Uuid, new: &'static str| NewReissue {
        tenant,
        actor: "sa-1",
        holder: "member",
        member_sub: Some("u-1"),
        kind: "reissue",
        reason: "requested",
        old_licence_id: Some("lic-old"),
        new_licence_id: Some(new),
        kid: Some("k1"),
        old_revoke: "revoked",
        status: "reissued",
    };
    let mut ptx = begin_platform(&e.state.db).await.unwrap();
    assert!(store::last_reissue(&mut ptx, t).await.unwrap().is_none());
    store::record_reissue(&mut ptx, &row(t, "lic-a"))
        .await
        .unwrap();
    store::record_reissue(&mut ptx, &row(other, "lic-x"))
        .await
        .unwrap();
    store::record_reissue(&mut ptx, &row(t, "lic-b"))
        .await
        .unwrap();
    // a pending re-mint: no new id yet
    store::record_reissue(
        &mut ptx,
        &NewReissue {
            kind: "remint",
            reason: "leaked",
            new_licence_id: None,
            kid: None,
            old_revoke: "pending",
            status: "pending",
            ..row(other, "unused")
        },
    )
    .await
    .unwrap();
    // `at` is the transaction time: the id (uuidv7) orders rows written in one transaction
    let last = store::last_reissue(&mut ptx, t).await.unwrap().unwrap();
    assert_eq!(last.new_licence_id.as_deref(), Some("lic-b"));
    assert_eq!(last.member_sub.as_deref(), Some("u-1"));
    let all = store::last_reissues(&mut ptx, &[t, other, Uuid::new_v4()])
        .await
        .unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[&t].new_licence_id.as_deref(), Some("lic-b"));
    assert_eq!(all[&other].kind, "remint");
    assert_eq!(all[&other].status, "pending");
    ptx.commit().await.unwrap();

    // a reissued row must name its new licence
    let mut ptx = begin_platform(&e.state.db).await.unwrap();
    let refused = store::record_reissue(
        &mut ptx,
        &NewReissue {
            new_licence_id: None,
            ..row(t, "x")
        },
    )
    .await;
    assert!(refused.is_err(), "status reissued without a new licence id");
    ptx.rollback().await.unwrap();
}

#[tokio::test]
async fn a_tombstoned_id_is_never_stored_and_survives_audit_retention() {
    let (e, _up, t) = setup().await;
    licensed_member(&e, t, "u-kat").await;
    disable(&e, "u-kat").await;
    assert_eq!(
        store_again(&e, t, "u-kat", "lic-u-kat").await,
        Stored::Stale
    );
    // audit retention deletes the revoke row (as the owner): the tombstone still holds
    sqlx::query("DELETE FROM audit_event WHERE tenant_id = $1 AND action = 'license.revoke'")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    assert_eq!(
        store_again(&e, t, "u-kat", "lic-u-kat").await,
        Stored::Stale
    );
    // another id stores
    assert_eq!(
        store_again(&e, t, "u-kat", "lic-fresh").await,
        Stored::Stored
    );
}

#[tokio::test]
async fn agent_app_cannot_update_or_delete_tombstones_nor_read_reissues() {
    let (e, _up, t) = setup().await;
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    assert!(
        store::record_revocation(&mut tx, t, "lic-1", Via::Member, "u-admin", None)
            .await
            .unwrap()
    );
    assert!(
        !store::record_revocation(&mut tx, t, "lic-1", Via::Tenant, "u-admin", None)
            .await
            .unwrap(),
        "a second revoke of the same id is a no-op"
    );
    assert!(store::is_revoked(&mut tx, t, "lic-1").await.unwrap());
    tx.commit().await.unwrap();

    for sql in [
        "UPDATE licence_revocation SET via = 'drain'",
        "DELETE FROM licence_revocation",
        "SELECT count(*) FROM licence_reissue",
    ] {
        let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
        let err = sqlx::query(sql).execute(&mut *tx).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{sql}: {err}");
        tx.rollback().await.unwrap();
    }

    // another tenant sees none of this tenant's tombstones
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, "Beta").await;
    let mut tx = begin_scoped(&e.state.db, other).await.unwrap();
    assert!(!store::is_revoked(&mut tx, t, "lic-1").await.unwrap());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM licence_revocation")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 0);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn backfill_copies_audited_revokes() {
    let url = common::fresh_database().await;
    let owner = PgPool::connect(&url).await.expect("owner pool");
    let mut before = sqlx::migrate!("./migrations");
    before.migrations = Cow::Owned(before.iter().filter(|m| m.version < 96).cloned().collect());
    before.run(&owner).await.expect("migrate below 0096");
    let t = Uuid::new_v4();
    insert_tenant(&owner, t, "Old").await;
    for (action, status, resource, details) in [
        (
            "license.revoke",
            "success",
            "member_entitlement",
            json!({"license_id": "lic-m"}),
        ),
        (
            "license.revoke",
            "success",
            "member_entitlement",
            json!({"license_id": "lic-m", "already_revoked": true}),
        ),
        (
            "license.revoke",
            "success",
            "tenant",
            json!({"license_id": "lic-t"}),
        ),
        (
            "license.revoke",
            "success",
            "tenant",
            json!({"license_id": "lic-r", "via": "reissue"}),
        ),
        (
            "license.revoke_failed",
            "failure",
            "tenant",
            json!({"license_id": "lic-f"}),
        ),
        (
            "license.revoke",
            "failure",
            "tenant",
            json!({"license_id": "lic-x"}),
        ),
    ] {
        sqlx::query(
            "INSERT INTO audit_event (tenant_id, actor_id, actor_type, action, resource_type, resource_id, status, details) \
             VALUES ($1, 'u-old', 'human', $2, $3, 'r', $4, $5)",
        )
        .bind(t)
        .bind(action)
        .bind(resource)
        .bind(status)
        .bind(details)
        .execute(&owner)
        .await
        .unwrap();
    }
    sqlx::migrate!("./migrations")
        .run(&owner)
        .await
        .expect("0096 applies with its backfill");
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT license_id, via, actor FROM licence_revocation WHERE tenant_id = $1 ORDER BY license_id",
    )
    .bind(t)
    .fetch_all(&owner)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            (
                "lic-m".to_string(),
                "member".to_string(),
                "u-old".to_string()
            ),
            (
                "lic-r".to_string(),
                "reissue".to_string(),
                "u-old".to_string()
            ),
            (
                "lic-t".to_string(),
                "tenant".to_string(),
                "u-old".to_string()
            ),
        ]
    );
}
