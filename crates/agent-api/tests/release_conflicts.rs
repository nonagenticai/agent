//! B2 T13: release conflicts, read and resolve (spec B2 §4, 05 §4.6, J16, D54) — `GET /api/jobs/{job}/
//! release-conflicts`, `GET …/release-conflicts/board` and `resolveConflict` (`POST …/refinements/{revert|rebase}`
//! `{conflict}`). Real Postgres 18 (one container for this binary, a fresh database per test), through the RLS
//! runtime role `agent_app` and the real router, on a job created through `POST /api/jobs` from
//! `job/customer_care_agent@v4` (the B2 creation hook ran: target row + baseline).
//!
//! Each test makes the tenant's refinement LIVE first (a context space override, or a setting override, saved, cut
//! and applied as R1), then seeds the conflict row against R1 as the detectors would (owner SQL): the conflict
//! detection itself is T12's (`release_apply.rs`).
use crate::b5::support as b5_support;
use crate::common;

use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};

use common::design_contract::ContractJob;
use common::{TestEnv, get, insert_user, json as req, person, send};

/// Every role in the contract tenant.
const ADMIN: &str = "u-conflict-admin";

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

/// Approve (cut); `apply` = approve and apply in one call. Returns the release id.
async fn approve(app: &Router, f: &ContractJob, tok: &str, at: &Value, apply: bool) -> String {
    let body = json!({"apply": apply, "selection_seen_at": at});
    let (s, b) = send(app, req("POST", &uri(f, "releases"), Some(tok), body)).await;
    assert!(s == StatusCode::CREATED || s == StatusCode::OK, "{s}: {b}");
    assert_eq!(b["applied"], apply, "{b}");
    b["release"]["id"].as_str().expect("release id").to_string()
}

