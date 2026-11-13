//! B2 T12: apply, rollback, re-apply, follow-live, conflicts on apply and the rollback preview (spec B2 §5, D54/D62/
//! D69) — `POST /api/jobs/{job}/releases/{id}/apply`, `GET …/releases/{id}/rollback-preview`, and approve with
//! `apply: true` (§5 step 13). Real Postgres 18 (one container for this binary, a fresh database per test), through
//! the RLS runtime role `agent_app` and the real router, on a job created through `POST /api/jobs` from
//! `job/customer_care_agent@v4` (the B2 creation hook ran: target row + baseline).
//!
//! The design changes are `languages` saves (`PUT …/languages`, kind `languages`); the tenant-wide case (D69) runs on
//! a registry with one extra test-only tenant kind (`scratch_tenant`, `Registry::register_scratch`).
use crate::b5::support as b5_support;
use crate::common;

use std::sync::{Arc, Mutex};

use agent_api::design::{
    self, DesignOf, DesignTarget, DraftRef, DraftWrite, Label, Place, Scope, Snap,
};
use agent_api::error::ApiResult;
use agent_api::platform::templates::TemplatePin;
use agent_api::readiness::{
    AreaKey, AreaStatus, Assessment, MissingItem, Readiness, ReadinessContributor, ReadinessCx,
    board,
};
use agent_api::release::conflicts as conflict_engine;
use axum::Router;
use axum::http::StatusCode;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use common::design_contract::ContractJob;
use common::{TestEnv, get, insert_user, json as req, person, send};

/// Every role in the contract tenant.
const ADMIN: &str = "u-apply-admin";

async fn setup() -> (TestEnv, ContractJob, String) {
    let e = common::env().await;
    let f = b5_support::new_job(&e).await;
    insert_user(&e.owner, ADMIN, f.tenant, "tenant_admin").await;
    let tok = person(ADMIN, &[]);
    (e, f, tok)
}

fn uri(f: &ContractJob, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", f.job.job_key)
}

