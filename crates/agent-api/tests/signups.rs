//! Early-access signup (D73, guide contracts C1–C6): intake guard, validation, de-dupe, queue ceiling,
//! Telegram (send-only, names in code entities, C1 buttons), the super_admin queue, the journaled approval
//! (tenant → licences → Keycloak user → membership → member licence → ONE email, last), the tick retry, and
//! the Telegram decision door (C3, idempotent). Real Postgres 18 (one container for this binary) and one
//! in-process fake for Keycloak / licences / Turnstile / Telegram.
use crate::common;

use std::time::Duration;

use axum::http::StatusCode;
use common::fakes::{self, Upstream, lock};
use common::{TestEnv, get, json, person, send, service, super_admin};
use serde_json::{Value, json};
use uuid::Uuid;

const INTAKE: &str = "/api/signup-approvals/intake";
const INVITE: &str = "/api/signup-approvals/invite";
const INTAKE_CLIENT: &str = "agent-signup-intake";
const DECIDER: &str = "chat-bridge-signup-decider";

fn intake_token() -> String {
    service(INTAKE_CLIENT, &[INTAKE_CLIENT], &[INTAKE_CLIENT])
}

fn decider_token() -> String {
    service(DECIDER, &[DECIDER], &["agent-signup-decide"])
}

async fn env_on() -> (TestEnv, Upstream) {
    env_on_with(|_| {}).await
}

async fn env_on_with(
    tweak: impl FnOnce(&mut std::collections::HashMap<String, String>),
) -> (TestEnv, Upstream) {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| {
        fakes::signup_env(&up, v);
        tweak(v);
    })
    .await;
    (e, up)
}

fn body(email: &str) -> Value {
    json!({"email": email, "name": "Ann Early", "company": "Globex", "source": "web",
           "message": "We run\n12 clinics", "locale": "de-DE"})
}

