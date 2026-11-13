//! B4 audit gaps (#35, #44, #45): tests the B4 audits found missing. Each test names the mutation it catches.
//!
//! 1. (#35) A provisioned tenant that binds its own `qualify` provider runs the binding, never the
//!    tenant router lane (`providers::llm::resolve_in` / `tenant_router`).
//! 2. (#44) Cross-tenant: the F2 conversation replay (`sse_hub::conversation_events` / `messages_since`) and the
//!    runner's `ops::party_email` never read another tenant's (or job's) rows.
//! 3. (#45) A PROD case with a LIVE `tenant_calendar` snapshot: the handover hours and SLA targets read the released
//!    calendar and policy, not the draft.
//! 4. (#44) Pins the CURRENT behaviour when the live design does not cover `handover_policy` (no baseline, or a
//!    release whose `snapshot_kinds` lacks it — the state a failed coverage backfill leaves): the PROD read falls
//!    through to the working rows, i.e. prod runs the DRAFT policy (reported as a defect, not fixed here).
//!
//! Real Postgres 18 (one container for the `b4` binary, a fresh database per test).
use std::collections::HashMap;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get as route_get;
use axum::{Json, Router};
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use tower::ServiceExt;
use uuid::Uuid;

use crate::b4::events::Feed;
use crate::common::conv::{self, Ctx};
use crate::common::design_contract::{self, ContractJob, cuttable_selection, new_job};
use crate::common::{self, TestEnv, get, insert_tenant, insert_user, json, person, send};
use crate::redis_env as r;
use agent_api::connectors::transport::Transport;
use agent_api::conversations::hub::Frame;
use agent_api::conversations::store;
use agent_api::design::{CaseDesign, DesignOf, DraftRef, JobRef};
use agent_api::handover::policy;
use agent_api::netguard::Policy;
use agent_api::providers::llm::{self, ResolveCtx, Use};
use agent_api::runtime::runner::{agent_cases, ops};
use agent_api::tenancy::begin_scoped;

const WAIT: Duration = Duration::from_secs(5);

// ==== 1. (#35) a fallback-listed tenant's own qualify binding wins ===================================================

/// The key the mock provider accepts.
const GOOD: &str = "sk-good-1";

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    vars.insert(
        "PROVIDER_SECRET_KEYS".to_string(),
        format!(r#"{{"p1":"{key}"}}"#),
    );
    vars.insert("PROVIDER_SECRET_KEY_CURRENT".to_string(), "p1".to_string());
}

async fn openai_models() -> (StatusCode, Json<Value>) {
    (
        StatusCode::OK,
        Json(json!({"data": [{"id": "gpt-4.1-mini"}]})),
    )
}

/// A loopback OpenAI-compatible `/models`: its base URL.
async fn fake_provider() -> String {
    let app = Router::new().route("/ok/models", route_get(openai_models));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}/ok")
}

fn loopback_app(e: &TestEnv) -> Router {
    let mut st = e.state.clone();
    st.connectors = Transport::new(Policy {
        allow_loopback: true,
        private_targets: vec![],
    });
    agent_api::router(st)
}

async fn member(e: &TestEnv, tenant: Uuid, role: &str) -> String {
    let sub = format!("u-{}-{}", role.replace(',', "-"), Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, role).await;
    person(&sub, &[])
}