/// A complete selection of the seed options: compiles with no violation.
fn sel() -> Value {
    json!({
        "profile": "retail_ecommerce",
        "industry": "retail",
        "departments": ["industry_dept/retail/customer_service"],
        "areas": ["order-support"],
        "dimensions": {"channel": ["chat"]}, "region": "eu",
    })
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

/// Save [`sel`] (the first save); returns the new `selection_updated_at`.
async fn put_target(app: &Router, f: &ContractJob, tok: &str) -> Value {
    let body = json!({"selection": sel(), "expected_updated_at": null});
    let (s, b) = send(app, req("PUT", &uri(f, "target"), Some(tok), body)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["selection_updated_at"].clone()
}

async fn put_languages(app: &Router, f: &ContractJob, tok: &str, langs: Value) {
    let body = json!({"languages": langs});
    let (s, b) = send(app, req("PUT", &uri(f, "languages"), Some(tok), body)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Approve (cut); `apply` = approve and apply in one transaction. Returns `(status, ApproveResult)`.
async fn approve(app: &Router, f: &ContractJob, tok: &str, at: &Value, apply: bool) -> Value {
    let body = json!({"apply": apply, "selection_seen_at": at});
    let (s, b) = send(app, req("POST", &uri(f, "releases"), Some(tok), body)).await;
    assert!(s == StatusCode::CREATED || s == StatusCode::OK, "{s}: {b}");
    assert_eq!(b["applied"], apply, "apply: true applies (§5 step 13): {b}");
    b
}

/// Cut a release (apply false); returns its id.
async fn cut(app: &Router, f: &ContractJob, tok: &str, at: &Value) -> String {
    let b = approve(app, f, tok, at, false).await;
    assert_eq!(b["created"], true, "{b}");
    b["release"]["id"].as_str().expect("release id").to_string()
}

async fn apply_on(app: &Router, f: &ContractJob, tok: &str, id: &str) -> (StatusCode, Value) {
    let path = uri(f, &format!("releases/{id}/apply"));
    send(app, req("POST", &path, Some(tok), json!({}))).await
}

/// Apply `id`, expecting 200 `applied: true`.
async fn apply(app: &Router, f: &ContractJob, tok: &str, id: &str) -> Value {
    let (s, b) = apply_on(app, f, tok, id).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["applied"], true, "{b}");
    assert_eq!(b["release"]["id"], id, "{b}");
    assert_eq!(b["release"]["state"], "current", "{b}");
    b
}

async fn preview(app: &Router, f: &ContractJob, tok: &str, id: &str) -> (StatusCode, Value) {
    let path = uri(f, &format!("releases/{id}/rollback-preview"));
    send(app, get(&path, Some(tok))).await
}

async fn count(e: &TestEnv, sql: &'static str, f: &ContractJob) -> i64 {
    sqlx::query_scalar(sql)
        .bind(f.job.job_id)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn audits(e: &TestEnv, f: &ContractJob, action: &str) -> Vec<Value> {
    sqlx::query_scalar("SELECT details FROM audit_event WHERE job = $1 AND action = $2 ORDER BY id")
        .bind(f.job.job_key.as_str())
        .bind(action)
        .fetch_all(&e.owner)
        .await
        .unwrap()
}

async fn languages(e: &TestEnv, f: &ContractJob) -> Vec<String> {
    sqlx::query_scalar("SELECT languages FROM jobs WHERE id = $1")
        .bind(f.job.job_id)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn current(e: &TestEnv, f: &ContractJob) -> Option<String> {
    sqlx::query_scalar("SELECT current_release_id FROM jobs WHERE id = $1")
        .bind(f.job.job_id)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn state(e: &TestEnv, f: &ContractJob, id: &str) -> String {
    sqlx::query_scalar("SELECT state FROM job_release WHERE job_id = $1 AND release_id = $2")
        .bind(f.job.job_id)
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

#[derive(Debug, sqlx::FromRow)]
struct Published {
    id: i64,
    key: String,
    after: Option<Value>,
    published_in: String,
    rolled_back: Option<Value>,
}

async fn published(e: &TestEnv, f: &ContractJob) -> Vec<Published> {
    sqlx::query_as::<_, Published>(
        "SELECT id, key, after, published_in, rolled_back FROM published_change WHERE job_id = $1 ORDER BY id",
    )
    .bind(f.job.job_id)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

/// R1 = languages [en, de] (approved and applied in one call), R2 = [en, de, fr] (cut, then applied). Returns
/// `(R1, R2, selection_seen_at)`.
async fn two_releases(app: &Router, f: &ContractJob, tok: &str) -> (String, String, Value) {
    let at = put_target(app, f, tok).await;
    put_languages(app, f, tok, json!(["en", "de"])).await;
    let b = approve(app, f, tok, &at, true).await;
    let r1 = b["release"]["id"].as_str().unwrap().to_string();
    assert_eq!(b["release"]["state"], "current", "{b}");
    put_languages(app, f, tok, json!(["en", "de", "fr"])).await;
    let r2 = cut(app, f, tok, &at).await;
    apply(app, f, tok, &r2).await;
    (r1, r2, at)
}

// ---- apply --------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn apply_makes_snapshot_live_and_publishes_bundle() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e.app, &f, &tok).await;
    put_languages(&e.app, &f, &tok, json!(["en", "de"])).await;
    let id = cut(&e.app, &f, &tok, &at).await;
    let drafts = count(
        &e,
        "SELECT count(*) FROM release_bundled_draft WHERE job_id = $1",
        &f,
    )
    .await;
    assert_eq!(drafts, 2, "the cut bundles the selection and the languages");
    assert_eq!(current(&e, &f).await, None);

    let b = apply(&e.app, &f, &tok, &id).await;
    assert_eq!(b["previous"], Value::Null);
    assert_eq!(b["published"], 2, "{b}");
    assert_eq!(b["direction"], "apply");
    assert_eq!(b["rolled_back"], 0);
    assert_eq!(b["reapplied"], 0);
    assert_eq!(b["conflicts_open"], 0);
    assert_eq!(b["release"]["released_by"], ADMIN, "{b}");

    // live = the snapshot; the job runs the release's pins
    assert_eq!(current(&e, &f).await.as_deref(), Some(id.as_str()));
    let pins: (Value, Value) = sqlx::query_as(
        "SELECT j.pins, r.pins FROM jobs j JOIN job_release r ON r.job_id = j.id \
          WHERE j.id = $1 AND r.release_id = j.current_release_id",
    )
    .bind(f.job.job_id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(pins.0, pins.1);

    // the job read models report the applied release (acceptance: `current_release` was always null)
    let hash: String =
        sqlx::query_scalar("SELECT hash FROM job_release WHERE job_id = $1 AND release_id = $2")
            .bind(f.job.job_id)
            .bind(&id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    let want = json!({"id": id, "hash": hash});
    let (s, one) = send(
        &e.app,
        get(&format!("/api/jobs/{}", f.job.job_key), Some(&tok)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{one}");
    assert_eq!(one["current_release"], want, "{one}");
    let (s, list) = send(&e.app, get("/api/jobs", Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let item = list["items"]
        .as_array()
        .and_then(|a| a.iter().find(|j| j["key"] == f.job.job_key.as_str()))
        .expect("the job is listed");
    assert_eq!(item["current_release"], want, "{list}");

    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let live = design::value(
        &mut tx,
        &e.state.design,
        &f.job_ref(),
        &DesignOf::Live,
        "languages",
        &DraftRef::new(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(live, Some(json!(["en", "de"])));

    // the ledger is empty: every bundled change is published, stamped with this release
    assert_eq!(
        count(
            &e,
            "SELECT count(*) FROM draft_ledger WHERE job_id = $1",
            &f
        )
        .await,
        0
    );
    let rows = published(&e, &f).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows.iter()
            .all(|p| p.published_in == id && p.rolled_back.is_none())
    );
    let langs = rows
        .iter()
        .find(|p| p.key == "languages")
        .expect("languages row");
    assert_eq!(langs.after, Some(json!({"v": ["en", "de"]})));

    // audit: one release.apply, one job.draft_published per published change
    let applied = audits(&e, &f, "release.apply").await;
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0]["published"], 2);
    let pubs = audits(&e, &f, "job.draft_published").await;
    assert_eq!(pubs.len(), 2);
    assert!(pubs.iter().all(|d| d["release"] == id.as_str()));

    // a plain apply publishing its own bundle moved no live value under an unpublished change: no rollback-log row.
    // (Mutation check: follow_live ignoring `publishing` flags the bundled rows as live_moved → this row appears.)
    assert_eq!(
        count(
            &e,
            "SELECT count(*) FROM release_rollback WHERE job_id = $1",
            &f
        )
        .await,
        0,
        "no draft moved, nothing undone or re-applied"
    );
    let audit_dirs = audits(&e, &f, "release.rollback").await;
    assert!(audit_dirs.is_empty());
}

#[tokio::test]
async fn change_saved_after_cut_survives_apply() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e.app, &f, &tok).await;
    put_languages(&e.app, &f, &tok, json!(["en", "de"])).await;
    let id = cut(&e.app, &f, &tok, &at).await;
    // saved after the cut: a setting override
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &uri(&f, "settings"),
            Some(&tok),
            json!({"stance": "Hello from us."}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");

    apply(&e.app, &f, &tok, &id).await;
    let keys: Vec<String> =
        sqlx::query_scalar("SELECT key FROM draft_ledger WHERE job_id = $1 ORDER BY key")
            .bind(f.job.job_id)
            .fetch_all(&e.owner)
            .await
            .unwrap();
    assert_eq!(keys, ["setting:stance"], "only the later change stays");
    let value: Option<Option<String>> = sqlx::query_scalar(
        "SELECT value FROM job_setting_override WHERE job_id = $1 AND key = 'stance'",
    )
    .bind(f.job.job_id)
    .fetch_optional(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        value,
        Some(Some("Hello from us.".to_string())),
        "the draft keeps it"
    );
    let rows = published(&e, &f).await;
    assert!(rows.iter().all(|p| p.key != "setting:stance"), "{rows:?}");
}

#[tokio::test]
async fn already_current_is_noop() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e.app, &f, &tok).await;
    let b = approve(&e.app, &f, &tok, &at, true).await;
    assert_eq!(b["created"], true);
    let id = b["release"]["id"].as_str().unwrap().to_string();

    let (s, b) = apply_on(&e.app, &f, &tok, &id).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["applied"], false);
    assert_eq!(b["previous"], Value::Null);
    assert_eq!(b["conflicts_open"], 0);
    assert!(b.get("published").is_none(), "{b}");
    assert_eq!(audits(&e, &f, "release.apply").await.len(), 1);

    // approve again with apply and nothing new: 409 nothing_to_publish
    let body = json!({"apply": true, "selection_seen_at": at});
    let (s, b) = send(&e.app, req("POST", &uri(&f, "releases"), Some(&tok), body)).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "nothing_to_publish");
}

#[tokio::test]
async fn approve_apply_true_applies_a_new_and_a_reused_cut() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e.app, &f, &tok).await;
    let id = cut(&e.app, &f, &tok, &at).await;
    // the same draft again with apply: the newest release is reused and applied
    let b = approve(&e.app, &f, &tok, &at, true).await;
    assert_eq!(b["created"], false, "{b}");
    assert_eq!(b["release"]["id"], id.as_str());
    assert_eq!(b["release"]["state"], "current");
    assert_eq!(current(&e, &f).await.as_deref(), Some(id.as_str()));
}

#[tokio::test]
async fn apply_not_ready_409_reads_release_design() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e.app, &f, &tok).await;
    let id = cut(&e.app, &f, &tok, &at).await;
    let fake = Arc::new(Blocks {
        seen: Mutex::new(Vec::new()),
    });
    let mut r = Readiness::default();
    r.register(fake.clone());
    let mut st = e.state.clone();
    st.readiness = Arc::new(r);
    let app = agent_api::router(st);

    let (s, b) = apply_on(&app, &f, &tok, &id).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "release_not_ready");
    assert_eq!(
        b["error"]["message"],
        format!("{id} is not ready: strata_rag.embed.")
    );
    assert_eq!(b["error"]["blocking"][0]["code"], "slot_not_deployed");
    assert_eq!(
        *fake.seen.lock().unwrap(),
        [DesignOf::Release(id.clone())],
        "the gate reads the release's own design"
    );
    assert_eq!(state(&e, &f, &id).await, "approved", "nothing applied");
    assert_eq!(current(&e, &f).await, None);
}

#[tokio::test]
async fn integrity_error_on_tampered_bundle() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e.app, &f, &tok).await;
    let id = cut(&e.app, &f, &tok, &at).await;
    sqlx::raw_sql("ALTER TABLE job_release DISABLE TRIGGER job_release_immutable")
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE job_release SET bundle = bundle || '{\"tampered\": true}' WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(&id)
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::raw_sql("ALTER TABLE job_release ENABLE TRIGGER job_release_immutable")
        .execute(&e.owner)
        .await
        .unwrap();

    let (s, b) = apply_on(&e.app, &f, &tok, &id).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR, "{b}");
    assert_eq!(code(&b), "internal");
    let rows: Vec<(String, Value)> = sqlx::query_as(
        "SELECT status, details FROM audit_event WHERE job = $1 AND action = 'release.integrity_error'",
    )
    .bind(f.job.job_key.as_str())
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0].0, "failure");
    assert_eq!(rows[0].1["code"], "integrity_error");
    assert_eq!(rows[0].1["release_id"], id.as_str());
    assert_eq!(state(&e, &f, &id).await, "approved", "nothing applied");
    assert!(audits(&e, &f, "release.apply").await.is_empty());
}