async fn submit(e: &TestEnv, email: &str) -> Uuid {
    let (s, b) = send(
        &e.app,
        json("POST", INTAKE, Some(&intake_token()), body(email)),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["status"], "queued");
    sqlx::query_scalar(
        "SELECT id FROM signup_approvals WHERE lower(email) = lower($1) AND status = 'pending'",
    )
    .bind(email)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

/// Detached Telegram sends land shortly after the response.
async fn telegram_count(up: &Upstream, want: usize) -> Vec<Value> {
    for _ in 0..50 {
        let t = lock(&up.mock).telegram.clone();
        if t.len() >= want {
            return t;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    lock(&up.mock).telegram.clone()
}

async fn steps(e: &TestEnv, id: Uuid) -> Vec<(String, String, Option<String>)> {
    sqlx::query_as(
        "SELECT step, status, code FROM signup_provisioning_steps WHERE signup_id = $1 ORDER BY ord",
    )
    .bind(id)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

/// Every step but `app_licences` is ok. That one (the tenant's bundle tokens) runs last and needs a licences bundle
/// and live upstreams; it is covered by tests/app_licences.rs, so it is not required to land here.
fn landed(st: &[(String, String, Option<String>)]) -> bool {
    st.iter()
        .filter(|(step, _, _)| step != "app_licences")
        .all(|(_, status, _)| status == "ok")
}

fn approve_uri(id: Uuid) -> String {
    format!("/api/signup-approvals/{id}/approve")
}

fn decision_uri(id: Uuid) -> String {
    format!("/api/signup-approvals/{id}/telegram-decision")
}

/// Every journal step fits the table's checks (0225): a step added to STEPS without widening `step` / `ord` fails every
/// approval with a 500.
#[tokio::test]
async fn the_journal_checks_admit_every_step_in_run_order() {
    let e = common::env().await;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO signup_approvals (email, name, source, intake_client) \
         VALUES ('steps@globex.com', 'Steps', 'web', 'agent-signup-intake') RETURNING id",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    for (i, step) in agent_api::signup::provisioning::STEPS.iter().enumerate() {
        sqlx::query(
            "INSERT INTO signup_provisioning_steps (signup_id, step, ord) VALUES ($1, $2, $3)",
        )
        .bind(id)
        .bind(step)
        .bind(i as i16 + 1)
        .execute(&e.owner)
        .await
        .unwrap_or_else(|err| panic!("step {step} (ord {}) refused: {err}", i + 1));
    }
}

#[tokio::test]
async fn dark_by_default_every_signup_route_is_the_unknown_route_404() {
    let e = common::env().await;
    for (m, uri) in [
        ("POST", INTAKE.to_string()),
        ("GET", "/api/signup-approvals".to_string()),
        ("POST", approve_uri(Uuid::new_v4())),
        ("POST", decision_uri(Uuid::new_v4())),
        ("POST", INVITE.to_string()),
    ] {
        let (s, b) = send(&e.app, json(m, &uri, Some(&super_admin()), json!({}))).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{m} {uri}");
        assert_eq!(b["error"]["code"], "not_found");
    }
}

#[tokio::test]
async fn intake_takes_only_the_intake_service_account() {
    let (e, _up) = env_on().await;
    let b = body("ann@globex.com");
    assert_eq!(
        send(&e.app, json("POST", INTAKE, None, b.clone())).await.0,
        StatusCode::UNAUTHORIZED
    );
    for (why, token) in [
        ("operator session", super_admin()),
        (
            "intake client with super_admin",
            service(
                INTAKE_CLIENT,
                &[INTAKE_CLIENT],
                &[INTAKE_CLIENT, "super_admin"],
            ),
        ),
        (
            "another app's intake client",
            service("cp-signup-intake", &["cp-signup-intake"], &[INTAKE_CLIENT]),
        ),
        (
            "aud without the azp",
            service(INTAKE_CLIENT, &["agent"], &[INTAKE_CLIENT]),
        ),
        ("no role", service(INTAKE_CLIENT, &[INTAKE_CLIENT], &[])),
        ("the decider", decider_token()),
    ] {
        let (s, r) = send(&e.app, json("POST", INTAKE, Some(&token), b.clone())).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{why}: {r}");
        assert_eq!(r["error"]["code"], "signup_intake_forbidden", "{why}");
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM signup_approvals")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn intake_validates_normalises_and_queues_once() {
    let (e, up) = env_on().await;
    for bad in [
        json!({"email": "nope", "name": "A", "source": "web"}),
        json!({"email": "a@b.co", "name": "\u{200B} ", "source": "web"}),
        json!({"email": "a@b.co", "name": "A", "source": "fax"}),
        json!({"email": "a@b.co", "name": "A", "source": "web", "extra": 1}),
        json!({"email": "a@b.co", "name": "A", "source": "web", "message": "x".repeat(2001)}),
    ] {
        let (s, r) = send(
            &e.app,
            json("POST", INTAKE, Some(&intake_token()), bad.clone()),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {r}");
    }
    let id = submit(&e, "Ann@Globex.com").await;
    let (locale, name, message): (Option<String>, String, Option<String>) =
        sqlx::query_as("SELECT locale, name, message FROM signup_approvals WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(locale.as_deref(), Some("de"), "de-DE → de (C6)");
    assert_eq!(name, "Ann Early");
    assert_eq!(message.as_deref(), Some("We run\n12 clinics"));
    // the same address again (any case) while pending: no second row
    let (s, r) = send(
        &e.app,
        json(
            "POST",
            INTAKE,
            Some(&intake_token()),
            body("ann@GLOBEX.com"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_eq!(r["status"], "already_pending");
    // an unknown language is "not told", never a refusal
    let mut b = body("bob@initech.com");
    b["locale"] = json!("it");
    let (s, _) = send(&e.app, json("POST", INTAKE, Some(&intake_token()), b)).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let l: Option<String> =
        sqlx::query_scalar("SELECT locale FROM signup_approvals WHERE email = 'bob@initech.com'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(l, None);

    // C1: one message per NEW signup, names in code entities, callback buttons + a URL button
    let t = telegram_count(&up, 2).await;
    assert_eq!(t.len(), 2, "the de-duplicated re-submit sent nothing");
    let m = &t[0];
    assert_eq!(m["chat_id"], "-1003934173351");
    assert_eq!(m["message_thread_id"], 4242);
    assert!(m.get("parse_mode").is_none());
    let text = m["text"].as_str().unwrap();
    assert!(
        text.contains("Ann Early") && text.contains("Ann@Globex.com") && text.contains("Globex")
    );
    assert!(text.contains("Language: de"));
    assert!(text.contains("Message: We run 12 clinics"), "{text}");
    let entities = m["entities"].as_array().unwrap();
    assert!(
        entities.len() >= 4,
        "name, email, company, message are code entities"
    );
    let kb = &m["reply_markup"]["inline_keyboard"];
    assert_eq!(kb[0][0]["callback_data"], format!("ea1:agent:a:{id}"));
    assert_eq!(kb[0][1]["callback_data"], format!("ea1:agent:r:{id}"));
    assert_eq!(
        kb[1][0]["url"],
        format!("https://agent.test/platform/signups?signup={id}")
    );
}

#[tokio::test]
async fn a_full_queue_refuses_new_signups() {
    let (e, _up) = env_on_with(|v| {
        v.insert("SIGNUP_QUEUE_MAX_PENDING".into(), "1".into());
    })
    .await;
    submit(&e, "one@acme.com").await;
    let (s, b) = send(
        &e.app,
        json("POST", INTAKE, Some(&intake_token()), body("two@acme.com")),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(b["error"]["code"], "signup_queue_full");
}

#[tokio::test]
async fn a_relayed_turnstile_token_is_verified_when_a_secret_is_set() {
    let (e, up) = env_on_with(|v| {
        v.insert("TURNSTILE_SECRET".into(), "ts-secret".into());
    })
    .await;
    let mut b = body("ann@globex.com");
    b["turnstileToken"] = json!("bad-token");
    let (s, r) = send(
        &e.app,
        json("POST", INTAKE, Some(&intake_token()), b.clone()),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(r["error"]["code"], "turnstile_failed");
    b["turnstileToken"] = json!("good-token");
    assert_eq!(
        send(&e.app, json("POST", INTAKE, Some(&intake_token()), b))
            .await
            .0,
        StatusCode::ACCEPTED
    );
    assert_eq!(lock(&up.mock).verified, ["bad-token", "good-token"]);
}

#[tokio::test]
async fn the_queue_is_super_admin_only_and_suggests_a_name() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@mail.acme.co.uk").await;
    let (s, b) = send(
        &e.app,
        get("/api/signup-approvals", Some(&person("u-x", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(
        send(&e.app, get("/api/signup-approvals", Some(&intake_token())))
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (s, b) = send(
        &e.app,
        get("/api/signup-approvals?status=pending", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let item = &b["items"][0];
    assert_eq!(item["id"], id.to_string());
    assert_eq!(item["suggested_tenant_name"], "Globex");
    assert_eq!(item["locale"], "de");
    assert_eq!(item["steps"], json!([]));
    assert_eq!(b["pending"], 1);
    assert_eq!(b["queue_cap"], 500);
    assert_eq!(b["counts"]["received"], 1);
    let (s, b) = send(
        &e.app,
        get("/api/signup-approvals?status=bogus", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}

#[tokio::test]
async fn approve_provisions_every_step_and_sends_one_email_last() {
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"tenantName": "Globex Health", "residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["tenant"]["name"], "Globex Health");
    let tenant: Uuid = b["tenant"]["id"].as_str().unwrap().parse().unwrap();
    assert_eq!(b["signup"]["status"], "approved");
    assert_eq!(b["signup"]["tenant_id"], tenant.to_string());
    let st = steps(&e, id).await;
    // `app_licences` (the tenant's bundle tokens) runs last and needs a licences bundle and live upstreams; it is
    // covered by tests/app_licences.rs, so it is not required to land here
    assert!(
        st.iter()
            .filter(|(step, _, _)| step != "app_licences")
            .all(|(_, status, _)| status == "ok"),
        "every step ok: {st:?}"
    );
    assert_eq!(st.len(), 7);

    let sub = {
        let g = lock(&up.mock);
        assert_eq!(g.registered, [tenant.to_string()]);
        assert_eq!(
            g.created.len(),
            1,
            "the identity was created by the approval"
        );
        let sub = g.created[0].clone();
        // both name parts, so the portal profile is complete and first login asks only for a password
        assert_eq!(g.created_reps[0]["firstName"], "Ann");
        assert_eq!(g.created_reps[0]["lastName"], "Early");
        // the tenant's own licence (licences_tenant, D77 L4), then the requester's member licence
        assert_eq!(
            g.issued,
            [
                (tenant.to_string(), format!("agent-tenant:{tenant}:a")),
                (tenant.to_string(), sub.clone())
            ]
        );
        assert_eq!(g.minted, 2);
        // attributes before the email: locale + signup_app
        let attrs = &g.users.iter().find(|u| u.0 == sub).unwrap().2;
        assert_eq!(attrs["locale"], json!(["de"]));
        assert_eq!(attrs["signup_app"], json!(["agent"]));
        // exactly one email, to the app with the app's client
        assert_eq!(g.emails.len(), 1);
        let (who, q, actions) = &g.emails[0];
        assert_eq!(*who, sub);
        assert!(q.contains("client_id=agent"), "{q}");
        assert!(
            q.contains("redirect_uri=https%3A%2F%2Fagent.test%2F"),
            "{q}"
        );
        assert_eq!(*actions, json!(["UPDATE_PASSWORD", "VERIFY_EMAIL"]));
        sub
    };

    // the requester is the tenant_admin and signs straight in
    let (s, me) = send(&e.app, get("/api/me", Some(&person(&sub, &[])))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(me["tenant"]["id"], tenant.to_string());
    assert_eq!(
        me["roles"],
        json!(common::ALL_ROLES),
        "first member: every role (D88)"
    );
    let sealed: String = sqlx::query_scalar(
        "SELECT token_encrypted FROM member_entitlement WHERE tenant_id = $1 AND status = 'active'",
    )
    .bind(tenant)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(
        sealed.starts_with("v2:v1:") && !sealed.contains("tok-"),
        "sealed, not clear"
    );

    // a second decision is a 409, and nothing is created twice
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(b["error"]["code"], "signup_not_pending");
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{id}/reject"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(lock(&up.mock).emails.len(), 1);

    // the decision is announced
    let t = telegram_count(&up, 2).await;
    let d = t.last().unwrap()["text"].as_str().unwrap().to_string();
    assert!(d.starts_with("Agent signup approved"), "{d}");
    // every step ok but `app_licences`, which cannot land here (tests/app_licences.rs); the email outcome shows either way
    let outstanding: Vec<&str> = d.lines().filter(|l| l.starts_with("  - ")).collect();
    assert!(
        outstanding
            .iter()
            .all(|l| l.starts_with("  - app_licences:")),
        "{d}"
    );
    assert!(
        d.contains("all steps ok; set-password email sent")
            || d.contains("Email: set-password email sent."),
        "{d}"
    );
}

#[tokio::test]
async fn approval_rules_for_names_and_unknown_ids() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"tenantName": "x"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(Uuid::new_v4()),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["code"], "signup_not_found");
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            "/api/signup-approvals/nope/approve",
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // a member of some tenant is not a super_admin
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&person("u-x", &[])),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_email_waits_for_the_licence_and_the_tick_finishes_the_job() {
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    lock(&up.mock).licences_down = true;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(
        s,
        StatusCode::OK,
        "approval is recorded even when licences is down: {b}"
    );
    assert_eq!(b["signup"]["status"], "approved", "{b}");
    assert_eq!(b["seated"], true, "{b}");
    let st = steps(&e, id).await;
    // identity, workspace and seat landed (they gate the decision); the licence and email wait for licences
    assert!(st[..3].iter().all(|(_, s, _)| s == "ok"), "{st:?}");
    assert_eq!(
        st[3],
        (
            "licences_tenant".into(),
            "pending".into(),
            Some("licences_unavailable".into())
        )
    );
    assert!(st[4..].iter().all(|(_, s, _)| s == "pending"), "{st:?}");
    assert!(
        lock(&up.mock).emails.is_empty(),
        "no email before the licence"
    );

    // licences recovers; the tick (backoff forced due) finishes every step and sends the one email
    lock(&up.mock).licences_down = false;
    sqlx::query(
        "UPDATE signup_provisioning_steps SET next_attempt_at = now() WHERE signup_id = $1",
    )
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
    let sg = e.state.signup.clone().unwrap();
    let ran = agent_api::signup::provisioning::tick(&e.state, &sg)
        .await
        .unwrap();
    assert_eq!(ran, 1);
    assert!(landed(&steps(&e, id).await));
    assert_eq!(lock(&up.mock).emails.len(), 1);
    // nothing due any more: a second tick does nothing
    assert_eq!(
        agent_api::signup::provisioning::tick(&e.state, &sg)
            .await
            .unwrap(),
        0
    );
    assert_eq!(lock(&up.mock).emails.len(), 1);
}

#[tokio::test]
async fn a_failed_email_is_retried_without_redoing_the_licence() {
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    lock(&up.mock).email_down = true;
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let st = steps(&e, id).await;
    assert_eq!(
        st[5],
        (
            "invite_email".into(),
            "pending".into(),
            Some("invite_email_failed".into())
        )
    );
    lock(&up.mock).email_down = false;
    sqlx::query(
        "UPDATE signup_provisioning_steps SET next_attempt_at = now() WHERE signup_id = $1",
    )
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
    let sg = e.state.signup.clone().unwrap();
    agent_api::signup::provisioning::tick(&e.state, &sg)
        .await
        .unwrap();
    let g = lock(&up.mock);
    assert_eq!(g.emails.len(), 1);
    assert_eq!(
        g.issued
            .iter()
            .filter(|(_, holder)| !holder.starts_with("agent-tenant:"))
            .count(),
        1,
        "the member licence was not created twice"
    );
    assert_eq!(g.minted, 2, "tenant + member, once each");
}

/// (status, code, message) of one step.
async fn step(e: &TestEnv, id: Uuid, name: &str) -> (String, Option<String>, Option<String>) {
    sqlx::query_as(
        "SELECT status, code, message FROM signup_provisioning_steps WHERE signup_id = $1 AND step = $2",
    )
    .bind(id)
    .bind(name)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

/// A portal identity for `email` that existed before the signup.
fn existing(up: &Upstream, id: &str, email: &str, password: bool, verified: bool) {
    let mut g = lock(&up.mock);
    g.users.push((id.into(), email.into(), Default::default()));
    if password {
        g.with_password.push(id.into());
    }
    if verified {
        g.email_verified.push(id.into());
    }
}

/// The requester's pending tenant_admin invite in `tenant`: (state, roles, invitee_sub).
async fn member_invite(e: &TestEnv, tenant: Uuid) -> Option<(String, Vec<String>, Option<String>)> {
    sqlx::query_as("SELECT state, roles, invitee_sub FROM member_invites WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_optional(&e.owner)
        .await
        .unwrap()
}

/// SECURITY (CP invite-service `seatMintedIdentity`): a portal account that existed before the approval — whose
/// email may never have been verified, e.g. registered through another app — is NEVER seated as tenant_admin and
/// never licensed at approval. The approval founds the workspace and records a pending tenant_admin invite; no
/// email is sent. Holds whatever the account's password or verification state.
#[tokio::test]
async fn a_pre_existing_identity_is_never_seated_or_licensed_at_approval() {
    for (password, verified) in [(true, true), (true, false), (false, false), (false, true)] {
        let (e, up) = env_on().await;
        existing(&up, "kc-existing", "ann@globex.com", password, verified);
        let id = submit(&e, "ann@globex.com").await;
        let (s, b) = send(
            &e.app,
            json(
                "POST",
                &approve_uri(id),
                Some(&super_admin()),
                json!({"residency": "eu"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["signup"]["status"], "approved");
        assert_eq!(b["seated"], false, "{b}");
        let tenant: Uuid = b["tenant"]["id"].as_str().unwrap().parse().unwrap();
        // no member row: the account has no home and is not let in
        let members: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(&e.owner)
            .await
            .unwrap();
        assert_eq!(members, 0, "password={password} verified={verified}");
        let (s, me) = send(&e.app, get("/api/me", Some(&person("kc-existing", &[])))).await;
        assert!(
            s == StatusCode::FORBIDDEN || me["tenant"].is_null(),
            "not provisioned: {s} {me}"
        );
        // no member licence, no entitlement, no email; the tenant licence only
        let ents: i64 =
            sqlx::query_scalar("SELECT count(*) FROM member_entitlement WHERE tenant_id = $1")
                .bind(tenant)
                .fetch_one(&e.owner)
                .await
                .unwrap();
        assert_eq!(ents, 0);
        {
            let g = lock(&up.mock);
            assert!(g.created.is_empty(), "no identity created");
            assert!(
                g.issued.iter().all(|(_, h)| h.starts_with("agent-tenant:")),
                "{:?}",
                g.issued
            );
            assert!(g.emails.is_empty(), "no email at approval");
            let attrs = &g.users.iter().find(|u| u.0 == "kc-existing").unwrap().2;
            assert!(attrs.is_empty(), "its attributes are left alone: {attrs:?}");
        }
        // the seat waits in a pending invite (accepted with a verified email, B1b)
        let (state, roles, invitee) = member_invite(&e, tenant).await.expect("invite recorded");
        assert_eq!(state, "pending");
        assert_eq!(roles, common::ALL_ROLES.map(String::from).to_vec());
        assert_eq!(invitee.as_deref(), Some("kc-existing"));
        for name in ["membership", "member_licence", "invite_email"] {
            let (status, code, message) = step(&e, id, name).await;
            assert_eq!(status, "skipped", "{name}");
            assert_eq!(code.as_deref(), Some("existing_identity_invite_pending"));
            assert!(!message.unwrap().contains("globex"), "no personal data");
        }
        // Telegram says so
        let t = telegram_count(&up, 2).await;
        let d = t.last().unwrap()["text"].as_str().unwrap().to_string();
        assert!(d.starts_with("Agent signup approved"), "{d}");
        assert!(d.contains("NOT seated") && d.contains("NO email"), "{d}");
    }
}

/// Control for the rule above: an identity the approval itself created IS seated, licensed and emailed.
#[tokio::test]
async fn only_an_identity_minted_by_the_approval_is_seated() {
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    let tenant = approve_ok(&e, id, "eu").await;
    let sub = lock(&up.mock).created[0].clone();
    let home: Uuid = sqlx::query_scalar("SELECT tenant_id FROM users WHERE sub = $1")
        .bind(&sub)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(home, tenant);
    assert!(member_invite(&e, tenant).await.is_none());
    assert_eq!(step(&e, id, "membership").await.0, "ok");
}

/// The existing-identity path reads nothing from Keycloak's credentials: an unreadable credential list cannot
/// change the outcome.
#[tokio::test]
async fn unreadable_credentials_do_not_change_an_existing_identity_approval() {
    let (e, up) = env_on().await;
    existing(&up, "kc-existing", "ann@globex.com", true, true);
    lock(&up.mock).credentials_forbidden = true;
    let id = submit(&e, "ann@globex.com").await;
    let tenant = approve_ok(&e, id, "eu").await;
    assert_eq!(
        member_invite(&e, tenant).await.map(|i| i.0).as_deref(),
        Some("pending")
    );
    assert!(lock(&up.mock).emails.is_empty());
}

#[tokio::test]
async fn telegram_approval_of_an_existing_identity_says_it_was_not_seated() {
    let (e, up) = env_on().await;
    existing(&up, "kc-existing", "ann@globex.com", true, true);
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            json!({"action": "approve", "telegramUserId": 7}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "approved");
    let summary = b["summary"].as_str().unwrap();
    assert!(summary.chars().count() <= 200);
    assert!(
        summary.starts_with("Approved: ann@globex.com -> workspace \"Globex\""),
        "{summary}"
    );
    assert!(summary.contains("NOT seated"), "{summary}");
    assert!(lock(&up.mock).emails.is_empty());
}

#[tokio::test]
async fn a_transport_failure_persists_no_personal_data() {
    // Keycloak's admin API unreachable: the lookup URL carries ?email=<requester>
    let (e, up) = env_on_with(|v| {
        v.insert(
            "KEYCLOAK_ADMIN_API_URL".into(),
            "http://127.0.0.1:1/admin/realms/portal".into(),
        );
    })
    .await;
    let id = submit(&e, "ann.early@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    // CP order: the identity gates the decision; nothing is decided or founded while Keycloak is down
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
    assert_eq!(b["error"]["code"], "keycloak_unavailable");
    assert!(!b.to_string().to_lowercase().contains("globex"), "{b}");
    let status: String = sqlx::query_scalar("SELECT status FROM signup_approvals WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(status, "pending");
    let tenants: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(tenants, 0, "no workspace before the identity");
    let (status, code, message) = step(&e, id, "keycloak_user").await;
    assert_eq!(
        (status.as_str(), code.as_deref()),
        ("pending", Some("keycloak_unavailable"))
    );
    let message = message.expect("a transport failure is described");
    assert!(message.contains("could not connect"), "{message}");
    let messages: Vec<Option<String>> =
        sqlx::query_scalar("SELECT message FROM signup_provisioning_steps WHERE signup_id = $1")
            .bind(id)
            .fetch_all(&e.owner)
            .await
            .unwrap();
    for m in messages.into_iter().flatten() {
        let l = m.to_lowercase();
        assert!(
            !l.contains("ann") && !l.contains("globex") && !l.contains("%40") && !l.contains('?'),
            "personal data in a step message: {m}"
        );
    }
    // the queue API returns the same journal
    let (_, q) = send(&e.app, get("/api/signup-approvals", Some(&super_admin()))).await;
    let steps = q["items"][0]["steps"].to_string().to_lowercase();
    assert!(
        !steps.contains("globex") && !steps.contains("%40"),
        "{steps}"
    );
    assert!(lock(&up.mock).emails.is_empty());
}

#[tokio::test]
async fn a_telegram_reject_that_cannot_be_recorded_is_c3_failed() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    // every update of a signup row fails
    for sql in [
        "CREATE FUNCTION test_refuse() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'refused'; END $$",
        "CREATE TRIGGER test_refuse BEFORE UPDATE ON signup_approvals FOR EACH ROW EXECUTE FUNCTION test_refuse()",
    ] {
        sqlx::query(sql).execute(&e.owner).await.unwrap();
    }
    let tap = json!({"action": "reject", "telegramUserId": 7});
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            tap.clone(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
    assert_eq!(
        b["status"], "failed",
        "C3 body, not the error envelope: {b}"
    );
    assert!(b.get("error").is_none(), "{b}");
    assert!(b["summary"].as_str().unwrap().chars().count() <= 200);
    let status: String = sqlx::query_scalar("SELECT status FROM signup_approvals WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(status, "pending", "nothing was decided");
    // once the database recovers the same tap decides it
    sqlx::query("DROP TRIGGER test_refuse ON signup_approvals")
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        json("POST", &decision_uri(id), Some(&decider_token()), tap),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "rejected");
}

#[tokio::test]
async fn the_decider_role_counts_only_as_a_realm_role() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    // agent-signup-decide held only as a CLIENT role (C3 requires the realm role)
    let client_only = common::sign(&json!({
        "sub": format!("service-account-{DECIDER}"), "exp": common::in_five_minutes(),
        "iss": common::TEST_ISSUER, "aud": [DECIDER], "azp": DECIDER,
        "realm_access": {"roles": []},
        "resource_access": {DECIDER: {"roles": ["agent-signup-decide"]}}
    }));
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&client_only),
            json!({"action": "approve", "telegramUserId": 7}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "signup_decider_forbidden");
    // likewise the intake role as a client role
    let intake_client_only = common::sign(&json!({
        "sub": format!("service-account-{INTAKE_CLIENT}"), "exp": common::in_five_minutes(),
        "iss": common::TEST_ISSUER, "aud": [INTAKE_CLIENT], "azp": INTAKE_CLIENT,
        "resource_access": {INTAKE_CLIENT: {"roles": [INTAKE_CLIENT]}}
    }));
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            INTAKE,
            Some(&intake_client_only),
            body("bob@initech.com"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "signup_intake_forbidden");
    // control: the realm-role token is accepted
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            json!({"action": "reject", "telegramUserId": 7}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// CP `assertApprovalAdmitsUser`: an approval never moves a member out of their workspace. Refused 409 BEFORE
/// anything is created, the row stays pending, and nobody is emailed.
#[tokio::test]
async fn a_requester_in_another_workspace_is_refused_before_anything_is_created() {
    // by the requester's portal identity (the member row has no email)
    let (e, up) = env_on().await;
    lock(&up.mock).users.push((
        "kc-member".into(),
        "ann@globex.com".into(),
        Default::default(),
    ));
    let other = Uuid::new_v4();
    common::insert_tenant(&e.owner, other, "Elsewhere").await;
    common::insert_user(&e.owner, "kc-member", other, "tenant_user").await;
    let id = submit(&e, "ann@globex.com").await;
    refused_elsewhere(&e, &up, id).await;

    // by the address alone (a member row carrying it, whatever the identity)
    let (e, up) = env_on().await;
    let other = Uuid::new_v4();
    common::insert_tenant(&e.owner, other, "Elsewhere").await;
    common::insert_user(&e.owner, "kc-someone", other, "tenant_admin").await;
    sqlx::query("UPDATE users SET email = 'Ann@Globex.com' WHERE sub = 'kc-someone'")
        .execute(&e.owner)
        .await
        .unwrap();
    let id = submit(&e, "ann@globex.com").await;
    refused_elsewhere(&e, &up, id).await;
    assert!(lock(&up.mock).created.is_empty(), "no identity was created");
}

async fn refused_elsewhere(e: &TestEnv, up: &Upstream, id: Uuid) {
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "user_belongs_to_another_workspace");
    let status: String = sqlx::query_scalar("SELECT status FROM signup_approvals WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(status, "pending", "nothing was decided");
    let tenants: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(tenants, 1, "no orphan workspace (only the other one)");
    assert!(lock(&up.mock).emails.is_empty());
    assert!(lock(&up.mock).issued.is_empty(), "no licence");
}

/// Review #6: a requester who is a DISABLED member of another workspace is refused at once (one attempt), never
/// founded a workspace for.
#[tokio::test]
async fn a_member_disabled_elsewhere_is_refused_at_once() {
    let (e, up) = env_on().await;
    lock(&up.mock).users.push((
        "kc-member".into(),
        "ann@globex.com".into(),
        Default::default(),
    ));
    let other = Uuid::new_v4();
    common::insert_tenant(&e.owner, other, "Elsewhere").await;
    common::insert_user(&e.owner, "kc-member", other, "tenant_user").await;
    sqlx::query("UPDATE users SET disabled_at = now() WHERE sub = 'kc-member'")
        .execute(&e.owner)
        .await
        .unwrap();
    let id = submit(&e, "ann@globex.com").await;
    refused_elsewhere(&e, &up, id).await;
    let (status, code, attempts): (String, Option<String>, i32) = sqlx::query_as(
        "SELECT status, code, attempts FROM signup_provisioning_steps \
          WHERE signup_id = $1 AND step = 'keycloak_user'",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), code.as_deref(), attempts),
        ("failed", Some("user_belongs_to_another_workspace"), 1)
    );
}

/// Review #6, at the seat itself: the identity THIS approval minted turns out to be a DISABLED member of another
/// workspace by the time the `membership` step runs (`agent_auth_user` hides it, the insert conflicts on `sub`).
/// That refusal fails identically every time, so the step is `failed` after ONE attempt (no backoff) and the
/// approve answers 409 `user_in_other_tenant`.
#[tokio::test]
async fn a_membership_step_for_a_sub_disabled_elsewhere_fails_after_one_attempt() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    // First approve: identity minted and workspace founded, the seat cannot land (stays pending).
    refuse_seats(&e).await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
    allow_seats(&e).await;
    assert_eq!(step(&e, id, "keycloak_user").await.0, "ok");
    assert_eq!(step(&e, id, "tenant").await.0, "ok");
    let sub: String = sqlx::query_scalar(
        "SELECT data->>'sub' FROM signup_provisioning_steps WHERE signup_id = $1 AND step = 'keycloak_user'",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap();

    // Meanwhile the minted sub became a member of another workspace there, and was disabled (no email on the row,
    // so the approve's address pre-check does not see it).
    let other = Uuid::new_v4();
    common::insert_tenant(&e.owner, other, "Elsewhere").await;
    common::insert_user(&e.owner, &sub, other, "tenant_user").await;
    sqlx::query("UPDATE users SET disabled_at = now() WHERE sub = $1")
        .bind(&sub)
        .execute(&e.owner)
        .await
        .unwrap();

    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "user_in_other_tenant");
    let (status, code, attempts, next_due): (String, Option<String>, i32, bool) = sqlx::query_as(
        "SELECT status, code, attempts, next_attempt_at IS NOT NULL FROM signup_provisioning_steps \
          WHERE signup_id = $1 AND step = 'membership'",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        (status.as_str(), code.as_deref(), attempts, next_due),
        ("failed", Some("user_in_other_tenant"), 1, false),
        "terminal at once, no backoff"
    );
    assert_eq!(signup_status(&e, id).await, "pending", "nothing decided");
    let home: Uuid = sqlx::query_scalar("SELECT tenant_id FROM users WHERE sub = $1")
        .bind(&sub)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(home, other, "the member was not moved");
}

#[tokio::test]
async fn concurrent_approvals_create_one_tenant() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    let reqs = (0..4).map(|_| {
        let app = e.app.clone();
        tokio::spawn(async move {
            send(
                &app,
                json(
                    "POST",
                    &approve_uri(id),
                    Some(&super_admin()),
                    json!({"residency": "eu"}),
                ),
            )
            .await
            .0
        })
    });
    let mut ok = 0;
    for r in reqs.collect::<Vec<_>>() {
        match r.await.unwrap() {
            StatusCode::OK => ok += 1,
            StatusCode::CONFLICT => {}
            other => panic!("unexpected {other}"),
        }
    }
    assert_eq!(ok, 1);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn reject_creates_nothing() {
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{id}/reject"),
            Some(&super_admin()),
            json!({"reason": "test"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "rejected");
    assert_eq!(b["reason"], "test");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert!(lock(&up.mock).created.is_empty());
}

#[tokio::test]
async fn telegram_decision_door_is_c3_and_idempotent() {
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    let tap = |action: &str| json!({"action": action, "telegramUserId": 4_242_424});

    // only chat-bridge's decider token, never the intake SA or an operator session
    for (why, token) in [
        ("intake SA", intake_token()),
        ("operator", super_admin()),
        (
            "decider with super_admin",
            service(DECIDER, &[DECIDER], &["agent-signup-decide", "super_admin"]),
        ),
        (
            "aud without azp",
            service(DECIDER, &["agent"], &["agent-signup-decide"]),
        ),
        (
            "cp role",
            service(DECIDER, &[DECIDER], &["cp-signup-decide"]),
        ),
    ] {
        let (s, b) = send(
            &e.app,
            json("POST", &decision_uri(id), Some(&token), tap("approve")),
        )
        .await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{why}: {b}");
        assert_eq!(b["error"]["code"], "signup_decider_forbidden");
    }
    // strict body
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            json!({"action": "approve"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            json!({"action": "maybe", "telegramUserId": 1}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            tap("approve"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "approved");
    let tenant = b["tenantId"].as_str().unwrap().to_string();
    assert!(b["summary"].as_str().unwrap().chars().count() <= 200);
    let (by, via): (String, String) =
        sqlx::query_as("SELECT decided_by, decided_via FROM signup_approvals WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        (by.as_str(), via.as_str()),
        ("telegram:4242424", "telegram")
    );
    assert_eq!(lock(&up.mock).emails.len(), 1);

    // a second tap (either button) is already_decided and does nothing
    for action in ["approve", "reject"] {
        let (s, b) = send(
            &e.app,
            json(
                "POST",
                &decision_uri(id),
                Some(&decider_token()),
                tap(action),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["status"], "already_decided");
        assert_eq!(b["outcome"], "approved", "CP reply shape: {b}");
        assert_eq!(b["tenantId"], tenant);
        assert!(
            b["summary"].as_str().unwrap().contains("ann@globex.com"),
            "{b}"
        );
    }
    assert_eq!(lock(&up.mock).emails.len(), 1);

    // reject by tap
    let id2 = submit(&e, "bob@initech.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id2),
            Some(&decider_token()),
            tap("reject"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["status"], "rejected");
    assert_eq!(b["summary"], "Rejected: bob@initech.com.");
    // a decided-rejected row: outcome rejected, never a tenantId
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id2),
            Some(&decider_token()),
            tap("approve"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b["status"], "already_decided");
    assert_eq!(b["outcome"], "rejected");
    assert!(b.get("tenantId").is_none(), "{b}");
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(Uuid::new_v4()),
            Some(&decider_token()),
            tap("reject"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["code"], "signup_not_found");
}

// ---- CP convergence: approve order, claim, auto-suffix, reject repair, C3 shape, invite ----------------------------

async fn signup_status(e: &TestEnv, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM signup_approvals WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn tenant_count(e: &TestEnv) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

/// Every insert into `users` fails (the seat step cannot land) until [`allow_seats`].
async fn refuse_seats(e: &TestEnv) {
    for sql in [
        "CREATE OR REPLACE FUNCTION test_no_seat() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'no seat'; END $$",
        "CREATE TRIGGER test_no_seat BEFORE INSERT ON users FOR EACH ROW EXECUTE FUNCTION test_no_seat()",
    ] {
        sqlx::query(sql).execute(&e.owner).await.unwrap();
    }
}

async fn allow_seats(e: &TestEnv) {
    sqlx::query("DROP TRIGGER test_no_seat ON users")
        .execute(&e.owner)
        .await
        .unwrap();
}

/// CP order: the signup is marked approved LAST. A seat that cannot land leaves it pending (502 with the step code
/// and the journal), and the next approve resumes INTO the workspace the first one founded — never a second one.
#[tokio::test]
async fn a_failed_approve_stays_pending_and_the_next_one_resumes_the_same_workspace() {
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    refuse_seats(&e).await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
    assert_eq!(b["error"]["code"], "internal");
    let journal = b["error"]["steps"].as_array().unwrap().clone();
    assert_eq!(journal.len(), 7, "{b}");
    assert_eq!(signup_status(&e, id).await, "pending", "nothing decided");
    assert_eq!(
        tenant_count(&e).await,
        1,
        "the founded workspace is kept for the resume"
    );
    let (approving, decided): (bool, Option<String>) = sqlx::query_as(
        "SELECT approving_claim IS NOT NULL, decided_via FROM signup_approvals WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(!approving, "the claim is released");
    assert_eq!(decided, None);
    assert!(lock(&up.mock).emails.is_empty(), "no email before the seat");
    // the tick never runs a pending signup's steps (CP decision gate)
    force_due(&e, id).await;
    let sg = e.state.signup.clone().unwrap();
    assert_eq!(
        agent_api::signup::provisioning::tick(&e.state, &sg)
            .await
            .unwrap(),
        0
    );
    assert!(lock(&up.mock).emails.is_empty());

    allow_seats(&e).await;
    let tenant = approve_ok(&e, id, "eu").await;
    assert_eq!(tenant_count(&e).await, 1, "resumed, not founded twice");
    assert_eq!(signup_status(&e, id).await, "approved");
    assert!(landed(&steps(&e, id).await));
    {
        let g = lock(&up.mock);
        assert_eq!(g.created.len(), 1, "one identity");
        assert_eq!(g.emails.len(), 1, "one email, after the seat");
    }
    let home: Uuid = sqlx::query_scalar("SELECT tenant_id FROM users")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(home, tenant);
    assert_eq!(platform_rows(&e, "signup.approve").await.len(), 1);
}

/// CP reject repair: rejecting a signup whose approve aborted offboards the workspace that approve founded.
#[tokio::test]
async fn reject_after_an_aborted_approve_offboards_its_workspace() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    refuse_seats(&e).await;
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    allow_seats(&e).await;
    let founded: Uuid = sqlx::query_scalar("SELECT id FROM tenants")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            json!({"action": "reject", "telegramUserId": 7}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "rejected");
    assert_eq!(b["cleanup"], "done");
    assert!(b["summary"].as_str().unwrap().contains("offboarded"), "{b}");
    let status: String = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(founded)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(status, "archived", "no orphan workspace");
    assert_eq!(signup_status(&e, id).await, "rejected");
}

/// One approver at a time (CP claim): while a claim is live, a reject and a second approve are refused 409
/// `approval_in_progress` and a Telegram tap answers `already_decided` / `in_progress`.
#[tokio::test]
async fn a_live_approve_claim_refuses_other_deciders() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    sqlx::query(
        "UPDATE signup_approvals SET approving_until = now() + interval '5 minutes', \
                approving_claim = gen_random_uuid() WHERE id = $1",
    )
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
    for (uri, body) in [
        (approve_uri(id), json!({"residency": "eu"})),
        (format!("/api/signup-approvals/{id}/reject"), json!({})),
    ] {
        let (s, b) = send(&e.app, json("POST", &uri, Some(&super_admin()), body)).await;
        assert_eq!(s, StatusCode::CONFLICT, "{uri}: {b}");
        assert_eq!(b["error"]["code"], "approval_in_progress");
    }
    for action in ["approve", "reject"] {
        let (s, b) = send(
            &e.app,
            json(
                "POST",
                &decision_uri(id),
                Some(&decider_token()),
                json!({"action": action, "telegramUserId": 7}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["status"], "already_decided");
        assert_eq!(b["outcome"], "in_progress");
    }
    // the queue shows it
    let (_, q) = send(&e.app, get("/api/signup-approvals", Some(&super_admin()))).await;
    assert_eq!(q["items"][0]["approving"], true);
    assert_eq!(tenant_count(&e).await, 0);
    // a lapsed claim is taken over
    sqlx::query(
        "UPDATE signup_approvals SET approving_until = now() - interval '1 second' WHERE id = $1",
    )
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
    approve_ok(&e, id, "eu").await;
}

/// CP auto-suffix (signup approvals only): a taken workspace name becomes `<name>-2` … `<name>-10`, so a Telegram
/// approval never bounces on a name; past `-10` it is 409 / C3 502 `tenant_name_taken` and nothing is decided.
#[tokio::test]
async fn a_taken_workspace_name_is_suffixed_up_to_ten() {
    let (e, _up) = env_on().await;
    common::insert_tenant(&e.owner, Uuid::new_v4(), "Globex").await;
    common::insert_tenant(&e.owner, Uuid::new_v4(), "globex-2").await;
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            json!({"action": "approve", "telegramUserId": 7}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "approved");
    assert!(
        b["summary"]
            .as_str()
            .unwrap()
            .starts_with("Approved: ann@globex.com -> workspace \"Globex-3\";"),
        "{b}"
    );
    let tenant: Uuid = b["tenantId"].as_str().unwrap().parse().unwrap();
    let name: String = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(name, "Globex-3");
    let approve = platform_rows(&e, "signup.approve").await;
    assert_eq!(approve.len(), 1);

    // every variant taken
    for n in 4..=10 {
        common::insert_tenant(&e.owner, Uuid::new_v4(), &format!("Globex-{n}")).await;
    }
    let id = submit(&e, "bob@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            json!({"action": "approve", "telegramUserId": 7}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
    assert_eq!(b["status"], "failed");
    assert_eq!(b["code"], "tenant_name_taken");
    assert!(b["summary"].as_str().unwrap().chars().count() <= 200);
    assert_eq!(signup_status(&e, id).await, "pending");
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "tenant_name_taken");
    // a free name approves it, resuming the same journal
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu", "tenantName": "Globex Labs"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["tenant"]["name"], "Globex Labs");
}

#[test]
fn name_candidates_fit_120_characters() {
    use agent_api::signup::provisioning::name_candidates;
    let c = name_candidates("Acme");
    assert_eq!(c.len(), 10);
    assert_eq!(c[0], "Acme");
    assert_eq!(c[1], "Acme-2");
    assert_eq!(c[9], "Acme-10");
    let long = "x".repeat(120);
    for n in name_candidates(&long) {
        assert!(n.chars().count() <= 120, "{n}");
    }
    assert!(name_candidates(&long)[9].ends_with("-10"));
}

fn invite_body(email: &str) -> Value {
    json!({"email": email, "name": "Zoe Acme", "company": "Acme Clinics", "locale": "nl", "residency": "eu"})
}

/// CP `inviteEarlyAccess`: a super_admin invites a person straight into early access — a `source = 'invite'`
/// signup (no trust, no intake client), then the SAME approve. No new-signup notice.
#[tokio::test]
async fn a_super_admin_invite_is_approved_at_once() {
    let (e, up) = env_on().await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            INVITE,
            Some(&super_admin()),
            invite_body("zoe@acme.io"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["reused_pending"], false);
    assert_eq!(b["seated"], true);
    assert_eq!(b["tenant"]["name"], "Acme Clinics");
    let sg = &b["signup"];
    assert_eq!(sg["status"], "approved");
    assert_eq!(sg["source"], "invite");
    assert_eq!(sg["trust"], Value::Null);
    assert_eq!(sg["intake_client"], Value::Null);
    assert_eq!(sg["invited_by"], "u-super");
    assert_eq!(sg["locale"], "nl");
    assert_eq!(sg["decided_via"], "ui");
    {
        let g = lock(&up.mock);
        assert_eq!(g.created.len(), 1);
        let attrs = &g.users.iter().find(|u| u.0 == g.created[0]).unwrap().2;
        assert_eq!(
            attrs["locale"],
            json!(["nl"]),
            "the email speaks the chosen language"
        );
        assert_eq!(g.emails.len(), 1);
    }
    let invites = platform_rows(&e, "signup.invite").await;
    assert_eq!(invites.len(), 1);
    assert_eq!(invites[0].0, "u-super");
    assert!(!invites[0].2.to_string().contains('@'));
    assert_eq!(platform_rows(&e, "signup.intake").await.len(), 0);
    // only the decision notice reaches Telegram, never a new-signup notice
    let t = telegram_count(&up, 1).await;
    assert_eq!(t.len(), 1, "{t:?}");
    assert!(
        t[0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Agent signup approved")
    );
}

#[tokio::test]
async fn an_invite_reuses_a_pending_signup_and_its_language_wins() {
    let (e, up) = env_on().await;
    let id = submit(&e, "bob@initech.com").await;
    let mut body = invite_body("Bob@Initech.com");
    body["locale"] = json!("fr");
    body["tenantName"] = json!("Initech Care");
    let (s, b) = send(&e.app, json("POST", INVITE, Some(&super_admin()), body)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["reused_pending"], true);
    assert_eq!(b["signup"]["id"], id.to_string());
    assert_eq!(
        b["signup"]["source"], "web",
        "the queued row is approved, not duplicated"
    );
    assert_eq!(b["signup"]["locale"], "fr");
    assert_eq!(b["tenant"]["name"], "Initech Care");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM signup_approvals")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1);
    let g = lock(&up.mock);
    let attrs = &g.users.iter().find(|u| u.0 == g.created[0]).unwrap().2;
    assert_eq!(attrs["locale"], json!(["fr"]));
}

#[tokio::test]
async fn invite_validation_and_guards() {
    let (e, _up) = env_on().await;
    let cases: [(Value, StatusCode, &str); 5] = [
        (
            json!({"email": "zoe@acme.io", "name": "Zoe", "locale": "xx", "residency": "eu"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_locale",
        ),
        (
            json!({"email": "zoe@acme.io", "name": "Zoe"}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_residency",
        ),
        (
            json!({"email": "not-an-email", "name": "Zoe", "residency": "eu"}),
            StatusCode::BAD_REQUEST,
            "invalid_body",
        ),
        (
            json!({"email": "zoe@acme.io", "name": "Zoe", "residency": "eu", "role": "x"}),
            StatusCode::BAD_REQUEST,
            "invalid_body",
        ),
        (
            json!({"email": "zoe@acme.io", "name": "Zoe", "residency": "eu", "tenantName": "x"}),
            StatusCode::BAD_REQUEST,
            "invalid_body",
        ),
    ];
    for (body, status, code) in cases {
        let (s, b) = send(
            &e.app,
            json("POST", INVITE, Some(&super_admin()), body.clone()),
        )
        .await;
        assert_eq!(s, status, "{body}: {b}");
        assert_eq!(b["error"]["code"], code, "{body}: {b}");
    }
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            INVITE,
            Some(&person("u-x", &[])),
            invite_body("zoe@acme.io"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // an address that already belongs to a workspace: refused before anything is written
    let other = Uuid::new_v4();
    common::insert_tenant(&e.owner, other, "Elsewhere").await;
    common::insert_user(&e.owner, "kc-zoe", other, "tenant_user").await;
    sqlx::query("UPDATE users SET email = 'zoe@acme.io' WHERE sub = 'kc-zoe'")
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            INVITE,
            Some(&super_admin()),
            invite_body("ZOE@acme.io"),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "user_belongs_to_another_workspace");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM signup_approvals")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0, "no stray pending row");
}

// ---- B1 T14: residency, platform audit, Retry, licence steps and the licence reconciler (D77) ---------------------

async fn approve_ok(e: &TestEnv, id: Uuid, residency: &str) -> Uuid {
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": residency}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["tenant"]["id"].as_str().unwrap().parse().unwrap()
}

async fn force_due(e: &TestEnv, id: Uuid) {
    sqlx::query(
        "UPDATE signup_provisioning_steps SET next_attempt_at = now(), lease_until = NULL WHERE signup_id = $1",
    )
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
}

async fn platform_rows(e: &TestEnv, action: &str) -> Vec<(String, String, Value)> {
    sqlx::query_as(
        "SELECT actor_id, status, details FROM platform_audit WHERE action = $1 ORDER BY id",
    )
    .bind(action)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

/// (step status, entitlement status) for the requester's member licence of `tenant`.
async fn licence_pair(e: &TestEnv, id: Uuid, tenant: Uuid) -> (String, Option<String>) {
    let step: String = sqlx::query_scalar(
        "SELECT status FROM signup_provisioning_steps WHERE signup_id = $1 AND step = 'member_licence'",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let ent: Option<String> =
        sqlx::query_scalar("SELECT status FROM member_entitlement WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_optional(&e.owner)
            .await
            .unwrap();
    (step, ent)
}

#[tokio::test]
async fn approve_without_residency_422() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    for body in [
        json!({}),
        json!({"residency": "mars"}),
        json!({"tenantName": "Globex Health"}),
    ] {
        let (s, b) = send(
            &e.app,
            json("POST", &approve_uri(id), Some(&super_admin()), body),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(b["error"]["code"], "invalid_residency");
    }
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM tenants")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
    let status: String = sqlx::query_scalar("SELECT status FROM signup_approvals WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(status, "pending");
}

#[tokio::test]
async fn approve_sets_residency() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    let tenant = approve_ok(&e, id, "us").await;
    let residency: String = sqlx::query_scalar("SELECT residency FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(residency, "us");
    let (origin, email): (String, Option<String>) =
        sqlx::query_as("SELECT origin, email FROM users WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(origin, "tenant_create");
    assert_eq!(email.as_deref(), Some("ann@globex.com"));
}

#[tokio::test]
async fn telegram_approve_uses_configured_residency() {
    let (e, _up) = env_on_with(|v| {
        v.insert("SIGNUP_TELEGRAM_RESIDENCY".into(), "us".into());
    })
    .await;
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            json!({"action": "approve", "telegramUserId": 7}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "approved");
    let tenant: Uuid = b["tenantId"].as_str().unwrap().parse().unwrap();
    let residency: String = sqlx::query_scalar("SELECT residency FROM tenants WHERE id = $1")
        .bind(tenant)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(residency, "us");
    let rows = platform_rows(&e, "signup.approve").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "system:telegram");
}

#[tokio::test]
async fn signup_decisions_land_in_platform_audit_only() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    let other = submit(&e, "bob@initech.com").await;
    let tenant = approve_ok(&e, id, "eu").await;
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{other}/reject"),
            Some(&super_admin()),
            json!({"reason": "not a fit for ann"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let intake = platform_rows(&e, "signup.intake").await;
    assert_eq!(intake.len(), 2);
    assert_eq!(intake[0].0, "system:agent-signup-intake");
    let approve = platform_rows(&e, "signup.approve").await;
    assert_eq!(approve.len(), 1);
    assert_eq!(approve[0].0, "u-super");
    assert_eq!(approve[0].2["residency"], "eu");
    let reject = platform_rows(&e, "signup.reject").await;
    assert_eq!(reject.len(), 1);
    assert!(
        !reject[0].2.to_string().contains("not a fit"),
        "the reason is free text"
    );
    let all: Vec<(String, Value)> = sqlx::query_as("SELECT action, details FROM platform_audit")
        .fetch_all(&e.owner)
        .await
        .unwrap();
    for (action, details) in &all {
        assert!(!details.to_string().contains('@'), "{action}: {details}");
    }
    let tenant_signup: i64 =
        sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action LIKE 'signup.%'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(tenant_signup, 0, "signup.* never reaches a tenant log");
    let actions: Vec<String> =
        sqlx::query_scalar("SELECT action FROM audit_event WHERE tenant_id = $1 ORDER BY id")
            .bind(tenant)
            .fetch_all(&e.owner)
            .await
            .unwrap();
    for a in ["tenant.create", "user.add", "license.issue"] {
        assert!(actions.iter().any(|x| x == a), "{a} in {actions:?}");
    }
    assert_eq!(
        actions.iter().filter(|a| *a == "license.issue").count(),
        2,
        "tenant licence + member licence: {actions:?}"
    );
}

#[tokio::test]
async fn retry_requeues_failed_steps_and_runs_them() {
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    lock(&up.mock).licences_down = true;
    approve_ok(&e, id, "eu").await;
    // the licences step gave up
    sqlx::query(
        "UPDATE signup_provisioning_steps SET status = 'failed', code = 'licences_unavailable', \
                next_attempt_at = NULL WHERE signup_id = $1 AND step = 'licences_tenant'",
    )
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
    lock(&up.mock).licences_down = false;
    let uri = format!("/api/signup-approvals/{id}/retry");
    let (s, b) = send(&e.app, json("POST", &uri, Some(&super_admin()), json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["requeued"], json!(["licences_tenant"]));
    let steps = b["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 7);
    assert!(
        steps
            .iter()
            .filter(|s| s["step"] != "app_licences")
            .all(|s| s["status"] == "ok"),
        "the retried step and the ones after it ran: {b}"
    );
    let rows = platform_rows(&e, "signup.retry").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2["steps"], json!(["licences_tenant"]));
    assert_eq!(rows[0].2["codes"], json!(["licences_unavailable"]));
    assert_eq!(lock(&up.mock).emails.len(), 1);
    let (s, _) = send(
        &e.app,
        json("POST", &uri, Some(&person("u-x", &[])), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn retry_on_pending_signup_409() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{id}/retry"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "signup_not_approved");
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{}/retry", Uuid::new_v4()),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["code"], "signup_not_found");
}

#[tokio::test]
async fn retry_twice_is_a_noop() {
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    approve_ok(&e, id, "eu").await;
    let uri = format!("/api/signup-approvals/{id}/retry");
    let (s, b) = send(&e.app, json("POST", &uri, Some(&super_admin()), json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["requeued"], json!([]));
    let (s, b) = send(&e.app, json("POST", &uri, Some(&super_admin()), json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["requeued"], json!([]));
    assert!(platform_rows(&e, "signup.retry").await.is_empty());
    assert_eq!(lock(&up.mock).emails.len(), 1, "nothing ran again");
}

#[tokio::test]
async fn list_carries_queue_counts() {
    let (e, _up) = env_on().await;
    submit(&e, "ann@globex.com").await;
    let other = submit(&e, "bob@initech.com").await;
    send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{other}/reject"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    let (s, b) = send(&e.app, get("/api/signup-approvals", Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["queue_cap"], 500);
    assert_eq!(b["pending"], 1);
    assert_eq!(
        b["counts"],
        json!({"received": 2, "approved": 0, "rejected": 1})
    );
    assert!(b["as_of"].is_string());
    assert_eq!(b["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn launch_lists_names_never_values() {
    let (e, _up) = env_on().await;
    let (s, b) = send(
        &e.app,
        get("/api/platform/signup-launch", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["enabled"], true);
    let items = b["items"].as_array().unwrap();
    let of = |name: &str| {
        items
            .iter()
            .find(|i| i["name"] == name)
            .unwrap_or_else(|| panic!("{name} not listed"))["status"]
            .clone()
    };
    assert_eq!(of("TELEGRAM_BOT_TOKEN"), "set");
    assert_eq!(of("SIGNUP_TELEGRAM_RESIDENCY"), "set");
    assert_eq!(of("TURNSTILE_SECRET"), "missing");
    let text = b.to_string();
    assert!(
        !text.contains(fakes::BOT_TOKEN)
            && !text.contains("-1003934173351")
            && !text.contains("4242"),
        "names only, never values: {text}"
    );
    assert_eq!(b["routes"].as_array().unwrap().len(), 7);
    let (s, _) = send(
        &e.app,
        get("/api/platform/signup-launch", Some(&person("u-x", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn launch_mounted_while_dark() {
    let e = common::env().await;
    let (s, b) = send(
        &e.app,
        get("/api/platform/signup-launch", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["enabled"], false);
    let enabled = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["name"] == "EARLY_ACCESS_SIGNUP_ENABLED")
        .unwrap()
        .clone();
    assert_eq!(enabled["status"], "missing");
    let (s, _) = send(&e.app, get("/api/signup-approvals", Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "the switch is off");
}

#[tokio::test]
async fn member_licence_step_ok_iff_entitlement_active() {
    // success: step ok and the entitlement active, together
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    let tenant = approve_ok(&e, id, "eu").await;
    assert_eq!(
        licence_pair(&e, id, tenant).await,
        ("ok".to_string(), Some("active".to_string()))
    );

    // mint failure: neither ok nor active
    let id = submit(&e, "bob@initech.com").await;
    lock(&up.mock).mint_down = true;
    // the intake body names the same company: a second workspace needs its own name
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"tenantName": "Initech", "residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let tenant: Uuid = b["tenant"]["id"].as_str().unwrap().parse().unwrap();
    // let the tenant licence exist so the run reaches the member licence while mints still fail
    sqlx::query("UPDATE tenants SET licence_id = 'lic-pre' WHERE id = $1")
        .bind(tenant)
        .execute(&e.owner)
        .await
        .unwrap();
    force_due(&e, id).await;
    let sg = e.state.signup.clone().unwrap();
    agent_api::signup::provisioning::tick(&e.state, &sg)
        .await
        .unwrap();
    let (step, ent) = licence_pair(&e, id, tenant).await;
    assert_eq!(step, "pending", "a mint failure is retried");
    assert_eq!(ent.as_deref(), Some("pending"));

    // retry after the failure: ok and active together
    lock(&up.mock).mint_down = false;
    force_due(&e, id).await;
    agent_api::signup::provisioning::tick(&e.state, &sg)
        .await
        .unwrap();
    assert_eq!(
        licence_pair(&e, id, tenant).await,
        ("ok".to_string(), Some("active".to_string()))
    );
}

#[tokio::test]
async fn signup_mints_after_commit_never_inside_the_txn() {
    let (e, up) = env_on().await;
    // the entitlement write (inside the member licence step's transaction) fails
    for sql in [
        "CREATE FUNCTION test_no_entitlement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'nope'; END $$",
        "CREATE TRIGGER test_no_entitlement BEFORE INSERT ON member_entitlement FOR EACH ROW EXECUTE FUNCTION test_no_entitlement()",
    ] {
        sqlx::query(sql).execute(&e.owner).await.unwrap();
    }
    let id = submit(&e, "ann@globex.com").await;
    let tenant = approve_ok(&e, id, "eu").await;
    let (step, ent) = licence_pair(&e, id, tenant).await;
    assert_eq!(step, "pending");
    assert_eq!(ent, None);
    let g = lock(&up.mock);
    assert!(
        g.issued
            .iter()
            .all(|(_, holder)| holder.starts_with("agent-tenant:")),
        "no member licence was created or minted: {:?}",
        g.issued
    );
    assert_eq!(g.minted, 1, "only the tenant licence");
    assert!(g.emails.is_empty());
}

async fn reconcile_env() -> (TestEnv, Upstream) {
    let up = fakes::upstream().await;
    let e = common::env_with(|v| fakes::identity_env(&up, v)).await;
    (e, up)
}

async fn licensed(e: &TestEnv, name: &str) -> Uuid {
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, name).await;
    sqlx::query("UPDATE tenants SET licence_id = 'lic-' || $2 WHERE id = $1")
        .bind(t)
        .bind(name)
        .execute(&e.owner)
        .await
        .unwrap();
    // a fully provisioned tenant: its app licences too, so the app-licence backfill has nothing to do here
    common::app_licences_held(&e.owner, t).await;
    t
}

#[tokio::test]
async fn tick_mints_pending_and_revokes_revoking_entitlements() {
    let (e, up) = reconcile_env().await;
    let t = licensed(&e, "Recon").await;
    for (sub, status, lic) in [("u-p", "pending", None), ("u-r", "revoking", Some("lic-r"))] {
        common::insert_user(&e.owner, sub, t, "tenant_user").await;
        sqlx::query(
            "INSERT INTO member_entitlement (tenant_id, user_sub, status, license_id) VALUES ($1, $2, $3, $4)",
        )
        .bind(t)
        .bind(sub)
        .bind(status)
        .bind(lic)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let r = agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    assert_eq!((r.minted, r.revoked, r.failed), (1, 1, 0), "{r:?}");
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT user_sub, status FROM member_entitlement WHERE tenant_id = $1 ORDER BY user_sub",
    )
    .bind(t)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("u-p".to_string(), "active".to_string()),
            ("u-r".to_string(), "revoked".to_string())
        ]
    );
    assert_eq!(lock(&up.mock).revoked, ["lic-r"]);
    // nothing left: a second pass does nothing
    let r = agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    assert_eq!(r, Default::default());
}

#[tokio::test]
async fn tick_drains_licence_revoke_pending() {
    let (e, up) = reconcile_env().await;
    let t = licensed(&e, "Drain").await;
    sqlx::query("UPDATE tenants SET licence_revoke_pending = '{lic-x,lic-y}' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    lock(&up.mock).revoke_noop_ids = vec!["lic-y".into()];
    let r = agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    assert_eq!((r.drained, r.failed), (1, 1), "{r:?}");
    let pending: Vec<String> =
        sqlx::query_scalar("SELECT licence_revoke_pending FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        pending,
        ["lic-y"],
        "an id leaves only when licences revoked it"
    );
    assert_eq!(lock(&up.mock).revoked, ["lic-x"]);
}

#[tokio::test]
async fn tick_ensures_tenant_licence_for_active_tenant_with_null_licence_id() {
    let (e, up) = reconcile_env().await;
    let (active, suspended, archived) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    common::insert_tenant(&e.owner, active, "Healme").await;
    common::insert_tenant(&e.owner, suspended, "Paused").await;
    common::insert_tenant(&e.owner, archived, "Gone").await;
    sqlx::query("UPDATE tenants SET status = 'suspended', suspended_at = now() WHERE id = $1")
        .bind(suspended)
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(archived)
        .execute(&e.owner)
        .await
        .unwrap();
    let r = agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    assert_eq!(r.tenant_licences, 1, "{r:?}");
    let rows: Vec<(Uuid, Option<String>, Option<String>)> =
        sqlx::query_as("SELECT id, licence_id, licence_token_encrypted FROM tenants")
            .fetch_all(&e.owner)
            .await
            .unwrap();
    for (id, lic, token) in rows {
        if id == active {
            assert!(lic.is_some());
            assert!(token.unwrap().starts_with("v2:v1:"));
        } else {
            assert!(lic.is_none(), "suspended / archived tenants are skipped");
        }
    }
    assert_eq!(lock(&up.mock).issued.len(), 1);
}

#[tokio::test]
async fn tick_lost_race_revokes_the_losing_token() {
    let (e, up) = reconcile_env().await;
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, "Racer").await;
    lock(&up.mock).mint_delay_ms = 400;
    let (r1, r2) = tokio::join!(
        agent_api::signup::provisioning::reconcile(&e.state),
        agent_api::signup::provisioning::reconcile(&e.state),
    );
    let stored = r1.unwrap().tenant_licences + r2.unwrap().tenant_licences;
    assert_eq!(stored, 1, "one pass stores");
    let lic: Option<String> = sqlx::query_scalar("SELECT licence_id FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let g = lock(&up.mock);
    assert_eq!(g.minted, 2);
    assert_eq!(g.revoked.len(), 1, "the loser's licence is revoked");
    assert_ne!(Some(g.revoked[0].clone()), lic, "never the winner's");
}

/// CP `scripts/renotify-signup.ts`: re-post ONE pending signup's notice (with its buttons); dry run by default,
/// pending intake rows only, nothing written.
#[tokio::test]
async fn renotify_reposts_a_pending_signups_notice_and_writes_nothing() {
    use agent_api::signup::renotify::{Renotice, renotify};
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    telegram_count(&up, 1).await;
    let before: Value =
        sqlx::query_scalar("SELECT to_jsonb(s) FROM signup_approvals s WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .unwrap();

    match renotify(&e.state, id, false).await.unwrap() {
        Renotice::DryRun(what) => {
            assert!(what.contains(&id.to_string()), "{what}");
            assert!(!what.contains("ann@"), "no personal data: {what}");
        }
        other => panic!("{other:?}"),
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(lock(&up.mock).telegram.len(), 1, "a dry run posts nothing");

    assert_eq!(
        renotify(&e.state, id, true).await.unwrap(),
        Renotice::Posted(true)
    );
    let t = lock(&up.mock).telegram.clone();
    assert_eq!(t.len(), 2);
    assert_eq!(t[1]["text"], t[0]["text"], "the same notice");
    assert_eq!(
        t[1]["reply_markup"], t[0]["reply_markup"],
        "with its buttons"
    );
    let after: Value =
        sqlx::query_scalar("SELECT to_jsonb(s) FROM signup_approvals s WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(before, after, "nothing written");

    // refused: unknown, decided, and an invite (never notified)
    assert!(matches!(
        renotify(&e.state, Uuid::new_v4(), true).await.unwrap(),
        Renotice::Refused(_)
    ));
    approve_ok(&e, id, "eu").await;
    assert!(matches!(
        renotify(&e.state, id, true).await.unwrap(),
        Renotice::Refused(r) if r.contains("approved")
    ));
    sqlx::query(
        "INSERT INTO signup_approvals (email, name, source, trust, intake_client, invited_by) \
         VALUES ('zoe@acme.io', 'Zoe', 'invite', NULL, NULL, 'u-super')",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    let invite: Uuid =
        sqlx::query_scalar("SELECT id FROM signup_approvals WHERE source = 'invite'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert!(matches!(
        renotify(&e.state, invite, true).await.unwrap(),
        Renotice::Refused(r) if r.contains("never notified")
    ));
}

// ---- reject retires the identity an aborted approve minted; verified-email accept of an approval's invite ------

/// A person token carrying `email` and `email_verified`.
fn verified_token(sub: &str, email: &str) -> String {
    common::sign(&json!({
        "sub": sub, "preferred_username": sub, "email": email, "email_verified": true,
        "exp": common::in_five_minutes(), "iss": common::TEST_ISSUER, "aud": common::TEST_AUDIENCE,
        "azp": common::TEST_AUDIENCE, "resource_access": {common::TEST_AUDIENCE: {"roles": []}}
    }))
}

/// Takes `Globex` and `Globex-2` … `Globex-10`, so a signup suggesting "Globex" cannot found its workspace.
async fn take_every_globex_name(e: &TestEnv) {
    common::insert_tenant(&e.owner, Uuid::new_v4(), "Globex").await;
    for n in 2..=10 {
        common::insert_tenant(&e.owner, Uuid::new_v4(), &format!("Globex-{n}")).await;
    }
}

async fn reject_ui(e: &TestEnv, id: Uuid) -> Value {
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{id}/reject"),
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

/// CP `retireRejectedIdentity`: the identity an aborted approve minted is DELETED when nobody ever set it up
/// (no credential, no IdP link), else DISABLED and marked `signup_app=agent_rejected:<id>`.
#[tokio::test]
async fn reject_retires_the_identity_an_aborted_approve_minted() {
    // never set up: deleted
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    take_every_globex_name(&e).await;
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let minted = lock(&up.mock).created[0].clone();
    let b = reject_ui(&e, id).await;
    assert_eq!(b["identity_retired"], "deleted", "{b}");
    assert_eq!(lock(&up.mock).deleted, std::slice::from_ref(&minted));
    let repair = platform_rows(&e, "signup.reject_repair").await;
    assert_eq!(repair.len(), 1);
    assert_eq!(repair[0].2["identity"], "deleted");

    // set up meanwhile (a password): disabled and marked, never deleted
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    take_every_globex_name(&e).await;
    send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    let minted = lock(&up.mock).created[0].clone();
    lock(&up.mock).with_password.push(minted.clone());
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &decision_uri(id),
            Some(&decider_token()),
            json!({"action": "reject", "telegramUserId": 7}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["cleanup"], "done");
    assert!(b["summary"].as_str().unwrap().contains("disabled"), "{b}");
    let g = lock(&up.mock);
    assert!(g.deleted.is_empty());
    assert_eq!(g.disabled, std::slice::from_ref(&minted));
    let attrs = &g.users.iter().find(|u| u.0 == minted).unwrap().2;
    assert_eq!(attrs["signup_app"], json!([format!("agent_rejected:{id}")]));
}

/// A pre-existing identity is never deleted or disabled by a reject, whatever the approve left behind.
#[tokio::test]
async fn reject_never_touches_a_pre_existing_identity() {
    let (e, up) = env_on().await;
    existing(&up, "kc-existing", "ann@globex.com", false, false);
    let id = submit(&e, "ann@globex.com").await;
    take_every_globex_name(&e).await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    let b = reject_ui(&e, id).await;
    assert_eq!(b["identity_retired"], Value::Null, "{b}");
    let g = lock(&up.mock);
    assert!(g.deleted.is_empty() && g.disabled.is_empty());
    assert!(g.users.iter().any(|u| u.0 == "kc-existing"));
}

/// The approval of a pre-existing identity finishes when that person signs in with the VERIFIED invited email: the
/// invite bound to their account is accepted, they are seated with every role, licensed through the signup's own
/// `member_licence` step, and the signup's journal shows the skipped steps done.
#[tokio::test]
async fn a_pre_existing_identity_is_seated_when_it_signs_in_verified() {
    let (e, up) = env_on().await;
    existing(&up, "kc-existing", "ann@globex.com", true, true);
    let id = submit(&e, "ann@globex.com").await;
    let tenant = approve_ok(&e, id, "eu").await;
    // unverified: nothing
    let unverified = common::sign(&json!({
        "sub": "kc-existing", "preferred_username": "ann", "email": "ann@globex.com", "email_verified": false,
        "exp": common::in_five_minutes(), "iss": common::TEST_ISSUER, "aud": common::TEST_AUDIENCE,
        "azp": common::TEST_AUDIENCE, "resource_access": {common::TEST_AUDIENCE: {"roles": []}}
    }));
    send(&e.app, get("/api/me", Some(&unverified))).await;
    assert_eq!(step(&e, id, "membership").await.0, "skipped");

    let (s, me) = send(
        &e.app,
        get(
            "/api/me",
            Some(&verified_token("kc-existing", "Ann@Globex.com")),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{me}");
    assert_eq!(me["tenant"]["id"], tenant.to_string(), "{me}");
    assert_eq!(me["roles"], json!(common::ALL_ROLES));
    for (name, status, code) in [
        ("membership", "ok", Some("invite_accepted")),
        ("member_licence", "ok", None),
        ("invite_email", "ok", Some("invite_accepted")),
    ] {
        let (s, c, _) = step(&e, id, name).await;
        assert_eq!((s.as_str(), c.as_deref()), (status, code), "{name}");
    }
    assert_eq!(
        licence_pair(&e, id, tenant).await,
        ("ok".to_string(), Some("active".to_string()))
    );
    assert!(
        lock(&up.mock)
            .issued
            .iter()
            .any(|(_, h)| h == "kc-existing")
    );
    let (state, by): (String, Option<String>) =
        sqlx::query_as("SELECT state, accepted_by FROM member_invites WHERE tenant_id = $1")
            .bind(tenant)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        (state.as_str(), by.as_deref()),
        ("accepted", Some("kc-existing"))
    );
    // the queue shows the finished journal
    let (_, q) = send(&e.app, get("/api/signup-approvals", Some(&super_admin()))).await;
    let st = q["items"][0]["steps"].as_array().unwrap().clone();
    assert!(
        st.iter()
            .filter(|s| s["step"] != "app_licences")
            .all(|s| s["status"] == "ok"),
        "{st:?}"
    );
}

/// An identity a reject of THIS service retired: disabled, `signup_app=agent_rejected:<id>`.
fn retired(up: &Upstream, id: &str, email: &str, password: bool) {
    existing(up, id, email, password, false);
    let mut g = lock(&up.mock);
    g.disabled.push(id.into());
    let u = g.users.iter_mut().find(|u| u.0 == id).unwrap();
    u.2.insert(
        "signup_app".into(),
        json!(["agent_rejected:00000000-0000-0000-0000-000000000001"]),
    );
}

/// CP `reclaimDisabledIdentity` + invite-service `minted = created || reclaimed.credentialless`: a re-signup
/// re-enables an identity this service retired; it is provisioned like a freshly created one ONLY when it has no
/// credential and no IdP link. One that was set up takes the invite-pending path (re-enabled, so it can sign in and
/// accept). An identity disabled/marked by another app is never touched.
#[tokio::test]
async fn a_re_signup_reclaims_only_a_credentialless_identity_this_service_retired() {
    // credentialless: seated, licensed, set-password email
    let (e, up) = env_on().await;
    retired(&up, "kc-old", "ann@globex.com", false);
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["seated"], true, "{b}");
    {
        let g = lock(&up.mock);
        assert!(g.created.is_empty(), "reclaimed, not created");
        assert!(g.disabled.is_empty(), "re-enabled");
        let attrs = &g.users.iter().find(|u| u.0 == "kc-old").unwrap().2;
        assert_eq!(
            attrs["signup_app"],
            json!(["agent"]),
            "marker replaced: {attrs:?}"
        );
        let (who, _, actions) = g.emails.last().unwrap();
        assert_eq!(who, "kc-old");
        assert_eq!(*actions, json!(["UPDATE_PASSWORD", "VERIFY_EMAIL"]));
    }

    // set up (a credential): re-enabled but NOT seated; invite-pending, no email
    let (e, up) = env_on().await;
    retired(&up, "kc-old", "ann@globex.com", true);
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["seated"], false, "{b}");
    assert_eq!(step(&e, id, "membership").await.0, "skipped");
    {
        let g = lock(&up.mock);
        assert!(
            g.disabled.is_empty(),
            "re-enabled so it can sign in and accept"
        );
        assert!(g.emails.is_empty());
    }

    // another app's marker: never re-enabled, invite-pending
    let (e, up) = env_on().await;
    existing(&up, "kc-cp", "bob@initech.com", true, true);
    {
        let mut g = lock(&up.mock);
        g.disabled.push("kc-cp".into());
        let u = g.users.iter_mut().find(|u| u.0 == "kc-cp").unwrap();
        u.2.insert("signup_app".into(), json!(["cp_rejected:x"]));
    }
    let id = submit(&e, "bob@initech.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu", "tenantName": "Initech"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["seated"], false);
    let g = lock(&up.mock);
    assert_eq!(g.disabled, ["kc-cp".to_string()], "left disabled");
    assert!(g.emails.is_empty());
}

/// CP `restoreRejectedIdentity`: an approve that fails after reclaiming puts the identity back — disabled, marker
/// restored — and the next approve reclaims it afresh.
#[tokio::test]
async fn a_failed_approve_after_a_reclaim_disables_and_re_marks_it() {
    let (e, up) = env_on().await;
    retired(&up, "kc-old", "ann@globex.com", false);
    let id = submit(&e, "ann@globex.com").await;
    take_every_globex_name(&e).await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    {
        let g = lock(&up.mock);
        assert_eq!(g.disabled, ["kc-old".to_string()], "disabled again");
        let attrs = &g.users.iter().find(|u| u.0 == "kc-old").unwrap().2;
        assert_eq!(
            attrs["signup_app"],
            json!(["agent_rejected:00000000-0000-0000-0000-000000000001"]),
            "marker restored"
        );
    }
    assert_eq!(step(&e, id, "keycloak_user").await.0, "pending");
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu", "tenantName": "Globex Again"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["seated"], true, "{b}");
    assert!(lock(&up.mock).disabled.is_empty());
}

/// A create whose answer was lost (`create_attempted`) does not make an account found by email "minted": only the
/// ownership stamp the approval creates identities with does. Another app's account is invite-pending, and a reject
/// never disables or deletes it.
#[tokio::test]
async fn a_lost_create_does_not_adopt_another_apps_account() {
    let (e, up) = env_on().await;
    existing(&up, "kc-other-app", "ann@globex.com", false, false);
    let id = submit(&e, "ann@globex.com").await;
    sqlx::query(
        "INSERT INTO signup_provisioning_steps (signup_id, step, ord, status, data, next_attempt_at) \
         VALUES ($1, 'keycloak_user', 1, 'pending', '{\"create_attempted\": true}', now())",
    )
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
    take_every_globex_name(&e).await;
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let b = reject_ui(&e, id).await;
    assert_eq!(b["identity_retired"], Value::Null, "{b}");
    {
        let g = lock(&up.mock);
        assert!(
            g.disabled.is_empty() && g.deleted.is_empty(),
            "never touched"
        );
    }
    // the same with a free name: approved, but not seated
    let id = submit(&e, "ann@globex.com").await;
    sqlx::query(
        "INSERT INTO signup_provisioning_steps (signup_id, step, ord, status, data, next_attempt_at) \
         VALUES ($1, 'keycloak_user', 1, 'pending', '{\"create_attempted\": true}', now())",
    )
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu", "tenantName": "Globex Two"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["seated"], false, "{b}");
    assert_eq!(step(&e, id, "membership").await.0, "skipped");
}

/// CP reclaim order: the credentials are read while the identity is still disabled and marked. A read that fails
/// leaves it exactly so (never re-enabled without its marker), and nothing is decided.
#[tokio::test]
async fn a_failed_credential_read_leaves_a_retired_identity_disabled_and_marked() {
    let (e, up) = env_on().await;
    retired(&up, "kc-old", "ann@globex.com", false);
    lock(&up.mock).credentials_down = true;
    let id = submit(&e, "ann@globex.com").await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_GATEWAY, "{b}");
    {
        let g = lock(&up.mock);
        assert_eq!(g.disabled, ["kc-old".to_string()], "still disabled");
        let attrs = &g.users.iter().find(|u| u.0 == "kc-old").unwrap().2;
        assert_eq!(
            attrs["signup_app"],
            json!(["agent_rejected:00000000-0000-0000-0000-000000000001"]),
            "still marked"
        );
    }
    assert_eq!(tenant_count(&e).await, 0);
    // once Keycloak answers, the approve reclaims it
    lock(&up.mock).credentials_down = false;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["seated"], true, "{b}");
}

/// The positive half of the ownership stamp: a create whose answer was lost left an identity stamped
/// `signup_app=agent:<this signup id>`; the retry adopts it as minted by this approval and seats it.
#[tokio::test]
async fn a_lost_create_of_this_signup_is_adopted_by_its_stamp() {
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    existing(&up, "kc-mine", "ann@globex.com", false, false);
    {
        let mut g = lock(&up.mock);
        let u = g.users.iter_mut().find(|u| u.0 == "kc-mine").unwrap();
        u.2.insert("signup_app".into(), json!([format!("agent:{id}")]));
    }
    sqlx::query(
        "INSERT INTO signup_provisioning_steps (signup_id, step, ord, status, data, next_attempt_at) \
         VALUES ($1, 'keycloak_user', 1, 'pending', '{\"create_attempted\": true}', now())",
    )
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &approve_uri(id),
            Some(&super_admin()),
            json!({"residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["seated"], true, "{b}");
    let g = lock(&up.mock);
    assert!(g.created.is_empty(), "adopted, not created again");
    let (who, _, actions) = g.emails.last().unwrap();
    assert_eq!(who, "kc-mine");
    assert_eq!(*actions, json!(["UPDATE_PASSWORD", "VERIFY_EMAIL"]));
}

/// CP `license_provision_orphan_unrevoked_total` + `sweepTenantLicences`: a losing minter whose revoke does not take
/// counts the orphan and parks its id; the licence reconciler revokes it later. Never the winner's.
#[tokio::test]
async fn an_unrevoked_losing_licence_is_counted_parked_and_swept() {
    let h = agent_api::observability::metrics_handle();
    let (e, up) = reconcile_env().await;
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, "Racer").await;
    {
        let mut g = lock(&up.mock);
        g.mint_delay_ms = 400;
        g.revoke_noop = true;
    }
    let (r1, r2) = tokio::join!(
        agent_api::signup::provisioning::reconcile(&e.state),
        agent_api::signup::provisioning::reconcile(&e.state),
    );
    r1.unwrap();
    r2.unwrap();
    let (winner, parked): (Option<String>, Vec<String>) =
        sqlx::query_as("SELECT licence_id, licence_revoke_pending FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    let winner = winner.expect("one licence stored");
    assert_eq!(parked.len(), 1, "the loser's licence is parked: {parked:?}");
    assert_ne!(parked[0], winner, "never the winner's");
    let text = h.render();
    let orphans: f64 = text
        .lines()
        .find(|l| l.starts_with("agent_license_orphan_unrevoked_total"))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0);
    assert!(orphans >= 1.0, "{text}");
    // the sweep (the licence reconciler) revokes it once licences answers
    {
        let mut g = lock(&up.mock);
        g.revoke_noop = false;
        g.mint_delay_ms = 0;
    }
    sqlx::query("DELETE FROM licence_work_backoff")
        .execute(&e.owner)
        .await
        .unwrap();
    agent_api::signup::provisioning::reconcile(&e.state)
        .await
        .unwrap();
    let parked: Vec<String> =
        sqlx::query_scalar("SELECT licence_revoke_pending FROM tenants WHERE id = $1")
            .bind(t)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert!(parked.is_empty(), "{parked:?}");
    let g = lock(&up.mock);
    assert!(g.revoked.iter().any(|r| *r != winner), "{:?}", g.revoked);
    assert!(!g.revoked.contains(&winner));
}

// ---- B1b T1 (F-03): the turnstile outcome on the intake audit row ------------------------------------------------

/// `details.turnstile` of signup `id`'s `signup.intake` platform row.
async fn intake_turnstile(e: &TestEnv, id: Uuid) -> Option<String> {
    sqlx::query_scalar(
        "SELECT details->>'turnstile' FROM platform_audit \
          WHERE action = 'signup.intake' AND details->>'signup' = $1",
    )
    .bind(id.to_string())
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn submit_with(e: &TestEnv, b: Value) -> (StatusCode, Value) {
    send(&e.app, json("POST", INTAKE, Some(&intake_token()), b)).await
}

async fn pending_id(e: &TestEnv, email: &str) -> Uuid {
    sqlx::query_scalar(
        "SELECT id FROM signup_approvals WHERE lower(email) = lower($1) AND status = 'pending'",
    )
    .bind(email)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn intake_audit_row_records_turnstile_outcome_three_ways() {
    // no TURNSTILE_SECRET: nothing was checked
    let (e, _up) = env_on().await;
    let id = submit(&e, "off@globex.com").await;
    assert_eq!(
        intake_turnstile(&e, id).await.as_deref(),
        Some("not_configured")
    );
    // a secret, no relayed token: absent; a token siteverify accepts: verified
    let (e, up) = env_on_with(|v| {
        v.insert("TURNSTILE_SECRET".into(), "ts-secret".into());
    })
    .await;
    let id = submit(&e, "absent@globex.com").await;
    assert_eq!(intake_turnstile(&e, id).await.as_deref(), Some("absent"));
    let mut b = body("verified@globex.com");
    b["turnstileToken"] = json!("good-token");
    let (s, r) = submit_with(&e, b).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    let id = pending_id(&e, "verified@globex.com").await;
    assert_eq!(intake_turnstile(&e, id).await.as_deref(), Some("verified"));
    assert_eq!(
        lock(&up.mock).verified,
        ["good-token"],
        "one siteverify call"
    );
}

#[tokio::test]
async fn turnstile_refusals_unchanged() {
    // a token siteverify refuses: 403, nothing queued, no intake row
    let (e, _up) = env_on_with(|v| {
        v.insert("TURNSTILE_SECRET".into(), "ts-secret".into());
    })
    .await;
    let mut b = body("bad@globex.com");
    b["turnstileToken"] = json!("bad-token");
    let (s, r) = submit_with(&e, b).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{r}");
    assert_eq!(r["error"]["code"], "turnstile_failed");
    // siteverify unreachable: 503
    let (e2, _up2) = env_on_with(|v| {
        v.insert("TURNSTILE_SECRET".into(), "ts-secret".into());
        v.insert("TURNSTILE_VERIFY_URL".into(), common::NO_UPSTREAM.into());
    })
    .await;
    let mut b = body("down@globex.com");
    b["turnstileToken"] = json!("good-token");
    let (s, r) = submit_with(&e2, b).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{r}");
    assert_eq!(r["error"]["code"], "turnstile_unavailable");
    for env in [&e, &e2] {
        let (rows, intakes): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM signup_approvals), \
                    (SELECT count(*) FROM platform_audit WHERE action = 'signup.intake')",
        )
        .fetch_one(&env.owner)
        .await
        .unwrap();
        assert_eq!((rows, intakes), (0, 0), "a refusal records nothing");
    }
}

// ---- B1b T3 (F-03): intake signals on the approval queue ---------------------------------------------------------

/// The `signals` of queue row `id` (super_admin `GET /api/signup-approvals`).
async fn signals(e: &TestEnv, id: Uuid) -> Vec<Value> {
    let (s, b) = send(
        &e.app,
        get("/api/signup-approvals?limit=200", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for item in b["items"].as_array().unwrap() {
        assert!(
            item["signals"].is_array(),
            "every row carries signals: {item}"
        );
    }
    b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id.to_string())
        .unwrap_or_else(|| panic!("row {id} in the queue: {b}"))["signals"]
        .as_array()
        .unwrap()
        .clone()
}

fn of_key<'a>(s: &'a [Value], key: &str) -> Vec<&'a Value> {
    s.iter().filter(|x| x["key"] == key).collect()
}

fn one_of<'a>(s: &'a [Value], key: &str) -> &'a Value {
    let hits = of_key(s, key);
    assert_eq!(hits.len(), 1, "one {key} signal: {s:?}");
    hits[0]
}

#[tokio::test]
async fn turnstile_signal_read_from_intake_audit_row() {
    let (e, _up) = env_on_with(|v| {
        v.insert("TURNSTILE_SECRET".into(), "ts-secret".into());
    })
    .await;
    let absent = submit(&e, "absent@globex.com").await;
    let mut b = body("verified@globex.com");
    b["turnstileToken"] = json!("good-token");
    assert_eq!(submit_with(&e, b).await.0, StatusCode::ACCEPTED);
    let verified = pending_id(&e, "verified@globex.com").await;
    let t = one_of(&signals(&e, absent).await, "turnstile").clone();
    assert_eq!(
        (t["level"].as_str(), t["audit_action"].as_str()),
        (Some("risk"), Some("signup.intake"))
    );
    let t = one_of(&signals(&e, verified).await, "turnstile").clone();
    assert_eq!(t["level"], "ok");
    let (e, _up) = env_on().await;
    let off = submit(&e, "off@globex.com").await;
    assert_eq!(
        one_of(&signals(&e, off).await, "turnstile")["level"],
        "info"
    );
}

#[tokio::test]
async fn pre_t1_intake_row_gives_not_recorded_info() {
    let (e, _up) = env_on().await;
    // a request queued before T1 recorded the outcome: no intake row with `turnstile`
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO signup_approvals (email, name, source, intake_client) \
         VALUES ('old@globex.com', 'Old Request', 'web', 'agent-signup-intake') RETURNING id",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO platform_audit (actor_id, actor_type, action, subject_label, status, details) \
         VALUES ('system:agent-signup-intake', 'system', 'signup.intake', 'Globex', 'ok', \
                 jsonb_build_object('signup', $1::text, 'source', 'web'))",
    )
    .bind(id.to_string())
    .execute(&e.owner)
    .await
    .unwrap();
    let t = one_of(&signals(&e, id).await, "turnstile").clone();
    assert_eq!(t["level"], "info");
    assert_eq!(t["label"], "Bot check not recorded");
}

#[tokio::test]
async fn disposable_domain_is_risk_freemail_is_info() {
    let (e, _up) = env_on().await;
    let throwaway = submit(&e, "x@mailinator.com").await;
    let personal = submit(&e, "y@gmail.com").await;
    let company = submit(&e, "z@globex.com").await;
    assert_eq!(
        one_of(&signals(&e, throwaway).await, "domain")["level"],
        "risk"
    );
    let p = one_of(&signals(&e, personal).await, "domain").clone();
    assert_eq!(
        (p["level"].as_str(), p["label"].as_str()),
        (Some("info"), Some("Personal address"))
    );
    assert!(p["detail"].as_str().unwrap().contains("gmail.com"));
    assert_eq!(one_of(&signals(&e, company).await, "domain")["level"], "ok");
}

#[tokio::test]
async fn duplicate_email_any_status_is_risk() {
    let (e, _up) = env_on().await;
    let first = submit(&e, "dup@globex.com").await;
    reject_ui(&e, first).await;
    let again = submit(&e, "Dup@Globex.com").await;
    let d = one_of(&signals(&e, again).await, "duplicate").clone();
    assert_eq!(
        (d["level"].as_str(), d["audit_action"].as_str()),
        (Some("risk"), Some("signup.intake"))
    );
    // the same company domain, pending: info; a fresh address on another domain: ok
    let colleague = submit(&e, "colleague@globex.com").await;
    assert_eq!(
        one_of(&signals(&e, colleague).await, "duplicate")["level"],
        "info"
    );
    let lone = submit(&e, "lone@initech.test").await;
    assert_eq!(one_of(&signals(&e, lone).await, "duplicate")["level"], "ok");
}

#[tokio::test]
async fn previously_rejected_email_is_risk_with_reject_action() {
    let (e, _up) = env_on().await;
    let first = submit(&e, "again@globex.com").await;
    reject_ui(&e, first).await;
    let again = submit(&e, "again@globex.com").await;
    let h = signals(&e, again).await;
    let hist = of_key(&h, "history");
    assert!(
        hist.iter()
            .any(|x| x["level"] == "risk" && x["audit_action"] == "signup.reject"),
        "{h:?}"
    );
    let fresh = submit(&e, "fresh@globex.com").await;
    let hist = signals(&e, fresh).await;
    assert_eq!(one_of(&hist, "history")["level"], "ok");
}

#[tokio::test]
async fn existing_member_email_is_risk() {
    let (e, _up) = env_on().await;
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, "Initech").await;
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, roles, display_name, email, updated_by) \
         VALUES ('u-member', $1, '{}', 'Member', 'member@initech.test', 'test')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    let id = submit(&e, "Member@Initech.test").await;
    let h = signals(&e, id).await;
    let hist = of_key(&h, "history");
    assert!(
        hist.iter()
            .any(|x| x["level"] == "risk" && x["audit_action"] == "user.add"),
        "{h:?}"
    );
}

#[tokio::test]
async fn signals_never_contain_typed_text() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@initech.test").await;
    let s = signals(&e, id).await;
    let text = serde_json::to_string(&s).unwrap();
    for typed in ["Ann Early", "Globex", "clinics", "ann@", "initech.test@"] {
        assert!(!text.contains(typed), "{typed:?} in {text}");
    }
    // the five keys are there, each with a level the UI knows
    for key in ["turnstile", "domain", "duplicate", "history", "detail"] {
        assert!(!of_key(&s, key).is_empty(), "{key}: {s:?}");
    }
    for x in &s {
        assert!(
            ["ok", "info", "risk"].contains(&x["level"].as_str().unwrap()),
            "{x}"
        );
    }
    // a description was given in body(); none given → info
    assert_eq!(one_of(&s, "detail")["level"], "ok");
    let mut b = body("quiet@initech.test");
    b.as_object_mut().unwrap().remove("message");
    assert_eq!(submit_with(&e, b).await.0, StatusCode::ACCEPTED);
    let quiet = pending_id(&e, "quiet@initech.test").await;
    assert_eq!(one_of(&signals(&e, quiet).await, "detail")["level"], "info");
}

#[tokio::test]
async fn signals_page_query_count_is_constant() {
    use agent_api::platform_db::begin_platform;
    use agent_api::signup::{signals as sig, store};
    let (e, _up) = env_on().await;
    for i in 0..50 {
        sqlx::query(
            "INSERT INTO signup_approvals (email, name, source, intake_client, message) \
             VALUES ($1, 'Someone', 'web', 'agent-signup-intake', CASE WHEN $2 THEN 'hello' END)",
        )
        .bind(format!("p{i}@d{}.test", i % 7))
        .bind(i % 2 == 0)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let mut queries = Vec::new();
    for n in [5i64, 50] {
        let mut tx = begin_platform(&e.state.db).await.unwrap();
        let rows = store::list(&mut tx, None, None, n).await.unwrap();
        assert_eq!(rows.len() as i64, n);
        let s = sig::for_rows(&mut tx, &rows).await.unwrap();
        // still agent_platform afterwards (the caller's transaction is left as it was)
        let role: String = sqlx::query_scalar("SELECT current_user::text")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(role, "agent_platform");
        tx.commit().await.unwrap();
        assert_eq!(s.by_id.len() as i64, n, "every row has signals");
        queries.push(s.queries);
    }
    assert_eq!(
        queries[0], queries[1],
        "5 rows and 50 rows: the same query count"
    );
    assert!(queries[0] <= 3, "{queries:?}");
}

// ---- B1b T10: an approval's licences carry the workspace's residency region --------------------------------------

#[tokio::test]
async fn approval_issues_licences_in_the_residency_region() {
    let (e, up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    approve_ok(&e, id, "us").await;
    let g = lock(&up.mock);
    assert!(!g.issued_regions.is_empty(), "licences were created");
    assert!(
        g.issued_regions
            .iter()
            .all(|r| r.as_deref() == Some(fakes::REGION_US)),
        "{:?}",
        g.issued_regions
    );
}

// ---- B1b T11: the approval's access email outcome lands in the new workspace's log --------------------------------

#[tokio::test]
async fn approval_email_outcome_is_audited_in_the_workspace() {
    let (e, _up) = env_on().await;
    let id = submit(&e, "ann@globex.com").await;
    let t = approve_ok(&e, id, "eu").await;
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT status, details FROM audit_event WHERE tenant_id = $1 AND action = 'user.invite_email'",
    )
    .bind(t)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    let signup: Vec<&(String, Value)> = rows.iter().filter(|r| r.1["via"] == "signup").collect();
    assert_eq!(signup.len(), 1, "{rows:?}");
    assert_eq!(signup[0].0, "success");
    assert_eq!(signup[0].1["access"], "set_password");
}