/// Approve and apply the job's draft as release manager `tok` (pattern tests/b3/llm_providers.rs `publish`).
async fn publish(app: &Router, f: &ContractJob, tok: &str) {
    let job = &f.job.job_key;
    let (s, b) = send(app, get(&format!("/api/jobs/{job}/target"), Some(tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let body = json!({"apply": true, "selection_seen_at": b["selection_updated_at"].clone()});
    let (s, b) = send(
        app,
        json(
            "POST",
            &format!("/api/jobs/{job}/releases"),
            Some(tok),
            body,
        ),
    )
    .await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::OK,
        "approve: {s}: {b}"
    );
    assert_eq!(b["applied"], true, "{b}");
}

/// Resolve `use_` of the job in the live design with `ctx`.
async fn resolve_live(
    e: &TestEnv,
    ctx: ResolveCtx<'_>,
    f: &ContractJob,
    use_: Use,
) -> Option<llm::ResolvedLlm> {
    let mut tx = begin_scoped(&e.state.db, f.tenant).await.unwrap();
    let r = llm::resolve_in(&mut tx, ctx, f.tenant, &f.job_ref(), use_, &DesignOf::Live)
        .await
        .unwrap();
    drop(tx);
    r
}

/// Mutation caught: `resolve_in` consulting `tenant_router` before (or instead of) the design's binding — e.g.
/// `Ok(Some(_)) => tenant_router(..)`, or an early return for a provisioned tenant —
/// makes the bound resolve come back as the internal router (`provider_id: None`, `local_gpu`).
#[tokio::test]
async fn fallback_listed_tenant_with_own_qualify_binding_uses_the_binding() {
    let base = fake_provider().await;
    let e = common::env_with(with_keys).await;
    let f = new_job(&e).await;
    let app = loopback_app(&e);
    let im = member(&e, f.tenant, "integrations_manager").await;
    design_contract::write(
        &e,
        &f,
        "selection",
        &DraftRef::new(),
        Some(cuttable_selection()),
    )
    .await;
    design_contract::route_required_prod_ops(&e, &f).await;
    let rm = member(&e, f.tenant, "tenant_admin").await;

    // the tenant holds its own inference-router licence (the fake bundle minter hands the token out on demand)
    common::minter::grant_shared(f.tenant, "inference-router", "tenant-router-token");
    let ip = e.state.cfg.internal_providers.clone();
    let ctx = ResolveCtx {
        platform_tenant: e.state.cfg.platform_tenant_id,
        internal: &ip,
        design: e.state.design.as_ref(),
        keyring: e.state.provider_keyring.as_deref(),
        st: Some(&e.state),
    };

    // control: unbound, qualify runs on the router under the tenant's own token
    let unbound = resolve_live(&e, ctx, &f, Use::Qualify)
        .await
        .expect("a provisioned tenant's unbound qualify uses the router");
    assert_eq!((unbound.provider_id, unbound.funding), (None, "local_gpu"));
    assert_eq!(
        unbound.key.as_ref().map(|k| k.expose()),
        Some("tenant-router-token")
    );

    // the tenant binds its own qualify provider and publishes it
    let (s, b) = send(
        &app,
        json(
            "POST",
            "/api/providers",
            Some(&im),
            json!({"name": "Own", "kind": "openai", "base_url": base, "models": ["gpt-4.1-mini"], "secret": GOOD}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let p: Uuid = b["id"].as_str().unwrap().parse().unwrap();
    let (s, b) = send(
        &app,
        json(
            "PUT",
            "/api/providers/uses/qualify",
            Some(&im),
            json!({"provider_id": p, "model": "gpt-4.1-mini"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    publish(&app, &f, &rm).await;

    let bound = resolve_live(&e, ctx, &f, Use::Qualify)
        .await
        .expect("the binding serves");
    assert_eq!(bound.provider_id, Some(p), "the tenant binding wins");
    assert_eq!(bound.funding, "tenant_byo");
    assert_eq!(bound.model, "gpt-4.1-mini");
    assert_eq!(bound.base_url, base);
    // the router lane is qualify-only: an unbound gates use stays unbound for the same listed tenant
    assert!(resolve_live(&e, ctx, &f, Use::Gates).await.is_none());
}

// ==== 2. (#44) cross-tenant: F2 replay and party_email ===============================================================

async fn job_in(owner: &PgPool, t: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, 'J', '[]', 'test') RETURNING id",
    )
    .bind(t)
    .bind(key)
    .fetch_one(owner)
    .await
    .unwrap()
}

/// Conversation `c` of job `job` with `n` stored messages (`at_ms` 1..=n, texts `<tag>1`..): their ids.
async fn conversation_with(
    owner: &PgPool,
    t: Uuid,
    job: Uuid,
    c: &str,
    tag: &str,
    n: i64,
) -> Vec<Uuid> {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel) VALUES ($1, $2, $3, 'test', 'staff')",
    )
    .bind(t)
    .bind(job)
    .bind(c)
    .execute(owner)
    .await
    .unwrap();
    let mut ids = Vec::new();
    for i in 1..=n {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, author, text, at_ms) \
             VALUES ($1, $2, $3, 'customer', 'x', $4, $5) RETURNING message_id",
        )
        .bind(t)
        .bind(job)
        .bind(c)
        .bind(format!("{tag}{i}"))
        .bind(i)
        .fetch_one(owner)
        .await
        .unwrap();
        ids.push(id);
    }
    ids
}

/// `GET path` as SSE for `sub` with `Last-Event-ID: last`: the status and, on 200, the feed.
async fn open(app: &Router, path: &str, sub: &str, last: &str) -> (StatusCode, Option<Feed>) {
    let req = Request::get(path)
        .header("authorization", format!("Bearer {}", person(sub, &[])))
        .header("accept", "text/event-stream")
        .header("last-event-id", last)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let feed = (status == StatusCode::OK).then(|| Feed::new(resp.into_body()));
    (status, feed)
}

/// Mutations caught: dropping the `tenant_isolation` / `job_scope` RLS on `conversation_message` (or reading it
/// unscoped) together with the `l.tenant_id = m.tenant_id AND l.job_id = m.job_id` join in `messages_since` — tenant
/// A's message then anchors tenant B's replay and B's `b2` (at_ms 2 > A's anchor at_ms 1) is replayed first; and
/// `conversation_or_404` read unscoped — B then opens A's conversation (200, not 404).
#[tokio::test]
async fn f2_replay_never_reads_or_anchors_on_another_tenants_messages() {
    let vars = r::api_vars("gap-f2-xtenant").await;
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let (ta, tb) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, ta, "Alpha").await;
    insert_tenant(&e.owner, tb, "Beta").await;
    insert_user(&e.owner, "u-ev-a", ta, "tenant_user").await;
    insert_user(&e.owner, "u-ev-b", tb, "tenant_user").await;
    // the same job key and conversation id in both tenants
    let ja = job_in(&e.owner, ta, "j1").await;
    let jb = job_in(&e.owner, tb, "j1").await;
    let a = conversation_with(&e.owner, ta, ja, "conv-x", "a", 3).await;
    conversation_with(&e.owner, ta, ja, "conv-a-only", "a-only", 2).await;
    conversation_with(&e.owner, tb, jb, "conv-x", "b", 2).await;

    // B asking for A's conversation id (B has none by that id): 404, whatever the anchor
    let (s, _) = open(
        &e.app,
        "/api/jobs/j1/conversations/conv-a-only/events",
        "u-ev-b",
        &format!("msg/{}", a[0]),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "B never opens A's conversation");

    // B's own conv-x with A's message id as Last-Event-ID: no replay, the first frame is B's live one
    let (s, feed) = open(
        &e.app,
        "/api/jobs/j1/conversations/conv-x/events",
        "u-ev-b",
        &format!("msg/{}", a[0]),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let mut feed = feed.unwrap();
    e.state.events.publish(Frame {
        tenant: tb,
        job_id: jb,
        conversation_id: Some("conv-x".into()),
        event: "message",
        data: json!({"conversation_id": "conv-x", "message_id": "live-b", "role": "agent", "text": "live-b"}),
    });
    let first = feed.next(WAIT).await.expect("B's live frame");
    assert_eq!(
        first.data["text"],
        json!("live-b"),
        "A's id anchored a replay into B: {first:?}"
    );

    // control: A's own anchor replays A's later messages (the replay path is live in this env)
    let (s, feed) = open(
        &e.app,
        "/api/jobs/j1/conversations/conv-x/events",
        "u-ev-a",
        &format!("msg/{}", a[0]),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let got = feed.unwrap().next(WAIT).await.expect("A's replay");
    assert_eq!(got.data["text"], json!("a2"));
    // nothing was written: B's conversation still holds exactly its own two messages
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM conversation_message WHERE tenant_id = $1 AND conversation_id = 'conv-x'",
    )
    .bind(tb)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(n, 2);
}

/// A prod conversation `id` of (`t`, `job`, key `j1`) and a case on it (as agent_app): the case id.
async fn case_of(e: &TestEnv, t: Uuid, job: Uuid, id: &str) -> String {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, 'prod', 'web_chat', 'cust-1')",
    )
    .bind(t)
    .bind(job)
    .bind(id)
    .execute(&e.owner)
    .await
    .unwrap();
    let mut tx = store::begin_job(&e.state.db, t, job).await.unwrap();
    let conv = store::conversation(&mut tx, id).await.unwrap().unwrap();
    let d = store::design_of(&mut tx, job, "j1", &conv).await.unwrap();
    let case_id = store::create_case(&mut tx, job, &conv, "Help", &d)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    case_id
}

/// A party of `t` with a verified email identity `email`, linked to `case` of `job`.
async fn email_party(owner: &PgPool, t: Uuid, job: Uuid, case: &str, email: &str) {
    let party: Uuid = sqlx::query_scalar("INSERT INTO party (tenant_id) VALUES ($1) RETURNING id")
        .bind(t)
        .fetch_one(owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO party_identity (tenant_id, channel, subject, party_id, verified, first_seen_at, last_seen_at) \
         VALUES ($1, 'email', $2, $3, true, now() - interval '1 day', now())",
    )
    .bind(t)
    .bind(email)
    .bind(party)
    .execute(owner)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE agent_case SET party_id = $3 WHERE tenant_id = $1 AND job_id = $2 AND case_id = $4",
    )
    .bind(t)
    .bind(job)
    .bind(party)
    .bind(case)
    .execute(owner)
    .await
    .unwrap();
}