async fn apply(app: &Router, f: &ContractJob, tok: &str, id: &str) {
    let path = uri(f, &format!("releases/{id}/apply"));
    let (s, b) = send(app, req("POST", &path, Some(tok), json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["applied"], true, "{b}");
}

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

/// The tenant's context-space override is live: target saved, override saved, R1 approved and applied. Returns
/// `(selection_seen_at, R1, the space key)`.
async fn live_override(e: &TestEnv, f: &ContractJob, tok: &str) -> (Value, String, String) {
    let at = put_target(&e.app, f, tok).await;
    let space = base_space(&e.app, f, tok).await;
    let path = uri(f, &format!("context-spaces/{space}"));
    let (s, b) = send(
        &e.app,
        req("PUT", &path, Some(tok), json!({"label": "Mine"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let r1 = approve(&e.app, f, tok, &at, true).await;
    assert_eq!(ledger_count(e, f).await, 0, "R1 published the override");
    (at, r1, space)
}

/// Seed one conflict row against `release` (as a detector stores it); returns its id.
async fn insert_conflict(
    e: &TestEnv,
    f: &ContractJob,
    release: &str,
    object: &str,
    kind: &str,
    key: &str,
) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO release_conflict (tenant_id, job_id, release_id, kind, object, key, tenant_row, detail, \
                                       base_hash_before, base_hash_after, base_before, base_after, cause) \
         VALUES ($1, $2, $3, $4, $5, $6, 'override', 'seeded', 'sha256:old', 'sha256:new', $7, $8, $9) \
         RETURNING id",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(release)
    .bind(kind)
    .bind(object)
    .bind(key)
    .bind(json!({"key": key, "label": "Before"}))
    .bind(json!({"key": key, "label": "After"}))
    .bind(json!({"kind": "new_cut"}))
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn ledger_count(e: &TestEnv, f: &ContractJob) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM draft_ledger WHERE job_id = $1")
        .bind(f.job.job_id)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn ledger_id(e: &TestEnv, f: &ContractJob, key: &str) -> i64 {
    sqlx::query_scalar("SELECT id FROM draft_ledger WHERE job_id = $1 AND key = $2")
        .bind(f.job.job_id)
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

#[derive(Debug, sqlx::FromRow)]
struct Conflict {
    status: String,
    resolution: Option<String>,
    resolved_by: Option<String>,
    decision: Option<Value>,
}

async fn conflict(e: &TestEnv, id: i64) -> Conflict {
    sqlx::query_as::<_, Conflict>(
        "SELECT status, resolution, resolved_by, decision FROM release_conflict WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn resolve(
    app: &Router,
    f: &ContractJob,
    tok: &str,
    op: &str,
    id: i64,
) -> (StatusCode, Value) {
    let path = uri(f, &format!("refinements/{op}"));
    send(app, req("POST", &path, Some(tok), json!({"conflict": id}))).await
}

async fn board(app: &Router, f: &ContractJob, tok: &str, status: &str) -> Value {
    let path = uri(f, &format!("release-conflicts/board?status={status}"));
    let (s, b) = send(app, get(&path, Some(tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

async fn list(app: &Router, f: &ContractJob, tok: &str, status: &str) -> Vec<Value> {
    let path = uri(f, &format!("release-conflicts?status={status}"));
    let (s, b) = send(app, get(&path, Some(tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["next"], Value::Null, "{b}");
    b["items"].as_array().expect("items").clone()
}

async fn discard_all(app: &Router, f: &ContractJob, tok: &str) {
    let (s, b) = send(app, req("DELETE", &uri(f, "draft"), Some(tok), json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn card(b: &Value, id: i64) -> Value {
    b["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("card {id} in {b}"))
        .clone()
}

// ---- tests ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn resolve_saves_decision_in_draft_conflict_stays_open() {
    let (e, f, tok) = setup().await;
    let (_at, r1, space) = live_override(&e, &f, &tok).await;
    let id = insert_conflict(&e, &f, &r1, "context_space", "base_changed", &space).await;

    let (s, b) = resolve(&e.app, &f, &tok, "revert", id).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["draft_rev"].as_i64().is_some(), "{b}");

    // D54: a draft change carries the choice; the conflict stays open (decided in draft)
    let draft_key = format!("overlay:context_space:{space}");
    let d_id = ledger_id(&e, &f, &draft_key).await;
    let c = conflict(&e, id).await;
    assert_eq!(c.status, "open", "{c:?}");
    assert_eq!(c.resolution, None);
    let d = c.decision.expect("decision");
    assert_eq!(d["choice"], "reverted");
    assert_eq!(d["draft_key"], draft_key.as_str());
    assert_eq!(d["draft_id"], format!("d-{d_id}"));
    assert_eq!(d["by"], ADMIN);

    // the list shows it open with its decision
    let open = list(&e.app, &f, &tok, "open").await;
    assert_eq!(open.len(), 1, "{open:?}");
    assert_eq!(open[0]["id"], id);
    assert_eq!(open[0]["status"], "open");
    assert_eq!(open[0]["decision"]["choice"], "reverted");
    assert!(open[0]["decision"]["by_name"].is_string(), "{open:?}");
    assert_eq!(open[0]["release_id"], r1.as_str());

    // the card: decided in draft, the override gone from the working rows (mine = null), the stored base sides
    let b = board(&e.app, &f, &tok, "open").await;
    assert_eq!(b["open"], 0, "{b}");
    assert_eq!(b["decided_in_draft"], 1, "{b}");
    let c = card(&b, id);
    assert_eq!(c["draft"]["id"], format!("d-{d_id}"), "{c}");
    assert_eq!(c["draft"]["in_release"], Value::Null, "{c}");
    assert!(c["draft"]["label"].is_string(), "{c}");
    assert_eq!(c["mine"], Value::Null, "{c}");
    assert_eq!(c["base_subject"], space.as_str());
    assert_eq!(c["base_before"]["label"], "Before");
    assert_eq!(c["base_after"]["label"], "After");
    assert_eq!(c["cause"], json!({"kind": "new_cut"}));
    assert_eq!(c["impact"], Value::Null);
    assert_eq!(c["published_in"], Value::Null);
    assert_eq!(c["edit_path"], "/refinements");
}

#[tokio::test]
async fn apply_of_cut_resolves_it() {
    let (e, f, tok) = setup().await;
    let (at, r1, space) = live_override(&e, &f, &tok).await;
    let id = insert_conflict(&e, &f, &r1, "context_space", "base_changed", &space).await;
    let (s, b) = resolve(&e.app, &f, &tok, "revert", id).await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // the cut carries the decision: still open, the card names the waiting release
    let r2 = approve(&e.app, &f, &tok, &at, false).await;
    let b = board(&e.app, &f, &tok, "open").await;
    assert_eq!(card(&b, id)["draft"]["in_release"], r2.as_str(), "{b}");
    assert_eq!(conflict(&e, id).await.status, "open");

    apply(&e.app, &f, &tok, &r2).await;
    let c = conflict(&e, id).await;
    assert_eq!(c.status, "resolved", "{c:?}");
    assert_eq!(c.resolution.as_deref(), Some("reverted"));
    assert_eq!(c.resolved_by.as_deref(), Some(ADMIN));
    let d = c.decision.expect("decision");
    assert_eq!(d["published_in"], r2.as_str());
    assert_eq!(d["draft_id"], Value::Null);

    assert!(list(&e.app, &f, &tok, "open").await.is_empty());
    let all = list(&e.app, &f, &tok, "all").await;
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0]["status"], "resolved");
    assert!(all[0]["resolved_by_name"].is_string(), "{all:?}");
    let b = board(&e.app, &f, &tok, "all").await;
    assert_eq!(b["open"], 0, "{b}");
    assert_eq!(b["decided_in_draft"], 0, "{b}");
    assert_eq!(b["current_release"], r2.as_str(), "{b}");
    let c = card(&b, id);
    assert_eq!(c["published_in"], r2.as_str(), "{c}");
    assert_eq!(c["draft"], Value::Null, "{c}");
    // the open board hides it
    assert_eq!(
        board(&e.app, &f, &tok, "open").await["items"],
        json!([]),
        "resolved is not open"
    );
}

#[tokio::test]
async fn conflict_decided_409() {
    let (e, f, tok) = setup().await;
    let (at, r1, space) = live_override(&e, &f, &tok).await;
    let id = insert_conflict(&e, &f, &r1, "context_space", "base_changed", &space).await;
    let (s, b) = resolve(&e.app, &f, &tok, "revert", id).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let rev = b["draft_rev"].clone();

    // the unpublished ledger row carries it
    let (s, b) = resolve(&e.app, &f, &tok, "rebase", id).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "conflict_decided");
    let (s, b) = send(&e.app, get(&uri(&f, "draft"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["count"], 1, "the refused resolve wrote nothing: {b}");

    // an approved cut carries it even after its ledger row is discarded (fixture liveDecision)
    let r2 = approve(&e.app, &f, &tok, &at, false).await;
    discard_all(&e.app, &f, &tok).await;
    let c = conflict(&e, id).await;
    let d = c.decision.expect("kept: the cut still ships it");
    assert_eq!(d["draft_id"], Value::Null);
    let (s, b) = resolve(&e.app, &f, &tok, "revert", id).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "conflict_decided");
    let b = board(&e.app, &f, &tok, "open").await;
    let c = card(&b, id);
    assert_eq!(c["draft"]["id"], Value::Null, "{c}");
    assert_eq!(c["draft"]["in_release"], r2.as_str(), "{c}");
    assert_eq!(c["draft"]["label"], format!("Reverted {space}"), "{c}");
    assert_eq!(b["decided_in_draft"], 1, "{b}");
    assert_ne!(rev, Value::Null);
}

#[tokio::test]
async fn conflict_not_open_409() {
    let (e, f, tok) = setup().await;
    let (_at, r1, space) = live_override(&e, &f, &tok).await;
    let id = insert_conflict(&e, &f, &r1, "context_space", "base_changed", &space).await;
    sqlx::query("UPDATE release_conflict SET status = 'superseded' WHERE id = $1")
        .bind(id)
        .execute(&e.owner)
        .await
        .unwrap();

    let (s, b) = resolve(&e.app, &f, &tok, "revert", id).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "conflict_not_open");
    assert_eq!(ledger_count(&e, &f).await, 0, "nothing written");

    // the other refusals of the resolve branch
    let (s, b) = resolve(&e.app, &f, &tok, "revert", id + 1000).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "conflict_not_found");
    let open = insert_conflict(&e, &f, &r1, "context_space", "base_changed", &space).await;
    let (s, b) = resolve(&e.app, &f, &tok, "hide", open).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_op");
    let both = json!({"conflict": open, "object": "context_space", "key": space});
    let path = uri(&f, "refinements/revert");
    let (s, b) = send(&e.app, req("POST", &path, Some(&tok), both)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(code(&b), "invalid_body");
    for tail in [
        "release-conflicts?status=closed",
        "release-conflicts/board?status=closed",
    ] {
        let (s, b) = send(&e.app, get(&uri(&f, tail), Some(&tok))).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{tail}: {b}");
        assert_eq!(code(&b), "invalid_status");
    }
    // the superseded one is listed only under `all`
    assert_eq!(list(&e.app, &f, &tok, "open").await.len(), 1);
    assert_eq!(list(&e.app, &f, &tok, "all").await.len(), 2);
    assert_eq!(
        conflict(&e, open).await.decision,
        None,
        "refused: undecided"
    );
}

#[tokio::test]
async fn board_counts_open_vs_decided() {
    let (e, f, tok) = setup().await;
    let (at, _r1, space) = live_override(&e, &f, &tok).await;
    // a live setting override as well: its conflict resolves through the setting draft key
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &uri(&f, "settings"),
            Some(&tok),
            json!({"stance": "Mine"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let r2 = approve(&e.app, &f, &tok, &at, true).await;
    assert_eq!(ledger_count(&e, &f).await, 0, "R2 published the setting");
    let a = insert_conflict(&e, &f, &r2, "context_space", "base_changed", &space).await;
    let s_id = insert_conflict(&e, &f, &r2, "job_setting", "job_setting", "stance").await;

    let b = board(&e.app, &f, &tok, "open").await;
    assert_eq!(b["open"], 2, "{b}");
    assert_eq!(b["decided_in_draft"], 0, "{b}");
    assert_eq!(b["blocks_publish"], false);
    assert_eq!(b["current_release"], r2.as_str());
    assert_eq!(b["draft_count"], 0, "{b}");
    assert_eq!(card(&b, s_id)["mine"], json!({"stance": "Mine"}));

    let (s, r) = resolve(&e.app, &f, &tok, "revert", s_id).await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let d = conflict(&e, s_id).await.decision.expect("decision");
    assert_eq!(d["draft_key"], "setting:stance");
    let d_id = ledger_id(&e, &f, "setting:stance").await;
    assert_eq!(d["draft_id"], format!("d-{d_id}"));

    let b = board(&e.app, &f, &tok, "open").await;
    assert_eq!(b["open"], 1, "{b}");
    assert_eq!(b["decided_in_draft"], 1, "{b}");
    assert_eq!(b["draft_count"], 1, "{b}");
    assert_eq!(card(&b, a)["draft"], Value::Null);
    assert_eq!(card(&b, s_id)["draft"]["id"], format!("d-{d_id}"));

    // discarding the only draft that carried the decision: undecided again (T14 cleanup)
    discard_all(&e.app, &f, &tok).await;
    assert_eq!(conflict(&e, s_id).await.decision, None);
    let b = board(&e.app, &f, &tok, "open").await;
    assert_eq!(b["open"], 2, "{b}");
    assert_eq!(b["decided_in_draft"], 0, "{b}");
    assert_eq!(b["draft_count"], 0, "{b}");
}
