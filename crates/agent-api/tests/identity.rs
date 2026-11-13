//! B1 T4: the identity config and the Keycloak / licences operations shared by signup and tenant admin
//! (crate::identity), against the in-process fake upstream. T5 adds the licence-storage tests that need the
//! 0037 tables.
use crate::common;

use std::collections::HashMap;
use std::time::Duration;

use agent_api::config::Config;
use agent_api::identity::{
    Identity, IssuedLicence, RevokeOutcome, Stored, keycloak_error, store_member_entitlement,
    store_tenant_licence,
};
use agent_api::tenancy::begin_scoped;
use axum::http::StatusCode;
use common::fakes::{self, Upstream, lock};
use serde_json::Map;
use uuid::Uuid;

/// An `Identity` pointed at a fresh fake upstream (no database needed).
async fn identity() -> (Identity, Upstream) {
    let up = fakes::upstream().await;
    let mut vars: HashMap<String, String> = common::base_env("postgres://agent@h/agent");
    fakes::identity_env(&up, &mut vars);
    let cfg = Config::from_lookup(|k| vars.get(k).cloned()).expect("config");
    (Identity::new(cfg.identity, reqwest::Client::new()), up)
}

#[test]
fn identity_env_is_required_even_with_signup_dark() {
    let vars = common::base_env("postgres://agent@h/agent");
    assert!(!vars.contains_key("EARLY_ACCESS_SIGNUP_ENABLED"));
    let ok = Config::from_lookup(|k| vars.get(k).cloned()).expect("the full set boots");
    assert!(ok.signup.is_none(), "signup is dark");
    for (k, _) in common::identity_vars(common::NO_UPSTREAM) {
        let err = Config::from_lookup(|x| if x == k { None } else { vars.get(x).cloned() })
            .expect_err(k)
            .to_string();
        assert!(err.contains(k), "{k}: {err}");
    }
}

#[tokio::test]
async fn get_user_404_is_none_not_error() {
    let (id, up) = identity().await;
    lock(&up.mock)
        .users
        .push(("u-1".into(), "ann@globex.com".into(), Map::new()));
    assert_eq!(
        id.get_user("nobody").await.expect("404 is not an error"),
        None
    );
    let u = id.get_user("u-1").await.unwrap().expect("found");
    assert_eq!(u.sub, "u-1");
    assert_eq!(u.email.as_deref(), Some("ann@globex.com"));
    assert_eq!(
        u.name.as_deref(),
        Some("ann@globex.com"),
        "no first/last name: the username"
    );
}

/// The super_admin check reads the user's mappings on the configured `agent` client id (`AGENT_KC_CLIENT_UUID`)
/// directly: no `clients?clientId=` lookup, which would need `view-clients` (every client's secret).
#[tokio::test]
async fn has_agent_role_reads_the_configured_client_without_a_client_lookup() {
    let (id, up) = identity().await;
    {
        let mut g = lock(&up.mock);
        g.client_roles
            .push(("u-1".into(), "agent".into(), "super_admin".into()));
        g.client_roles
            .push(("u-3".into(), "other-app".into(), "super_admin".into()));
    }
    assert!(id.has_agent_role("u-1", "super_admin").await.unwrap());
    assert!(!id.has_agent_role("u-1", "tenant_admin").await.unwrap());
    assert!(!id.has_agent_role("u-2", "super_admin").await.unwrap());
    assert!(
        !id.has_agent_role("u-3", "super_admin").await.unwrap(),
        "a role on another client does not count"
    );
    assert_eq!(
        lock(&up.mock).client_lookups,
        0,
        "no clients?clientId= call"
    );
}

#[tokio::test]
async fn keycloak_5xx_is_502_keycloak_error_never_no_such_user() {
    let (id, up) = identity().await;
    lock(&up.mock)
        .users
        .push(("u-1".into(), "a@b.co".into(), Map::new()));
    lock(&up.mock).keycloak_down = true;
    let e = id
        .get_user("u-1")
        .await
        .expect_err("a 500 is an error, not None");
    assert_eq!(e.status, Some(500));
    assert_eq!(
        keycloak_error(&e).status_and_code(),
        (StatusCode::BAD_GATEWAY, "keycloak_error")
    );
    assert!(
        id.get_user("nobody").await.is_err(),
        "unknown sub under an outage is an error too"
    );
    assert!(id.has_agent_role("u-1", "super_admin").await.is_err());
}