/// `ops::party_email(case)` as the runner reads it: agent_runtime, both GUCs set to (`t`, `job`).
async fn party_email_as_runner(rt: &PgPool, t: Uuid, job: Uuid, case: &str) -> Option<String> {
    let mut tx = store::begin_job(rt, t, job).await.unwrap();
    let r = ops::party_email(&mut tx, case).await.unwrap();
    tx.commit().await.unwrap();
    r
}

/// `ops::party_email` filters on `case_id` alone (src/runtime/runner/ops.rs `party_email`): the tenant and job are
/// the RLS policies' (`tenant_isolation` + RESTRICTIVE `job_scope` on `agent_case`, `tenant_isolation` on
/// `party_identity`). Mutation caught: dropping either policy (or the runner reading unscoped) — tenant B's, or
/// tenant A's other job's, `CS-1` then resolves to A's customer email, so B's ticket would be raised for A's customer.
#[tokio::test]
async fn party_email_never_reads_another_tenants_or_jobs_case() {
    let e = common::env_with(|_| {}).await;
    let (ta, tb) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, ta, "Alpha").await;
    insert_tenant(&e.owner, tb, "Beta").await;
    let ja = job_in(&e.owner, ta, "j1").await;
    let ja2 = job_in(&e.owner, ta, "j2").await;
    let jb = job_in(&e.owner, tb, "j1").await;
    let ca = case_of(&e, ta, ja, "conv-a").await;
    let ca2 = case_of(&e, ta, ja2, "conv-a2").await;
    let cb = case_of(&e, tb, jb, "conv-b").await;
    // the collision this test is about: the same case id in every scope
    assert_eq!((ca.as_str(), ca2.as_str()), (cb.as_str(), cb.as_str()));
    assert!(ca.starts_with("CS-"), "{ca}");
    email_party(&e.owner, ta, ja, &ca, "alice@alpha.example").await;

    let rt = common::runtime_pool(&e.owner).await;
    // control: A's own case reads A's party email
    assert_eq!(
        party_email_as_runner(&rt, ta, ja, &ca).await.as_deref(),
        Some("alice@alpha.example")
    );
    // B (no party on its case) with the same id: nothing, never A's customer
    assert_eq!(party_email_as_runner(&rt, tb, jb, &cb).await, None);
    // A's other job with the same id: nothing either (job_scope)
    assert_eq!(party_email_as_runner(&rt, ta, ja2, &ca2).await, None);
    // and B's read touched nothing of A's
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM agent_case WHERE tenant_id = $1 AND party_id IS NOT NULL",
    )
    .bind(tb)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(n, 0);
}