// ---- rollback / re-apply ------------------------------------------------------------------------------------------

#[tokio::test]
async fn rollback_restores_older_snapshot_and_working_rows_follow() {
    let (e, f, tok) = setup().await;
    let (r1, r2, _) = two_releases(&e.app, &f, &tok).await;
    assert_eq!(languages(&e, &f).await, ["en", "de", "fr"]);

    let b = apply(&e.app, &f, &tok, &r1).await;
    assert_eq!(b["direction"], "rollback", "{b}");
    assert_eq!(b["previous"], r2.as_str());
    assert_eq!(b["rolled_back"], 1, "{b}");
    assert_eq!(b["reapplied"], 0);
    assert_eq!(b["published"], 0, "a rollback publishes nothing");

    assert_eq!(current(&e, &f).await.as_deref(), Some(r1.as_str()));
    assert_eq!(state(&e, &f, &r2).await, "applied_earlier");
    assert_eq!(
        languages(&e, &f).await,
        ["en", "de"],
        "a place with no unpublished change follows live"
    );
    assert_eq!(
        count(
            &e,
            "SELECT count(*) FROM draft_ledger WHERE job_id = $1",
            &f
        )
        .await,
        0
    );

    // R2's published change is out of Production, marked with the rollback
    let rows = published(&e, &f).await;
    let undone: Vec<&Published> = rows.iter().filter(|p| p.rolled_back.is_some()).collect();
    assert_eq!(undone.len(), 1, "{rows:?}");
    assert_eq!(undone[0].published_in, r2);
    assert_eq!(undone[0].key, "languages");
    let mark = undone[0].rolled_back.as_ref().unwrap();
    assert_eq!(mark["to"], r1.as_str());
    assert_eq!(mark["by"], ADMIN);
    let (log_id, dir, undone_ids): (i64, String, Vec<i64>) = sqlx::query_as(
        "SELECT id, direction, undone FROM release_rollback WHERE job_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(f.job.job_id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(dir, "rollback");
    assert_eq!(undone_ids, [undone[0].id]);
    assert_eq!(mark["rollback"], format!("rb-{log_id}"));

    // audit
    let rb = audits(&e, &f, "release.rollback").await;
    assert_eq!(rb.len(), 1);
    assert_eq!(rb[0]["from"], r2.as_str());
    assert_eq!(rb[0]["rolled_back"], 1);
    assert_eq!(audits(&e, &f, "job.change_rolled_back").await.len(), 1);
    assert_eq!(audits(&e, &f, "release.apply").await.len(), 3);

    // the detail shows the rollback that took Production away from R2
    let (s, d) = send(&e.app, get(&uri(&f, &format!("releases/{r2}")), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert_eq!(d["rolled_back"]["to"], r1.as_str(), "{d}");
}

#[tokio::test]
async fn rollback_flags_live_moved_on_unpublished_change() {
    let (e, f, tok) = setup().await;
    let (r1, _r2, _) = two_releases(&e.app, &f, &tok).await;
    put_languages(&e.app, &f, &tok, json!(["en"])).await;

    apply(&e.app, &f, &tok, &r1).await;
    assert_eq!(
        languages(&e, &f).await,
        ["en"],
        "an unpublished change keeps its draft value"
    );
    let moved: Option<Value> = sqlx::query_scalar(
        "SELECT live_moved FROM draft_ledger WHERE job_id = $1 AND key = 'languages'",
    )
    .bind(f.job.job_id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let moved = moved.expect("live_moved is set");
    assert_eq!(moved["release"], r1.as_str());
    assert_eq!(moved["was"], json!({"v": ["en", "de", "fr"]}));
    assert!(moved["at"].is_string());
    let draft_moved: Vec<String> = sqlx::query_scalar(
        "SELECT draft_moved FROM release_rollback WHERE job_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(f.job.job_id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(draft_moved, ["languages"]);
    // the draft page reads it (T14 shape)
    let (s, d) = send(&e.app, get(&uri(&f, "draft"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    assert!(d.to_string().contains("\"was\":\"en, de, fr\""), "{d}");
}

#[tokio::test]
async fn reapply_direction() {
    let (e, f, tok) = setup().await;
    let (r1, r2, at) = two_releases(&e.app, &f, &tok).await;
    apply(&e.app, &f, &tok, &r1).await;

    let b = apply(&e.app, &f, &tok, &r2).await;
    assert_eq!(b["direction"], "reapply", "{b}");
    assert_eq!(b["previous"], r1.as_str());
    assert_eq!(b["reapplied"], 1, "{b}");
    assert_eq!(b["rolled_back"], 0);
    assert_eq!(languages(&e, &f).await, ["en", "de", "fr"]);
    assert!(
        published(&e, &f)
            .await
            .iter()
            .all(|p| p.rolled_back.is_none()),
        "re-applied changes are live again"
    );
    assert_eq!(audits(&e, &f, "release.reapply").await.len(), 1);
    assert_eq!(audits(&e, &f, "job.change_reapplied").await.len(), 1);
    let (released_by, applied_by): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT released_by, applied_by FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(&r2)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(released_by.as_deref(), Some(ADMIN));
    assert_eq!(applied_by.as_deref(), Some(ADMIN));

    // a cut after two rollback-log rows stamps rollbacks_seen = 2 (T11 review I1)
    put_languages(&e.app, &f, &tok, json!(["en", "nl"])).await;
    let r3 = cut(&e.app, &f, &tok, &at).await;
    let seen: i32 = sqlx::query_scalar(
        "SELECT rollbacks_seen FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(&r3)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(seen, 2);
}

#[tokio::test]
async fn rollback_preview_matches_apply_effects() {
    let (e, f, tok) = setup().await;
    let (r1, r2, _) = two_releases(&e.app, &f, &tok).await;
    put_languages(&e.app, &f, &tok, json!(["en"])).await;

    let (s, b) = preview(&e.app, &f, &tok, &r2).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "already_current");
    let (s, b) = preview(&e.app, &f, &tok, "cfg-2020-01-01.1").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "release_not_found");

    let (s, p) = preview(&e.app, &f, &tok, &r1).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["target"], r1.as_str());
    assert_eq!(p["current"], r2.as_str());
    assert_eq!(p["direction"], "rollback");
    assert_eq!(p["selection_change"], Value::Null);
    assert_eq!(p["pins"], json!([]));
    assert_eq!(p["missing"], json!([]));
    assert!(p["bundle_now"].as_array().is_some_and(|a| !a.is_empty()));
    assert_eq!(p["bundle_now"], p["bundle_after"]);
    let undone = p["undone"].as_array().expect("undone");
    assert_eq!(undone.len(), 1, "{p}");
    assert_eq!(undone[0]["key"], "languages");
    assert_eq!(undone[0]["now"], "en, de, fr");
    assert_eq!(undone[0]["after"], "en, de");
    assert_eq!(p["draft"].as_array().map(Vec::len), Some(1), "{p}");
    assert_eq!(p["draft"][0]["draft"], "en");
    assert_eq!(p["draft"][0]["matches_live"], false);
    assert_eq!(p["design_changes"], 1, "{p}");
    // read only
    assert_eq!(current(&e, &f).await.as_deref(), Some(r2.as_str()));

    let b = apply(&e.app, &f, &tok, &r1).await;
    assert_eq!(b["direction"], p["direction"]);
    assert_eq!(b["rolled_back"], json!(undone.len()));
    assert_eq!(
        b["reapplied"],
        json!(p["reapplied"].as_array().unwrap().len())
    );
    let draft_moved: Vec<String> = sqlx::query_scalar(
        "SELECT draft_moved FROM release_rollback WHERE job_id = $1 ORDER BY id DESC LIMIT 1",
    )
    .bind(f.job.job_id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        json!(draft_moved.len()),
        json!(p["draft"].as_array().unwrap().len())
    );

    // and the way back previews the re-apply apply then does
    let (s, p) = preview(&e.app, &f, &tok, &r2).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(p["direction"], "reapply");
    let b = apply(&e.app, &f, &tok, &r2).await;
    assert_eq!(
        b["reapplied"],
        json!(p["reapplied"].as_array().unwrap().len())
    );
    assert_eq!(b["reapplied"], 1);
}

// ---- D69: tenant-wide kinds ---------------------------------------------------------------------------------------

const SCRATCH_DDL: &str = r#"
CREATE TABLE scratch_tenant_item (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    name      text NOT NULL,
    val       jsonb NOT NULL,
    PRIMARY KEY (tenant_id, name)
);
ALTER TABLE scratch_tenant_item ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON scratch_tenant_item FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON scratch_tenant_item TO agent_app;
"#;

fn name(r: &DraftRef) -> String {
    r.get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

fn name_ref(n: &str) -> DraftRef {
    let mut m = DraftRef::new();
    m.insert("name".to_string(), Value::String(n.to_string()));
    m
}

async fn put_tenant_row(
    c: &mut PgConnection,
    tenant: Uuid,
    name: String,
    v: Snap,
) -> ApiResult<()> {
    match v {
        None => {
            sqlx::query("DELETE FROM scratch_tenant_item WHERE tenant_id = $1 AND name = $2")
                .bind(tenant)
                .bind(name)
                .execute(&mut *c)
                .await?;
        }
        Some(val) => {
            sqlx::query(
                "INSERT INTO scratch_tenant_item (tenant_id, name, val) VALUES ($1, $2, $3) \
                 ON CONFLICT (tenant_id, name) DO UPDATE SET val = EXCLUDED.val",
            )
            .bind(tenant)
            .bind(name)
            .bind(val)
            .execute(&mut *c)
            .await?;
        }
    }
    Ok(())
}

/// Tenant-wide (D69), key `scratch-tenant:<name>`.
struct TenantRows;

impl DesignTarget for TenantRows {
    fn kind(&self) -> &'static str {
        "scratch_tenant"
    }
    fn scope(&self) -> Scope {
        Scope::Tenant
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("scratch-tenant:{}", name(r))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let v: Option<Value> = sqlx::query_scalar(
                "SELECT val FROM scratch_tenant_item WHERE tenant_id = $1 AND name = $2",
            )
            .bind(at.tenant)
            .bind(name(r))
            .fetch_optional(&mut *c)
            .await?;
            Ok(v)
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { put_tenant_row(c, at.tenant, name(r), v.clone()).await })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let names: Vec<String> = sqlx::query_scalar(
                "SELECT name FROM scratch_tenant_item WHERE tenant_id = $1 ORDER BY name",
            )
            .bind(at.tenant)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(names.into_iter().map(|n| name_ref(&n)).collect()))
        })
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        v.as_ref().map(Value::to_string)
    }
}

async fn tenant_value(e: &TestEnv, f: &ContractJob, n: &str) -> Option<Value> {
    sqlx::query_scalar("SELECT val FROM scratch_tenant_item WHERE tenant_id = $1 AND name = $2")
        .bind(f.tenant)
        .bind(n)
        .fetch_optional(&e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn rollback_never_rewrites_tenant_kind_rows() {
    let (mut e, f, tok) = setup().await;
    sqlx::raw_sql(SCRATCH_DDL).execute(&e.owner).await.unwrap();
    let mut reg = design::registry(e.state.compiler.clone());
    reg.register_scratch(Arc::new(TenantRows));
    let reg = Arc::new(reg);
    e.state.design = reg.clone();
    let app = agent_api::router(e.state.clone());
    sqlx::query("INSERT INTO scratch_tenant_item (tenant_id, name, val) VALUES ($1, 'a', '1')")
        .bind(f.tenant)
        .execute(&e.owner)
        .await
        .unwrap();

    let at = put_target(&app, &f, &tok).await;
    let r1 = approve(&app, &f, &tok, &at, true).await["release"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    // a tracked tenant-wide write (D69), published by R2
    let t = f.job.t.clone();
    let mut tx = t.begin(&e.state.db).await.unwrap();
    let tenant = f.tenant;
    let w = DraftWrite {
        area: "scratch",
        page: "J7",
        kind: "scratch_tenant",
        r#ref: name_ref("a"),
        label: Label::Now("Tenant a".to_string()),
    };
    design::track_tenant::<(), _>(&mut tx, &reg, &t, w, move |c| {
        Box::pin(put_tenant_row(c, tenant, "a".to_string(), Some(json!(2))))
    })
    .await
    .expect("tracked tenant write");
    tx.commit().await.unwrap();
    let b = approve(&app, &f, &tok, &at, true).await;
    assert_eq!(b["created"], true, "{b}");
    assert_eq!(
        count(
            &e,
            "SELECT count(*) FROM draft_ledger WHERE job_id = $1",
            &f
        )
        .await,
        0
    );

    let b = apply(&app, &f, &tok, &r1).await;
    assert_eq!(b["direction"], "rollback");
    assert_eq!(b["rolled_back"], 1, "{b}");
    assert_eq!(
        tenant_value(&e, &f, "a").await,
        Some(json!(2)),
        "one job's rollback never rewrites a tenant-wide row"
    );
    // live is the older snapshot
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let live = design::value(
        &mut tx,
        &reg,
        &f.job_ref(),
        &DesignOf::Live,
        "scratch_tenant",
        &name_ref("a"),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(live, Some(json!(1)));
}

// ---- conflicts on apply -------------------------------------------------------------------------------------------

/// A base context space key of the draft bundle that can be hidden (not `required: true`).
async fn base_space(app: &Router, f: &ContractJob, tok: &str) -> String {
    let (s, b) = send(app, get(&uri(f, "context_space"), Some(tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["base"]
        .as_array()
        .expect("base")
        .iter()
        .find(|i| i["required"] != json!(true))
        .and_then(|i| i["key"].as_str())
        .expect("a base context space")
        .to_string()
}

async fn insert_conflict(
    e: &TestEnv,
    f: &ContractJob,
    release: &str,
    key: &str,
    decision: Option<Value>,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO release_conflict (tenant_id, job_id, release_id, kind, object, key, tenant_row, detail, \
                                       decision) \
         VALUES ($1, $2, $3, 'base_changed', 'context_space', $4, 'override', 'seeded', $5) RETURNING id",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(release)
    .bind(key)
    .bind(decision)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

#[derive(Debug, sqlx::FromRow)]
struct Conflict {
    id: i64,
    kind: String,
    key: String,
    release_id: String,
    status: String,
    resolution: Option<String>,
    resolved_by: Option<String>,
    raised_by_cut: Option<String>,
    cause: Option<Value>,
    decision: Option<Value>,
}

async fn conflicts(e: &TestEnv, f: &ContractJob) -> Vec<Conflict> {
    sqlx::query_as::<_, Conflict>(
        "SELECT id, kind, key, release_id, status, resolution, resolved_by, raised_by_cut, cause, decision \
           FROM release_conflict WHERE job_id = $1 ORDER BY id",
    )
    .bind(f.job.job_id)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn conflicts_superseded_and_recomputed() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e.app, &f, &tok).await;
    let space = base_space(&e.app, &f, &tok).await;
    // the tenant's extension whose key the base has: base_added once a release with that base is live
    sqlx::query(
        "INSERT INTO job_overlay_row (tenant_id, job_id, object, key, kind, item, base_hash, content_hash, \
                                      updated_by) \
         VALUES ($1, $2, 'context_space', $3, 'extension', $4, NULL, 'sha256:x', 'test')",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(&space)
    .bind(json!({"key": space, "label": "Mine"}))
    .execute(&e.owner)
    .await
    .unwrap();
    let id = cut(&e.app, &f, &tok, &at).await;
    let ghost = insert_conflict(&e, &f, &id, "ghost", None).await;

    let b = apply(&e.app, &f, &tok, &id).await;
    assert_eq!(b["conflicts_open"], 1, "{b}");
    let all = conflicts(&e, &f).await;
    let old = all.iter().find(|c| c.id == ghost).unwrap();
    assert_eq!(old.status, "superseded", "{all:?}");
    let open: Vec<&Conflict> = all.iter().filter(|c| c.status == "open").collect();
    assert_eq!(open.len(), 1, "{all:?}");
    assert_eq!(open[0].kind, "base_added");
    assert_eq!(open[0].key, space);
    assert_eq!(open[0].release_id, id);
    assert_eq!(open[0].raised_by_cut, None);
    assert_eq!(open[0].cause, Some(json!({"kind": "new_cut"})));

    // the person decides it (a draft key r2 will not carry) and it has a cause other than r2's own (`new_cut`)
    let first = open[0].id;
    let cause = json!({"kind": "template_upgrade", "layer": "job/x", "from": "v1", "to": "v2"});
    let decision = json!({
        "choice": "rebased",
        "draft_key": "overlay:context_space:not-bundled",
        "draft_id": "d-1",
        "by": ADMIN,
        "at": "2026-10-20T10:00:00Z",
    });
    sqlx::query("UPDATE release_conflict SET cause = $2, decision = $3 WHERE id = $1")
        .bind(first)
        .bind(&cause)
        .bind(&decision)
        .execute(&e.owner)
        .await
        .unwrap();

    // applying again (rollback to nothing is impossible; re-cut with a change) recomputes: the old open one is
    // superseded and the same conflict is opened again under the new release, keeping its cause and decision
    put_languages(&e.app, &f, &tok, json!(["en", "de"])).await;
    let r2 = cut(&e.app, &f, &tok, &at).await;
    apply(&e.app, &f, &tok, &r2).await;
    let all = conflicts(&e, &f).await;
    let open: Vec<&Conflict> = all.iter().filter(|c| c.status == "open").collect();
    assert_eq!(open.len(), 1, "{all:?}");
    assert_eq!(open[0].release_id, r2);
    assert_eq!(open[0].kind, "base_added");
    assert_ne!(open[0].id, first, "a new row, not the old one reopened");
    assert_eq!(
        open[0].cause.as_ref(),
        Some(&cause),
        "the carried cause, not new_cut"
    );
    assert_eq!(
        open[0].decision.as_ref(),
        Some(&decision),
        "the carried decision"
    );
    assert_eq!(open[0].raised_by_cut, None);
    assert_eq!(open[0].resolution, None);
    assert_eq!(
        all.iter().filter(|c| c.status == "superseded").count(),
        2,
        "{all:?}"
    );
}

#[tokio::test]
async fn decided_conflict_resolved_by_carrying_cut() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e.app, &f, &tok).await;
    let space = base_space(&e.app, &f, &tok).await;
    let (s, b) = send(
        &e.app,
        req(
            "POST",
            &uri(&f, "refinements/hide"),
            Some(&tok),
            json!({"object": "context_space", "key": space}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let draft_key = format!("overlay:context_space:{space}");
    let ledger_id: i64 =
        sqlx::query_scalar("SELECT id FROM draft_ledger WHERE job_id = $1 AND key = $2")
            .bind(f.job.job_id)
            .bind(&draft_key)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    let id = cut(&e.app, &f, &tok, &at).await;
    let decision = json!({
        "choice": "reverted",
        "draft_key": draft_key,
        "draft_id": format!("d-{ledger_id}"),
        "by": ADMIN,
        "at": "2026-10-20T10:00:00Z",
    });
    let decided = insert_conflict(&e, &f, &id, &space, Some(decision)).await;
    let undecided = insert_conflict(&e, &f, &id, "other", None).await;

    let b = apply(&e.app, &f, &tok, &id).await;
    assert_eq!(b["conflicts_open"], 0, "{b}");
    let all = conflicts(&e, &f).await;
    let c = all.iter().find(|c| c.id == decided).unwrap();
    assert_eq!(c.status, "resolved", "{all:?}");
    assert_eq!(c.resolution.as_deref(), Some("reverted"));
    assert_eq!(c.resolved_by.as_deref(), Some(ADMIN));
    let d = c.decision.as_ref().unwrap();
    assert_eq!(d["published_in"], id.as_str());
    assert_eq!(d["draft_id"], Value::Null);
    let u = all.iter().find(|c| c.id == undecided).unwrap();
    assert_eq!(u.status, "superseded");
}

/// §5 approve step 11 (D15): a cut that adopts newer pins opens its conflicts at the cut — `raised_by_cut` = the
/// cut, cause `template_upgrade` — skips one already open, and does nothing when the pins did not move.
#[tokio::test]
async fn raise_upgrade_opens_conflicts_at_the_cut() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e.app, &f, &tok).await;
    let space = base_space(&e.app, &f, &tok).await;
    sqlx::query(
        "INSERT INTO job_overlay_row (tenant_id, job_id, object, key, kind, item, base_hash, content_hash, \
                                      updated_by) \
         VALUES ($1, $2, 'context_space', $3, 'extension', $4, NULL, 'sha256:x', 'test')",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(&space)
    .bind(json!({"key": space, "label": "Mine"}))
    .execute(&e.owner)
    .await
    .unwrap();
    let id = cut(&e.app, &f, &tok, &at).await;
    let bundle: Value =
        sqlx::query_scalar("SELECT bundle FROM job_release WHERE job_id = $1 AND release_id = $2")
            .bind(f.job.job_id)
            .bind(&id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    let pin = |v: i32| TemplatePin {
        layer: "job/customer_care_agent".to_string(),
        version: v,
    };
    let (old, new) = ([pin(4)], [pin(5)]);
    let jr = f.job_ref();

    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let det = &e.state.conflict_detectors;
    let same =
        conflict_engine::raise_upgrade(&mut tx, det, f.tenant, &jr, &id, &old, &old, None, &bundle)
            .await
            .unwrap();
    assert_eq!(same, 0, "pins did not move: nothing is raised");
    let n =
        conflict_engine::raise_upgrade(&mut tx, det, f.tenant, &jr, &id, &old, &new, None, &bundle)
            .await
            .unwrap();
    assert_eq!(n, 1);
    let again =
        conflict_engine::raise_upgrade(&mut tx, det, f.tenant, &jr, &id, &old, &new, None, &bundle)
            .await
            .unwrap();
    assert_eq!(
        again, 0,
        "an open conflict with the same kind, object and key is not raised twice"
    );
    tx.commit().await.unwrap();

    let all = conflicts(&e, &f).await;
    assert_eq!(all.len(), 1, "{all:?}");
    let c = &all[0];
    assert_eq!(c.status, "open");
    assert_eq!(c.kind, "base_added");
    assert_eq!(c.key, space);
    assert_eq!(c.release_id, id);
    assert_eq!(c.raised_by_cut.as_deref(), Some(id.as_str()));
    assert_eq!(
        c.cause,
        Some(
            json!({"kind": "template_upgrade", "layer": "job/customer_care_agent", "from": "v4", "to": "v5"})
        )
    );
    assert_eq!(c.decision, None);
}

// ---- upgrade requests and rollbacks_seen --------------------------------------------------------------------------

async fn upgrade_request(
    e: &TestEnv,
    f: &ContractJob,
    release: &str,
    version: Option<i32>,
) -> Uuid {
    // to_pins = the release's first pin (contained in its pins), or that pin at `version` (not contained)
    sqlx::query_scalar(
        "INSERT INTO template_upgrade_request (tenant_id, job_id, from_pins, to_pins, requested_by) \
         SELECT $1, $2, '[]'::jsonb, \
                jsonb_build_array(CASE WHEN $4::int IS NULL THEN r.pins -> 0 \
                                       ELSE jsonb_set(r.pins -> 0, '{version}', to_jsonb($4::int)) END), \
                'test' \
           FROM job_release r WHERE r.job_id = $2 AND r.release_id = $3 AND jsonb_array_length(r.pins) > 0 \
         RETURNING id",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(release)
    .bind(version)
    .fetch_one(&e.owner)
    .await
    .expect("the release has at least one pin")
}

async fn request_status(e: &TestEnv, id: Uuid) -> String {
    sqlx::query_scalar("SELECT status FROM template_upgrade_request WHERE id = $1")
        .bind(id)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

/// §5 apply step 6: an open upgrade request the release's pins satisfy (`pins @> to_pins`) is `done`; one they do
/// not satisfy stays open.
#[tokio::test]
async fn apply_closes_only_satisfied_upgrade_requests() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e.app, &f, &tok).await;
    let r1 = cut(&e.app, &f, &tok, &at).await;
    let met = upgrade_request(&e, &f, &r1, None).await;
    apply(&e.app, &f, &tok, &r1).await;
    assert_eq!(request_status(&e, met).await, "done");

    let unmet = upgrade_request(&e, &f, &r1, Some(999)).await;
    put_languages(&e.app, &f, &tok, json!(["en", "de"])).await;
    let r2 = cut(&e.app, &f, &tok, &at).await;
    apply(&e.app, &f, &tok, &r2).await;
    assert_eq!(request_status(&e, unmet).await, "open");
}

/// `rollbacks_seen` (fixture `publishedEffect`): applying a cut re-applies only the changes rolled back AFTER the cut
/// was taken. R3 is cut while R2 is live, then Production rolls back to R1 (R2's change out) and R3 is applied: R2's
/// change is re-applied.
#[tokio::test]
async fn cut_reapplies_changes_rolled_back_after_it() {
    let (e, f, tok) = setup().await;
    let (r1, _r2, at) = two_releases(&e.app, &f, &tok).await;
    put_languages(&e.app, &f, &tok, json!(["en", "de", "fr", "nl"])).await;
    let r3 = cut(&e.app, &f, &tok, &at).await;
    let b = apply(&e.app, &f, &tok, &r1).await;
    assert_eq!(b["rolled_back"], 1, "{b}");

    let b = apply(&e.app, &f, &tok, &r3).await;
    assert_eq!(b["direction"], "apply", "{b}");
    assert_eq!(b["reapplied"], 1, "the rollback came after the cut: {b}");
    assert!(
        published(&e, &f)
            .await
            .iter()
            .all(|p| p.rolled_back.is_none()),
        "R2's change is live again"
    );
}

/// The other side: a rollback logged BEFORE the cut is already in the cut's design; applying it re-applies nothing.
#[tokio::test]
async fn cut_does_not_reapply_changes_rolled_back_before_it() {
    let (e, f, tok) = setup().await;
    let (r1, _r2, at) = two_releases(&e.app, &f, &tok).await;
    let b = apply(&e.app, &f, &tok, &r1).await;
    assert_eq!(b["rolled_back"], 1, "{b}");
    put_languages(&e.app, &f, &tok, json!(["en", "nl"])).await;
    let r3 = cut(&e.app, &f, &tok, &at).await;

    let b = apply(&e.app, &f, &tok, &r3).await;
    assert_eq!(b["direction"], "apply", "{b}");
    assert_eq!(b["reapplied"], 0, "the rollback came before the cut: {b}");
    let undone = published(&e, &f).await;
    assert_eq!(
        undone.iter().filter(|p| p.rolled_back.is_some()).count(),
        1,
        "R2's change stays rolled back: {undone:?}"
    );
}

// ---- readiness fake -----------------------------------------------------------------------------------------------

/// A test-only contributor that always blocks on one W5 slot, recording which design it was asked to check.
struct Blocks {
    seen: Mutex<Vec<DesignOf>>,
}

impl ReadinessContributor for Blocks {
    fn area(&self) -> AreaKey {
        AreaKey::Providers
    }

    fn assess<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        cx: &'a ReadinessCx<'a>,
    ) -> BoxFuture<'a, ApiResult<Assessment>> {
        self.seen.lock().unwrap().push(cx.design.clone());
        let mut area = board::area_shell(AreaKey::Providers, &cx.job.key);
        area.status = AreaStatus::Gap;
        let a = Assessment {
            area,
            blocking: vec![MissingItem {
                dependency: "strata_rag".to_string(),
                slot: "embed".to_string(),
                detail: "W5 slot strata_rag.embed has no deployment".to_string(),
                href: "/admin/providers".to_string(),
            }],
            testing: Vec::new(),
            go_live: Vec::new(),
            steps: Vec::new(),
        };
        Box::pin(async move { Ok(a) })
    }
}

// ---- names (live acceptance 2026-10-21: names showed raw subs) ----------------------------------------------------

/// A tenant admin with the given `display_name` / `email` (either may be absent).
async fn insert_person(
    e: &TestEnv,
    f: &ContractJob,
    sub: &str,
    display_name: Option<&str>,
    email: Option<&str>,
) {
    sqlx::query(
        "INSERT INTO users (sub, tenant_id, roles, display_name, email, updated_by) \
         VALUES ($1, $2, $3, $4, $5, 'test')",
    )
    .bind(sub)
    .bind(f.tenant)
    .bind(common::role_keys("tenant_admin"))
    .bind(display_name)
    .bind(email)
    .execute(&e.owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn names_resolve_display_name_then_email_then_sub() {
    let (e, f, _) = setup().await;
    // Keycloak-style subs: the bug showed these instead of the person
    let named = "0b8f6a1e-5d1c-4c1a-9f0e-000000000001";
    let mailed = "0b8f6a1e-5d1c-4c1a-9f0e-000000000002";
    insert_person(&e, &f, named, Some("Ann Lee"), Some("ann@shop.example")).await;
    insert_person(&e, &f, mailed, None, Some("bo@shop.example")).await;
    let (ann, bo) = (person(named, &[]), person(mailed, &[]));

    // target view: the saver (email only) and the history actor
    let body = json!({"selection": sel(), "expected_updated_at": null});
    let (s, t) = send(&e.app, req("PUT", &uri(&f, "target"), Some(&bo), body)).await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert_eq!(t["selection_updated_by_name"], "bo@shop.example", "{t}");
    assert_eq!(t["history"][0]["actor_name"], "bo@shop.example", "{t}");
    let at = t["selection_updated_at"].clone();

    // release row: approved by Bo (email only), applied by Ann (display name)
    put_languages(&e.app, &f, &bo, json!(["en", "de"])).await;
    let id = cut(&e.app, &f, &bo, &at).await;
    apply(&e.app, &f, &ann, &id).await;
    let (s, list) = send(&e.app, get(&uri(&f, "releases"), Some(&ann))).await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let row = &list["items"][0];
    assert_eq!(row["id"], id.as_str(), "{list}");
    assert_eq!(row["names"][mailed], "bo@shop.example", "{row}");
    assert_eq!(row["names"][named], "Ann Lee", "{row}");

    // the shared helper: display name, else email, else the sub (a non-member is named by itself)
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let got = agent_api::members::names::lookup(
        &mut tx,
        &[
            named.to_string(),
            mailed.to_string(),
            "u-nobody".to_string(),
        ],
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(got[named], "Ann Lee");
    assert_eq!(got[mailed], "bo@shop.example");
    assert_eq!(got["u-nobody"], "u-nobody");
}

// ---- setting lines of the design views (live acceptance 2026-10-21: every setting read "release value") -----------

/// The `text` of place `key` in a `DesignState`.
fn line(state: &Value, key: &str) -> Value {
    state["values"]
        .as_array()
        .and_then(|vs| vs.iter().find(|v| v["key"] == key))
        .map_or(Value::Null, |v| v["text"].clone())
}

#[tokio::test]
async fn design_views_show_setting_values() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e.app, &f, &tok).await;
    let (s, view) = send(&e.app, get(&uri(&f, "settings"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{view}");
    let stance = view["base"]["stance"].clone();
    assert!(
        stance.is_string() && view["base"]["actor"].is_string(),
        "the seed bundle has setting bases: {view}"
    );
    // KH T4 (D108): the preamble is the identity instruction, no setting place
    assert!(view["base"].get("preamble").is_none(), "{view}");

    // an override and a hide, released
    let patch = json!({"actor": "Lumen care", "stance": null});
    let (s, b) = send(&e.app, req("PUT", &uri(&f, "settings"), Some(&tok), patch)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let b = approve(&e.app, &f, &tok, &at, true).await;
    let id = b["release"]["id"].as_str().expect("release id").to_string();

    for path in [
        "design/live".to_string(),
        "design/draft".to_string(),
        format!("releases/{id}/design"),
    ] {
        let (s, d) = send(&e.app, get(&uri(&f, &path), Some(&tok))).await;
        assert_eq!(s, StatusCode::OK, "{path}: {d}");
        assert_eq!(line(&d, "setting:actor"), "Lumen care", "{path}: {d}");
        assert_eq!(line(&d, "setting:stance"), "hidden", "{path}: {d}");
        assert!(
            !d.to_string().contains("release value"),
            "{path}: no placeholder text: {d}"
        );
    }

    // the diff: reverting the hide shows the base on the draft side, the hide on the live side
    let path = uri(&f, "settings/stance");
    let (s, b) = send(&e.app, req("DELETE", &path, Some(&tok), Value::Null)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, d) = send(&e.app, get(&uri(&f, "design/diff"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    let row = d["rows"]
        .as_array()
        .and_then(|rs| rs.iter().find(|r| r["key"] == "setting:stance"))
        .expect("the reverted hide differs from live");
    assert_eq!(row["live"], "hidden", "{d}");
    assert_eq!(row["draft"], stance, "{d}");
}

// ---- value fields of draft rows, release detail and previews (live acceptance round 2, 2026-10-21) ---------------
// `GET draft` answered `"live": "release value"` for a setting whose live value was the base, and for a hidden context
// space (the overlay target's "no tenant row" text); every `live` / `draft` / `now` / `after` is a VALUE.

/// The row of `key` in a list of change rows.
fn row_of<'v>(rows: &'v Value, key: &str) -> &'v Value {
    rows.as_array()
        .and_then(|rs| rs.iter().find(|r| r["key"] == key))
        .unwrap_or_else(|| panic!("no row {key}: {rows}"))
}

#[tokio::test]
async fn draft_rows_and_previews_show_values_not_placeholders() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e.app, &f, &tok).await;
    let (s, view) = send(&e.app, get(&uri(&f, "settings"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{view}");
    let stance = view["base"]["stance"].clone();
    assert!(
        stance.is_string(),
        "the seed bundle has a stance base: {view}"
    );
    let space = base_space(&e.app, &f, &tok).await;
    let (s, spaces) = send(&e.app, get(&uri(&f, "context_space"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{spaces}");
    let label = row_of(&spaces["base"], &space)["label"]
        .as_str()
        .expect("a base item has a label")
        .to_string();
    let r1 = approve(&e.app, &f, &tok, &at, true).await["release"]["id"]
        .as_str()
        .expect("release id")
        .to_string();

    // a setting override over the base: the draft row's live side is the base in force
    let patch = json!({"stance": "Our own stance"});
    let (s, b) = send(&e.app, req("PUT", &uri(&f, "settings"), Some(&tok), patch)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, d) = send(&e.app, get(&uri(&f, "draft"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    let row = row_of(&d["changes"], "setting:stance");
    assert_eq!(row["live"], stance, "{d}");
    assert_eq!(row["draft"], "Our own stance", "{d}");
    assert!(!d.to_string().contains("release value"), "{d}");

    // the release that carries it: its detail rows and the apply preview
    let r2 = cut(&e.app, &f, &tok, &at).await;
    let (s, detail) = send(&e.app, get(&uri(&f, &format!("releases/{r2}")), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{detail}");
    let row = row_of(&detail["drafts"], "setting:stance");
    assert_eq!(row["live"], stance, "{detail}");
    assert_eq!(row["draft"], "Our own stance", "{detail}");
    let (s, p) = preview(&e.app, &f, &tok, &r2).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let row = row_of(&p["draft"], "setting:stance");
    assert_eq!(row["live_now"], stance, "{p}");
    assert_eq!(row["live_after"], "Our own stance", "{p}");
    assert!(!p.to_string().contains("release value"), "{p}");

    // rolling back un-does it: now = the override, after = the base again
    apply(&e.app, &f, &tok, &r2).await;
    let (s, p) = preview(&e.app, &f, &tok, &r1).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let row = row_of(&p["undone"], "setting:stance");
    assert_eq!(row["now"], "Our own stance", "{p}");
    assert_eq!(row["after"], stance, "{p}");
    assert!(!p.to_string().contains("release value"), "{p}");

    // a hidden context space: live is the release's base row, not a setting placeholder
    let body = json!({"object": "context_space", "key": space});
    let (s, b) = send(
        &e.app,
        req("POST", &uri(&f, "refinements/hide"), Some(&tok), body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, d) = send(&e.app, get(&uri(&f, "draft"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{d}");
    let row = row_of(&d["changes"], &format!("overlay:context_space:{space}"));
    assert_eq!(row["live"], format!("base “{label}”"), "{d}");
    assert_eq!(row["draft"], "hidden", "{d}");
    assert!(!d.to_string().contains("release value"), "{d}");
    let (s, diff) = send(&e.app, get(&uri(&f, "design/diff"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{diff}");
    let row = row_of(&diff["rows"], &format!("overlay:context_space:{space}"));
    assert_eq!(row["live"], format!("base “{label}”"), "{diff}");
}