#[tokio::test]
async fn revoke_noop_200_body_is_not_revoked() {
    let (id, up) = identity().await;
    let t = Uuid::new_v4();
    lock(&up.mock).revoke_noop = true;
    assert_eq!(
        id.revoke_licence(t, "lic-1").await,
        RevokeOutcome::NotRevoked("licence_not_revoked")
    );
    assert!(lock(&up.mock).revoked.is_empty());
    lock(&up.mock).revoke_noop = false;
    assert_eq!(id.revoke_licence(t, "lic-1").await, RevokeOutcome::Revoked);
    assert_eq!(lock(&up.mock).revoked, ["lic-1"]);
    lock(&up.mock).licences_down = true;
    assert!(matches!(
        id.revoke_licence(t, "lic-2").await,
        RevokeOutcome::Unavailable(_)
    ));
}

#[tokio::test]
async fn issue_member_licence_registers_creates_and_seals() {
    let (id, up) = identity().await;
    let t = Uuid::new_v4();
    let l = id
        .issue_member_licence(t, "Globex", "eu", "u-1")
        .await
        .unwrap();
    assert!(l.token_sealed.starts_with("v2:v1:"));
    assert_eq!(
        id.cipher.open(t, &l.token_sealed).unwrap(),
        format!("tok-{t}")
    );
    let g = lock(&up.mock);
    assert_eq!(g.registered, [t.to_string()]);
    assert_eq!(g.issued, [(t.to_string(), "u-1".to_string())]);
    assert_eq!(g.mint_ids.as_slice(), std::slice::from_ref(&l.license_id));
}

fn licence(id: &str) -> IssuedLicence {
    IssuedLicence {
        license_id: id.to_string(),
        subject_id: "subject-1".to_string(),
        token_sealed: format!("v2:gone:{id}"),
    }
}