// ==== 3 + 4. (#44/#45) the handover policy and calendar a PROD case reads ============================================

fn t(fr: i64, res: i64) -> Value {
    json!({"first_response_min": fr, "resolve_min": res})
}

fn live_targets() -> Value {
    json!({"high": t(15, 240), "normal": t(60, 1440), "low": t(240, 4320)})
}

fn draft_targets() -> Value {
    json!({"high": t(5, 120), "normal": t(30, 720), "low": t(120, 2880)})
}

fn at(s: &str) -> DateTime<Utc> {
    s.parse().unwrap()
}

/// Every day, all day, in `tz`.
fn always(tz: &str, use_tenant_calendar: bool) -> Value {
    let w = json!([{"from": "00:00", "to": "24:00"}]);
    json!({
        "timezone": tz,
        "weekly": {"mon": w, "tue": w, "wed": w, "thu": w, "fri": w, "sat": w, "sun": w},
        "use_tenant_calendar": use_tenant_calendar,
        "out_of_hours": "queue",
    })
}

/// Monday to Friday 09:00–17:00 in `tz`.
fn weekdays(tz: &str) -> Value {
    let w = json!([{"from": "09:00", "to": "17:00"}]);
    json!({
        "timezone": tz,
        "weekly": {"mon": w, "tue": w, "wed": w, "thu": w, "fri": w},
        "use_tenant_calendar": false,
        "out_of_hours": "queue",
    })
}