#[tokio::test]
async fn store_lost_race_reports_lost_race_and_keeps_the_winner() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, "Alpha").await;
    sqlx::query("INSERT INTO member_entitlement (tenant_id, user_sub) VALUES ($1, 'u-1')")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let mut tx = begin_scoped(&e.state.db, t).await.unwrap();
    assert_eq!(
        store_member_entitlement(&mut tx, t, "u-1", &licence("lic-win"))
            .await
            .unwrap(),
        Stored::Stored
    );
    assert_eq!(
        store_member_entitlement(&mut tx, t, "u-1", &licence("lic-lose"))
            .await
            .unwrap(),
        Stored::LostRace
    );
    assert_eq!(
        store_member_entitlement(&mut tx, t, "u-nobody", &licence("lic-x"))
            .await
            .unwrap(),
        Stored::LostRace,
        "no pending row: nothing stored"
    );
    assert_eq!(
        store_tenant_licence(&mut tx, t, &licence("t-win"))
            .await
            .unwrap(),
        Stored::Stored
    );
    assert_eq!(
        store_tenant_licence(&mut tx, t, &licence("t-lose"))
            .await
            .unwrap(),
        Stored::LostRace
    );
    tx.commit().await.unwrap();
    let (status, lic, token): (String, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT status, license_id, token_encrypted FROM member_entitlement WHERE user_sub = 'u-1'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(status, "active");
    assert_eq!(lic.as_deref(), Some("lic-win"));
    assert_eq!(token.as_deref(), Some("v2:gone:lic-win"));
    let tl: Option<String> = sqlx::query_scalar("SELECT licence_id FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(tl.as_deref(), Some("t-win"));
}

// ---- B1b T10: the licence carries the tenant's residency region ---------------------------------------------------

#[tokio::test]
async fn every_issue_sends_the_residency_region() {
    let (id, up) = identity().await;
    let (eu, us) = (Uuid::new_v4(), Uuid::new_v4());
    id.issue_tenant_licence(eu, "Globex", "eu").await.unwrap();
    id.issue_member_licence(eu, "Globex", "eu", "u-1")
        .await
        .unwrap();
    id.issue_member_licence(us, "Initech", "us", "u-2")
        .await
        .unwrap();
    id.create_licence(us, "u-3", "US").await.unwrap();
    let g = lock(&up.mock);
    assert_eq!(
        g.issued_regions,
        [
            Some(fakes::REGION_EU.to_string()),
            Some(fakes::REGION_EU.to_string()),
            Some(fakes::REGION_US.to_string()),
            Some(fakes::REGION_US.to_string()),
        ],
        "the region id licences listed for the residency code, on every create"
    );
    assert_eq!(g.region_lookups, 1, "the region list is read once and kept");
}

#[tokio::test]
async fn a_residency_licences_has_no_region_for_is_issued_without_one() {
    let (id, up) = identity().await;
    lock(&up.mock).no_regions = true;
    let t = Uuid::new_v4();
    id.issue_member_licence(t, "Globex", "eu", "u-1")
        .await
        .unwrap();
    // a miss is re-read next time: a region registered later is picked up
    lock(&up.mock).no_regions = false;
    id.issue_member_licence(t, "Globex", "eu", "u-2")
        .await
        .unwrap();
    let g = lock(&up.mock);
    assert_eq!(g.issued_regions, [None, Some(fakes::REGION_EU.to_string())]);
    assert_eq!(g.region_lookups, 2);
}

#[tokio::test]
async fn a_failed_region_read_fails_the_issue() {
    let (id, up) = identity().await;
    lock(&up.mock).regions_down = true;
    let e = id
        .issue_member_licence(Uuid::new_v4(), "Globex", "eu", "u-1")
        .await
        .unwrap_err();
    assert_eq!(e.status, Some(503), "{e}");
    assert!(
        lock(&up.mock).issued_regions.is_empty(),
        "no licence is created without its region read"
    );
}

// ---- B2 T0b: the region list is cached for a TTL -------------------------------------------------------------------

#[tokio::test]
async fn a_second_issue_inside_the_ttl_makes_no_second_regions_call() {
    let (id, up) = identity().await;
    for sub in ["u-1", "u-2", "u-3"] {
        id.issue_member_licence(Uuid::new_v4(), "Globex", "eu", sub)
            .await
            .unwrap();
    }
    assert_eq!(lock(&up.mock).region_lookups, 1, "default TTL is minutes");
}

#[tokio::test]
async fn the_region_list_is_re_read_once_the_ttl_has_passed() {
    let (mut id, up) = identity().await;
    id.licences.set_regions_ttl(Duration::from_millis(50));
    let t = Uuid::new_v4();
    id.issue_member_licence(t, "Globex", "eu", "u-1")
        .await
        .unwrap();
    id.issue_member_licence(t, "Globex", "eu", "u-2")
        .await
        .unwrap();
    assert_eq!(lock(&up.mock).region_lookups, 1, "inside the TTL");
    tokio::time::sleep(Duration::from_millis(120)).await;
    id.issue_member_licence(t, "Globex", "eu", "u-3")
        .await
        .unwrap();
    let g = lock(&up.mock);
    assert_eq!(
        g.region_lookups, 2,
        "past the TTL even a cached hit re-reads"
    );
    assert_eq!(g.issued_regions.len(), 3);
}

#[tokio::test]
async fn an_unknown_region_re_reads_once_then_is_issued_without_one() {
    let (id, up) = identity().await;
    let t = Uuid::new_v4();
    id.issue_member_licence(t, "Globex", "eu", "u-1")
        .await
        .unwrap();
    assert_eq!(lock(&up.mock).region_lookups, 1);
    id.issue_member_licence(t, "Globex", "zz", "u-2")
        .await
        .unwrap();
    let g = lock(&up.mock);
    assert_eq!(g.region_lookups, 2, "the miss re-reads exactly once");
    assert_eq!(g.issued_regions, [Some(fakes::REGION_EU.to_string()), None]);
}