fn job(c: &Ctx) -> JobRef {
    JobRef {
        id: c.job,
        key: "j1".into(),
    }
}

async fn save_policy(c: &Ctx, targets: Value, hours: Value) {
    let (s, b) = c
        .call(
            "PUT",
            "/api/jobs/j1/handover/policy",
            "u-mgr",
            json!({"targets": targets, "hours": hours}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn save_calendar(c: &Ctx, body: Value) {
    let (s, b) = c.call("PUT", "/api/tenant/calendar", "u-mgr", body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Release `cfg-2026-10-22.1` of j1 as its live release covering `kinds`, its snapshot holding `rows`
/// (`(key, kind, value)`), owner pool (pattern tests/b4/hours.rs `release_live`).
async fn release_live(c: &Ctx, kinds: &[&str], rows: &[(&str, &str, Value)]) {
    let rel = "cfg-2026-10-22.1";
    let kinds: Vec<String> = kinds.iter().map(|k| k.to_string()).collect();
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, released_by, released_at,
               snapshot_kinds)
           VALUES ($1, $2, $3, $4, $4, '{"sections": []}', '{}', '[]', '{}', '{}', '[]', 'current', 'u-rm', now(),
               'u-rm', now(), $5)"#,
    )
    .bind(c.t)
    .bind(c.job)
    .bind(rel)
    .bind(format!("sha256:{}", "d".repeat(64)))
    .bind(&kinds)
    .execute(&c.e.owner)
    .await
    .unwrap();
    for (key, kind, v) in rows {
        sqlx::query(
            "INSERT INTO release_snapshot (tenant_id, job_id, release_id, key, kind, ref, snap) \
             VALUES ($1, $2, $3, $4, $5, '{}', $6)",
        )
        .bind(c.t)
        .bind(c.job)
        .bind(rel)
        .bind(*key)
        .bind(*kind)
        .bind(json!({ "v": v }))
        .execute(&c.e.owner)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(c.job)
        .bind(rel)
        .execute(&c.e.owner)
        .await
        .unwrap();
}

/// A conversation `id` in `env` and a case on it: (case id, the case's design).
async fn case_in(c: &Ctx, id: &str, env: &str) -> (String, CaseDesign) {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, $3, $4, 'web_chat', 'cust-1')",
    )
    .bind(c.t)
    .bind(c.job)
    .bind(id)
    .bind(env)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let mut tx = store::begin_job(&c.e.state.db, c.t, c.job).await.unwrap();
    let conv = store::conversation(&mut tx, id).await.unwrap().unwrap();
    let d = store::design_of(&mut tx, c.job, "j1", &conv).await.unwrap();
    let case_id = store::create_case(&mut tx, c.job, &conv, "Help", &d)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (case_id, d)
}

/// The scope of `d` closed at `now`: its next open time (`None` = open), read as the runner (agent_runtime).
async fn next_open(
    c: &Ctx,
    rt: &PgPool,
    d: &CaseDesign,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let mut tx = store::begin_job(rt, c.t, c.job).await.unwrap();
    let closed = agent_cases::closed_at(&mut tx, &job(c), d, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    closed.map(|x| x.next_open)
}

/// The job's SLA targets in `of` (as the runner), as JSON.
async fn targets_in(c: &Ctx, rt: &PgPool, of: DesignOf) -> Option<Value> {
    let mut tx = store::begin_job(rt, c.t, c.job).await.unwrap();
    let t = policy::read(&mut tx, &job(c), &of).await.unwrap();
    tx.commit().await.unwrap();
    t.map(policy::Targets::to_value)
}

/// Mutation caught: `policy::hours_of` reading the calendar with `&DesignOf::Draft` (or `calendar::read` ignoring
/// `of`) — prod is then closed on the DRAFT holiday (24 Dec) and open on the LIVE one (25 Dec); `policy::read`
/// ignoring `of` — prod's SLA targets come back as the draft's.
#[tokio::test]
async fn prod_handover_reads_the_live_calendar_and_policy_not_the_draft() {
    let c = conv::setup().await;
    // live: always open, uses the tenant calendar, whose released holiday is 25 December
    release_live(
        &c,
        &["handover_policy", "tenant_calendar"],
        &[
            (
                "handover_policy",
                "handover_policy",
                json!({"targets": live_targets(), "hours": always("Europe/Amsterdam", true)}),
            ),
            (
                "tenant-calendar",
                "tenant_calendar",
                json!({"timezone": "Europe/Amsterdam", "holidays": [{"date": "2026-12-25", "label": "Christmas"}]}),
            ),
        ],
    )
    .await;
    // draft: other targets, and a draft calendar whose only holiday is 24 December
    save_policy(&c, draft_targets(), always("Europe/Amsterdam", true)).await;
    save_calendar(
        &c,
        json!({"timezone": "Europe/Amsterdam", "holidays": [{"date": "2026-12-24"}]}),
    )
    .await;

    let (prod_case, prod) = case_in(&c, "conv-p1", "prod").await;
    let (_, test) = case_in(&c, "conv-t1", "test").await;
    assert!(matches!(prod, CaseDesign::Live { .. }), "{prod:?}");
    assert!(matches!(test, CaseDesign::Draft { .. }), "{test:?}");
    let rt = common::runtime_pool(&c.e.owner).await;
    let dec24 = at("2026-12-24T12:00:00Z");
    let dec25 = at("2026-12-25T12:00:00Z");

    // prod: the live holiday closes it until 26 Dec 00:00 Amsterdam (23:00 UTC); the draft holiday does not
    assert_eq!(
        next_open(&c, &rt, &prod, dec25).await,
        Some(at("2026-12-25T23:00:00Z")),
        "prod runs the released calendar"
    );
    assert_eq!(
        next_open(&c, &rt, &prod, dec24).await,
        None,
        "the draft holiday never reaches prod"
    );
    // control: test runs the draft calendar
    assert_eq!(
        next_open(&c, &rt, &test, dec24).await,
        Some(at("2026-12-24T23:00:00Z"))
    );
    assert_eq!(next_open(&c, &rt, &test, dec25).await, None);

    // SLA targets: prod reads the released policy, test the draft
    assert_eq!(
        targets_in(&c, &rt, DesignOf::Live).await,
        Some(live_targets())
    );
    assert_eq!(
        targets_in(&c, &rt, DesignOf::Draft).await,
        Some(draft_targets())
    );

    // the runner's prod handover on the live holiday is queued out of hours until the live calendar reopens
    let mut tx = store::begin_job(&rt, c.t, c.job).await.unwrap();
    let o = agent_cases::create_at(
        &mut tx,
        &job(&c),
        &prod_case,
        "conv-p1",
        "handover",
        &Map::new(),
        &prod,
        dec25,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(o.outcome_code.as_deref(), Some("outside_hours"), "{o:?}");
    let clock: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT clock_starts_at FROM agent_handover_items WHERE tenant_id = $1 AND case_id = $2",
    )
    .bind(c.t)
    .bind(&prod_case)
    .fetch_one(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(clock, Some(at("2026-12-25T23:00:00Z")));
}

/// What a PROD case (`DesignOf::Live`) reads for `handover_policy` when no release covers it:
/// (a) never released: the first draft save takes the job's baseline BEFORE the write (`design::track` →
///     `hooks::ensure_job` → `snapshot::ensure_baseline`), and that baseline covers the kind with no row — so prod
///     reads NOTHING (no SLA, always staffed; D54: a save is never live), not the draft.
/// (b) released, but the live release's `snapshot_kinds` lacks the kind (what a failed `design::coverage::backfill`
///     leaves until the next boot): PINS CURRENT BEHAVIOUR (kept, not a contract) — `design::read::value` falls through
///     to the working rows (`t.get`), so prod runs the DRAFT policy and hours, warned + counted
///     (`agent_design_live_read_uncovered_total`).
/// Control: once the release covers the kind (with no row), prod has no SLA and is always staffed.
/// Mutation caught: `ensure_baseline` skipped by `track` (or `value` ignoring `baseline_kinds`) flips (a) to the
/// draft; `value_in` treating an uncovered kind as covered flips (b) to `None`.
#[tokio::test]
async fn pinned_prod_reads_draft_policy_when_live_does_not_cover_it() {
    let c = conv::setup().await;
    let rt = common::runtime_pool(&c.e.owner).await;
    let baseline: Option<Vec<String>> =
        sqlx::query_scalar("SELECT baseline_kinds FROM jobs WHERE id = $1")
            .bind(c.job)
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert_eq!(baseline, None, "the fixture job has no baseline taken");
    // a draft save only: weekday hours
    save_policy(&c, draft_targets(), weekdays("Europe/Amsterdam")).await;
    let thursday_night = at("2026-10-22T22:00:00Z");

    // (a) never released: the save took the baseline first, and it covers handover_policy with no row
    let baseline: Option<Vec<String>> =
        sqlx::query_scalar("SELECT baseline_kinds FROM jobs WHERE id = $1")
            .bind(c.job)
            .fetch_one(&c.e.owner)
            .await
            .unwrap();
    assert!(
        baseline
            .as_ref()
            .is_some_and(|k| k.iter().any(|k| k == "handover_policy")),
        "the first draft save takes the baseline before the write: {baseline:?}"
    );
    let (_, prod) = case_in(&c, "conv-p1", "prod").await;
    assert_eq!(
        prod,
        CaseDesign::Live { release_id: None },
        "a prod case runs Live"
    );
    assert_eq!(
        targets_in(&c, &rt, DesignOf::Live).await,
        None,
        "never released: prod reads the pre-save baseline (no SLA), never the draft"
    );
    assert_eq!(
        next_open(&c, &rt, &prod, thursday_night).await,
        None,
        "never released: no live hours, so prod is always staffed (the draft weekday hours do not reach it)"
    );
    // control: the test env (Draft) does run the saved policy
    assert_eq!(
        targets_in(&c, &rt, DesignOf::Draft).await,
        Some(draft_targets())
    );

    // (b) a live release that does not cover handover_policy (backfill failed): falls through to the working rows
    release_live(&c, &["tenant_calendar"], &[]).await;
    let (_, prod2) = case_in(&c, "conv-p2", "prod").await;
    assert!(
        matches!(
            &prod2,
            CaseDesign::Live {
                release_id: Some(_)
            }
        ),
        "{prod2:?}"
    );
    assert_eq!(
        targets_in(&c, &rt, DesignOf::Live).await,
        Some(draft_targets()),
        "CURRENT: an uncovered kind in the live release reads the draft (warned + counted)"
    );
    // Thu 22:00 UTC = Fri 00:00 Amsterdam (CEST); the draft weekday hours reopen Fri 09:00 CEST = 07:00 UTC
    assert_eq!(
        next_open(&c, &rt, &prod2, thursday_night).await,
        Some(at("2026-10-23T07:00:00Z")),
        "CURRENT: an uncovered kind in the live release runs the draft hours"
    );

    // control: once the live release covers the kind (with no row), prod has no SLA and is always staffed
    sqlx::query(
        "UPDATE job_release SET snapshot_kinds = array_append(snapshot_kinds, 'handover_policy') \
          WHERE job_id = $1 AND release_id = 'cfg-2026-10-22.1'",
    )
    .bind(c.job)
    .execute(&c.e.owner)
    .await
    .unwrap();
    assert_eq!(targets_in(&c, &rt, DesignOf::Live).await, None);
    assert_eq!(next_open(&c, &rt, &prod2, thursday_night).await, None);
}
