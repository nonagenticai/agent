//! B2 T11: the cut (spec B2 §5 approve, D54/D62) and the release reads — `POST/GET /api/jobs/{job}/releases`,
//! `GET …/releases/status`, `GET …/releases/{id}`, `…/compare`, `…/design` — and the `ReleaseApproved` hooks (§2.7,
//! F-06). Real Postgres 18 (one container for this binary, a fresh database per test), through the RLS runtime role
//! `agent_app`, on a job created through `POST /api/jobs` from `job/customer_care_agent@v4` (the B2 creation hook
//! ran: target row with the template's default selection + baseline).
//!
//! The default selection names no channel, so it compiles with one violation (`bundle_incomplete`);
//! the happy paths first save [`sel`], a complete selection of the seed options.
use crate::common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_api::design::{self, DesignOf, DesignTarget, DraftRef, Place, Scope, Snap, snap_to_json};
use agent_api::error::{ApiError, ApiResult};
use agent_api::readiness::{
    AreaKey, AreaStatus, Assessment, MissingItem, Readiness, ReadinessContributor, ReadinessCx,
    board,
};
use agent_api::release::hooks::{Approved, ReleaseApproved};
use axum::Router;
use axum::http::StatusCode;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};

use common::design_contract::{self, ContractJob};
use common::{TestEnv, get, insert_user, json as req, person, send};

/// Every role in the contract tenant.
const ADMIN: &str = "u-cut-admin";

async fn setup() -> (TestEnv, ContractJob, String) {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    insert_user(&e.owner, ADMIN, f.tenant, "tenant_admin").await;
    let tok = person(ADMIN, &[]);
    (e, f, tok)
}

fn uri(f: &ContractJob, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", f.job.job_key)
}

/// A complete selection of the seed options (one area, `channel`, region eu): compiles with no violation.
fn sel(channel: &[&str]) -> Value {
    json!({
        "profile": "retail_ecommerce",
        "industry": "retail",
        "departments": ["industry_dept/retail/customer_service"],
        "areas": ["order-support"],
        "dimensions": {"channel": channel}, "region": "eu",
    })
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

/// Save `selection` (expected: the stored `updated_at`, `null` the first time); returns the new `updated_at`.
async fn put_target(
    e: &TestEnv,
    f: &ContractJob,
    tok: &str,
    selection: Value,
    expected: Value,
) -> Value {
    let body = json!({"selection": selection, "expected_updated_at": expected});
    let (s, b) = send(&e.app, req("PUT", &uri(f, "target"), Some(tok), body)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let at = b["selection_updated_at"].clone();
    assert!(at.is_string(), "{b}");
    at
}

async fn put_languages(e: &TestEnv, f: &ContractJob, tok: &str, langs: Value) {
    let body = json!({"languages": langs});
    let (s, b) = send(&e.app, req("PUT", &uri(f, "languages"), Some(tok), body)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

async fn approve_on(app: &Router, f: &ContractJob, tok: &str, body: Value) -> (StatusCode, Value) {
    send(app, req("POST", &uri(f, "releases"), Some(tok), body)).await
}

async fn approve(e: &TestEnv, f: &ContractJob, tok: &str, body: Value) -> (StatusCode, Value) {
    approve_on(&e.app, f, tok, body).await
}

/// The happy path: save [`sel`], then cut without apply. Returns the new release id.
async fn cut(e: &TestEnv, f: &ContractJob, tok: &str) -> String {
    let at = put_target(e, f, tok, sel(&["chat"]), Value::Null).await;
    let (s, b) = approve(e, f, tok, json!({"apply": false, "selection_seen_at": at})).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["created"], true, "{b}");
    assert_eq!(b["applied"], false, "{b}");
    assert_eq!(b["release"]["state"], "approved", "{b}");
    b["release"]["id"].as_str().expect("release id").to_string()
}

async fn read(e: &TestEnv, f: &ContractJob, tok: &str, tail: &str) -> Value {
    let (s, b) = send(&e.app, get(&uri(f, tail), Some(tok))).await;
    assert_eq!(s, StatusCode::OK, "{tail}: {b}");
    b
}

async fn count(e: &TestEnv, sql: &'static str, f: &ContractJob) -> i64 {
    sqlx::query_scalar(sql)
        .bind(f.job.job_id)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn selection_updated_at(e: &TestEnv, f: &ContractJob) -> Value {
    let (s, b) = send(&e.app, get(&uri(f, "target"), Some(&person(ADMIN, &[])))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["selection_updated_at"].clone()
}

/// One stored or drafted design place, comparable.
#[derive(Debug, PartialEq, sqlx::FromRow)]
struct Stored {
    key: String,
    kind: String,
    place_ref: Value,
    snap: Option<Value>,
}

#[derive(sqlx::FromRow)]
struct Head {
    state: String,
    hash: String,
    bundle_hash: String,
    current_release_id: Option<String>,
}

async fn stored_snapshot(e: &TestEnv, f: &ContractJob, id: &str) -> Vec<Stored> {
    sqlx::query_as::<_, Stored>(
        "SELECT key, kind, ref AS place_ref, snap FROM release_snapshot \
          WHERE job_id = $1 AND release_id = $2 ORDER BY key",
    )
    .bind(f.job.job_id)
    .bind(id)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

async fn draft_snapshot(e: &TestEnv, f: &ContractJob) -> Vec<Stored> {
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let mut rows = design::take_snapshot(&mut tx, &e.state.design, &f.job_ref(), &DesignOf::Draft)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    rows.sort_by(|a, b| a.key.cmp(&b.key));
    rows.into_iter()
        .map(|r| Stored {
            snap: snap_to_json(&r.snap),
            key: r.key,
            kind: r.kind,
            place_ref: Value::Object(r.r#ref),
        })
        .collect()
}

// ---- the cut ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn cut_stores_immutable_snapshot_of_draft() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e, &f, &tok, sel(&["chat"]), Value::Null).await;
    put_languages(&e, &f, &tok, json!(["en", "de"])).await;
    let draft = draft_snapshot(&e, &f).await;
    assert!(
        draft.iter().any(|r| r.kind == "languages"),
        "the draft has a languages place: {draft:?}"
    );
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id = b["release"]["id"].as_str().unwrap().to_string();
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    assert_eq!(id, format!("cfg-{today}.1"));

    // the snapshot = the draft at every snapshot place of every registered snapshot kind
    assert_eq!(stored_snapshot(&e, &f, &id).await, draft);
    let kinds: Vec<String> = sqlx::query_scalar(
        "SELECT unnest(snapshot_kinds) FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(&id)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    let mut want: Vec<String> = e
        .state
        .design
        .snapshot_kinds()
        .iter()
        .map(|k| k.to_string())
        .collect();
    want.sort();
    let mut got = kinds.clone();
    got.sort();
    assert_eq!(got, want);
    assert_eq!(b["release"]["snapshot_places"], json!(draft.len()), "{b}");

    // the release row: approved, nothing applied, hashes of the right shape, the cut is the job's newest
    let head = sqlx::query_as::<_, Head>(
        "SELECT r.state, r.hash, r.bundle_hash, j.current_release_id FROM job_release r \
           JOIN jobs j ON j.id = r.job_id WHERE r.job_id = $1 AND r.release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(&id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let hash = head.hash.clone();
    assert_eq!(head.state, "approved");
    assert!(hash.starts_with("sha256:") && head.bundle_hash.starts_with("sha256:"));
    assert_ne!(
        hash, head.bundle_hash,
        "the release hash covers the design too"
    );
    assert_eq!(head.current_release_id, None, "a cut is not live");

    // immutable: neither the snapshot rows nor the release content can change (the owner included)
    let upd = sqlx::query(
        "UPDATE release_snapshot SET snap = NULL WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(&id)
    .execute(&e.owner)
    .await;
    assert!(upd.is_err(), "release_snapshot is immutable");
    let upd =
        sqlx::query("UPDATE job_release SET bundle = '{}' WHERE job_id = $1 AND release_id = $2")
            .bind(f.job.job_id)
            .bind(&id)
            .execute(&e.owner)
            .await;
    assert!(upd.is_err(), "job_release content is immutable");

    // audited once, with the cut's identity
    let details: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'release.approve' AND job = $1",
    )
    .bind(f.job.job_key.as_str())
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(details.len(), 1, "{details:?}");
    assert_eq!(details[0]["release_id"], id.as_str());
    assert_eq!(details[0]["hash"], hash.as_str());
    assert_eq!(details[0]["selection"], sel(&["chat"]));
    assert_eq!(details[0]["adopt_pins"], json!([]));
    assert_eq!(details[0]["draft_count"], 2);

    // the release's design (releaseDesign) is the stored snapshot
    let d = read(&e, &f, &tok, &format!("releases/{id}/design")).await;
    assert_eq!(d["of"], "release");
    assert_eq!(d["release_id"], id.as_str());
    assert!(d["draft_rev"].is_null());
    let mut keys: Vec<String> = d["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["key"].as_str().unwrap().to_string())
        .collect();
    keys.sort();
    let want_keys: Vec<String> = draft.iter().map(|r| r.key.clone()).collect();
    assert_eq!(keys, want_keys);
    let (s, b) = send(
        &e.app,
        get(&uri(&f, "releases/cfg-2000-01-01.1/design"), Some(&tok)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "release_not_found");
}

#[tokio::test]
async fn cut_bundles_ledger_rows() {
    let (e, f, tok) = setup().await;
    put_languages(&e, &f, &tok, json!(["en", "de"])).await;
    let id = cut(&e, &f, &tok).await;
    let ledger: Vec<(i64, String)> =
        sqlx::query_as("SELECT id, key FROM draft_ledger WHERE job_id = $1 ORDER BY key")
            .bind(f.job.job_id)
            .fetch_all(&e.owner)
            .await
            .unwrap();
    let bundled: Vec<(i64, String)> = sqlx::query_as(
        "SELECT ledger_id, key FROM release_bundled_draft WHERE job_id = $1 AND release_id = $2 ORDER BY key",
    )
    .bind(f.job.job_id)
    .bind(&id)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(ledger.len(), 2, "languages + selection: {ledger:?}");
    assert_eq!(bundled, ledger, "the cut copies every ledger row");
    // the ledger stays: the changes publish on apply (T12), not on approve
    assert_eq!(
        count(
            &e,
            "SELECT count(*) FROM draft_ledger WHERE job_id = $1",
            &f
        )
        .await,
        2
    );

    // releaseList / releaseDetail / releaseCompare / releaseStatus over the cut
    let list = read(&e, &f, &tok, "releases").await;
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{list}");
    assert!(list["next"].is_null());
    let row = &items[0];
    assert_eq!(row["id"], id.as_str());
    assert_eq!(row["job"], f.job.job_key.as_str());
    assert_eq!(row["draft_count"], 2);
    assert_eq!(row["approved_by"], ADMIN);
    assert_eq!(
        row["names"][ADMIN], ADMIN,
        "insert_user's display name is the sub"
    );
    assert!(row["released_at"].is_null() && row["applied_by"].is_null());
    assert!(row["rolled_back"].is_null());
    assert!(row["counts"].is_object(), "{row}");
    assert!(row.get("upgrades").is_none(), "no release before it: {row}");

    let d = read(&e, &f, &tok, &format!("releases/{id}")).await;
    let drafts = d["drafts"].as_array().unwrap();
    assert_eq!(drafts.len(), 2, "{d}");
    for x in drafts {
        assert_eq!(x["in_release"], id.as_str(), "{x}");
        assert!(x["id"].as_str().unwrap().starts_with("d-"), "{x}");
        assert_eq!(x["by"], ADMIN);
    }
    let langs = drafts.iter().find(|x| x["kind"] == "languages").unwrap();
    assert_ne!(langs["live"], langs["draft"], "{langs}");
    assert_eq!(d["selection"]["areas"], json!(["order-support"]), "{d}");
    assert!(d["selection"]["profile_values"].is_object(), "{d}");
    let pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1")
        .bind(f.job.job_id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(d["provenance"]["pins"], pins);
    assert!(d["bundle"].is_array() && d["requires"].is_object(), "{d}");
    assert_eq!(d["rollbacks"], json!([]));
    assert!(d["readiness"].is_null());

    let c = read(&e, &f, &tok, &format!("releases/{id}/compare?with=current")).await;
    assert_eq!(c["id"], id.as_str());
    assert!(c["current"].is_null());
    assert_eq!(c["same_hash"], false);
    for s in c["sections"].as_array().unwrap() {
        assert_eq!(s["removed"], json!([]), "nothing is current: {s}");
    }

    let st = read(&e, &f, &tok, "releases/status").await;
    assert_eq!(st["approved_waiting"], id.as_str(), "{st}");
    assert_eq!(st["base_changed"], false, "nothing is current");
    assert!(st["selection_error"].is_null(), "{st}");
    assert_eq!(st["missing_slots"], json!([]), "{st}");
    assert!(st["draft_hash"].as_str().unwrap().starts_with("sha256:"));
    assert!(st["newer_pins"].is_array(), "{st}");
    assert_eq!(st["draft_count"], 2);

    let (s, b) = send(
        &e.app,
        get(&uri(&f, "releases/cfg-2000-01-01.1"), Some(&tok)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "release_not_found");
}

#[tokio::test]
async fn nothing_to_publish_409() {
    let (e, f, tok) = setup().await;
    let id = cut(&e, &f, &tok).await;
    // make the cut live without T12's apply: current, released, and its ledger rows published
    sqlx::query(
        "UPDATE job_release SET state = 'current', released_by = $3, released_at = now() \
          WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(&id)
    .bind(ADMIN)
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(f.job.job_id)
        .bind(&id)
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query("DELETE FROM draft_ledger WHERE job_id = $1")
        .bind(f.job.job_id)
        .execute(&e.owner)
        .await
        .unwrap();
    let at = selection_updated_at(&e, &f).await;
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "nothing_to_publish");
    assert_eq!(
        b["error"]["message"],
        format!("Nothing to publish: no draft changes and no template change since {id}.")
    );
    assert_eq!(
        count(&e, "SELECT count(*) FROM job_release WHERE job_id = $1", &f).await,
        1
    );

    // a new draft change is something to publish
    put_languages(&e, &f, &tok, json!(["en", "de"])).await;
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let st = read(&e, &f, &tok, "releases/status").await;
    assert_eq!(st["base_changed"], false, "same selection, same pins: {st}");
    let c = read(
        &e,
        &f,
        &tok,
        &format!("releases/{}/compare", b["release"]["id"].as_str().unwrap()),
    )
    .await;
    assert_eq!(c["current"], id.as_str());
    assert_eq!(c["same_hash"], false, "the design differs");
}

#[tokio::test]
async fn same_hash_as_newest_reuses_row() {
    let (e, f, tok) = setup().await;
    let id = cut(&e, &f, &tok).await;
    let at = selection_updated_at(&e, &f).await;
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["created"], false);
    assert_eq!(b["applied"], false);
    assert_eq!(b["release"]["id"], id.as_str());
    assert_eq!(
        count(&e, "SELECT count(*) FROM job_release WHERE job_id = $1", &f).await,
        1
    );
    let audits: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'release.approve' AND job = $1",
    )
    .bind(f.job.job_key.as_str())
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(audits, 1, "a reused cut writes no audit row");
}

#[tokio::test]
async fn mint_skips_existing_ids() {
    let (e, f, tok) = setup().await;
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    for (n, c) in [(1, 'a'), (2, 'b')] {
        let sha = format!("sha256:{}", c.to_string().repeat(64));
        sqlx::query(
            r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
                   provenance, requires, causes, state, approved_by, approved_at, snapshot_kinds)
               VALUES ($1, $2, $3, $4, $4, '{}', '{}', '[]', '{}', '{}', '{}', 'approved', 'u-rm',
                   now() - interval '1 hour', '{}')"#,
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .bind(format!("cfg-{today}.{n}"))
        .bind(sha)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let id = cut(&e, &f, &tok).await;
    assert_eq!(id, format!("cfg-{today}.3"));
    let list = read(&e, &f, &tok, "releases").await;
    let ids: Vec<&str> = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids[0], id.as_str(), "newest first: {ids:?}");
    assert_eq!(ids.len(), 3);
    // a seeded row stores no snapshot: it cannot be applied, and has no design
    let seeded = &list["items"][1];
    assert!(seeded["snapshot_places"].is_null(), "{seeded}");
    let (s, b) = send(
        &e.app,
        get(
            &uri(&f, &format!("releases/cfg-{today}.2/design")),
            Some(&tok),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "bundle_invalid");
}

#[tokio::test]
async fn selection_changed_409() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e, &f, &tok, sel(&["chat"]), Value::Null).await;
    for seen in [
        Value::Null,
        json!("2026-01-01T00:00:00Z"),
        json!("not a time"),
    ] {
        let (s, b) = approve(
            &e,
            &f,
            &tok,
            json!({"apply": false, "selection_seen_at": seen}),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{seen}: {b}");
        assert_eq!(code(&b), "selection_changed");
    }
    assert_eq!(
        count(&e, "SELECT count(*) FROM job_release WHERE job_id = $1", &f).await,
        0
    );
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");

    // a body field approve does not know is refused
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": at, "force": true}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(code(&b), "invalid_body");
}

#[tokio::test]
async fn bundle_incomplete_422_carries_violations() {
    let (e, f, tok) = setup().await;
    // the template's default selection: no channel (release/derive.rs asks for an area only when a compiled
    // context space carries one, and the minimal offering authors no space)
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": null}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "bundle_incomplete");
    assert_eq!(
        b["error"]["message"],
        "The bundle has 1 problem; nothing was approved."
    );
    let paths: Vec<&str> = b["error"]["violations"]
        .as_array()
        .expect("violations")
        .iter()
        .map(|v| v["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["channels"], "{b}");
    assert!(b["error"]["violations"][0]["message"].is_string());
    assert_eq!(
        count(&e, "SELECT count(*) FROM job_release WHERE job_id = $1", &f).await,
        0
    );
}

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

#[tokio::test]
async fn approve_apply_not_ready_409_carries_blocking() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e, &f, &tok, sel(&["chat"]), Value::Null).await;
    let fake = Arc::new(Blocks {
        seen: Mutex::new(Vec::new()),
    });
    let mut r = Readiness::default();
    r.register(fake.clone());
    let mut st = e.state.clone();
    st.readiness = Arc::new(r);
    let app = agent_api::router(st);

    let (s, b) = approve_on(
        &app,
        &f,
        &tok,
        json!({"apply": true, "selection_seen_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "release_not_ready");
    assert_eq!(
        b["error"]["message"],
        "Not approved: the release would not be ready (strata_rag.embed)."
    );
    let blocking = b["error"]["blocking"].as_array().expect("blocking");
    assert_eq!(blocking.len(), 1, "{b}");
    assert_eq!(blocking[0]["code"], "slot_not_deployed");
    assert_eq!(blocking[0]["subject"], "strata_rag.embed");
    assert_eq!(
        *fake.seen.lock().unwrap(),
        [DesignOf::Draft],
        "the gate reads the draft"
    );
    assert_eq!(
        count(&e, "SELECT count(*) FROM job_release WHERE job_id = $1", &f).await,
        0,
        "nothing was approved"
    );

    // without apply the gate does not run: the cut is approved
    let (s, b) = approve_on(
        &app,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(fake.seen.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn adopt_pins_unknown_layer_422() {
    let (e, f, tok) = setup().await;
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": null, "adopt_pins": ["industry/utilities"]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "unknown_layer");
    assert_eq!(
        b["error"]["message"],
        "This job does not pin industry/utilities."
    );
}

#[tokio::test]
async fn nothing_to_adopt_409() {
    let (e, f, tok) = setup().await;
    // the job on the newest customer-care template (whatever the seed's latest tag is)
    let (_, newest) = design_contract::latest_template(&e).await;
    sqlx::query(
        "UPDATE jobs SET pins = (SELECT pins FROM job_template_tag \
                                  WHERE layer = 'job/customer_care_agent' AND version = $2) \
          WHERE id = $1",
    )
    .bind(f.job.job_id)
    .bind(newest)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": null, "adopt_pins": ["job/customer_care_agent"]}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "nothing_to_adopt");
    assert_eq!(
        b["error"]["message"],
        format!("job/customer_care_agent@v{newest} is already the newest tag.")
    );
    assert_eq!(
        count(&e, "SELECT count(*) FROM job_release WHERE job_id = $1", &f).await,
        0
    );
}

#[tokio::test]
async fn later_draft_write_does_not_change_cut_snapshot() {
    let (e, f, tok) = setup().await;
    let id = cut(&e, &f, &tok).await;
    let stored = stored_snapshot(&e, &f, &id).await;
    let design = read(&e, &f, &tok, &format!("releases/{id}/design")).await;
    let hash: String =
        sqlx::query_scalar("SELECT hash FROM job_release WHERE job_id = $1 AND release_id = $2")
            .bind(f.job.job_id)
            .bind(&id)
            .fetch_one(&e.owner)
            .await
            .unwrap();

    put_languages(&e, &f, &tok, json!(["en", "de"])).await;
    let at = selection_updated_at(&e, &f).await;
    put_target(&e, &f, &tok, sel(&["chat", "whatsapp"]), at).await;

    assert_ne!(draft_snapshot(&e, &f).await, stored, "the draft moved on");
    assert_eq!(stored_snapshot(&e, &f, &id).await, stored);
    assert_eq!(
        read(&e, &f, &tok, &format!("releases/{id}/design")).await,
        design
    );
    let after: String =
        sqlx::query_scalar("SELECT hash FROM job_release WHERE job_id = $1 AND release_id = $2")
            .bind(f.job.job_id)
            .bind(&id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(after, hash);
    // the next cut is a different release
    let at = selection_updated_at(&e, &f).await;
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_ne!(b["release"]["id"], id.as_str());
}

// ---- the ReleaseApproved hooks ------------------------------------------------------------------------------------

type Log = Arc<Mutex<Vec<String>>>;

/// Reads the approved release in its own transaction (proving the approve committed first), then fails.
struct Failing(Log);

impl ReleaseApproved for Failing {
    fn name(&self) -> &'static str {
        "failing"
    }

    fn on_release_approved<'a>(
        &'a self,
        db: &'a PgPool,
        a: &'a Approved,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let mut tx = agent_api::tenancy::begin_scoped(db, a.tenant).await?;
            let state: Option<String> = sqlx::query_scalar(
                "SELECT state FROM job_release WHERE job_id = $1 AND release_id = $2",
            )
            .bind(a.job.id)
            .bind(&a.release_id)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            let seen = state.unwrap_or_else(|| "missing".to_string());
            self.0
                .lock()
                .unwrap()
                .push(format!("failing:{}:{seen}:{}", a.release_id, a.by.id));
            Err(ApiError::Conflict(
                "hook_failed",
                "the fake hook always fails".into(),
            ))
        })
    }
}

/// Panics (a hook bug must not reach the approve or the hooks after it).
struct Panicking(bool);

impl ReleaseApproved for Panicking {
    fn name(&self) -> &'static str {
        "panicking"
    }

    fn on_release_approved<'a>(
        &'a self,
        _db: &'a PgPool,
        _a: &'a Approved,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            if self.0 {
                panic!("the fake hook panics");
            }
            Ok::<(), ApiError>(())
        })
    }
}

/// Records that it ran.
struct Recorder(Log);

impl ReleaseApproved for Recorder {
    fn name(&self) -> &'static str {
        "recorder"
    }

    fn on_release_approved<'a>(
        &'a self,
        _db: &'a PgPool,
        a: &'a Approved,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            self.0
                .lock()
                .unwrap()
                .push(format!("recorder:{}", a.release_id));
            Ok(())
        })
    }
}

async fn wait_for(log: &Log, n: usize) -> Vec<String> {
    for _ in 0..200 {
        let got = log.lock().unwrap().clone();
        if got.len() >= n {
            return got;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    log.lock().unwrap().clone()
}

#[tokio::test]
async fn approved_hooks_run_after_commit_and_cannot_block() {
    let (e, f, tok) = setup().await;
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let hooks = &e.state.release_approved;
    hooks.register_approved(Arc::new(Panicking(true)));
    hooks.register_approved(Arc::new(Failing(log.clone())));
    hooks.register_approved(Arc::new(Recorder(log.clone())));

    let id = cut(&e, &f, &tok).await;
    let got = wait_for(&log, 2).await;
    assert_eq!(
        got,
        [
            format!("failing:{id}:approved:{ADMIN}"),
            format!("recorder:{id}")
        ],
        "every hook ran in order after the COMMIT (the failing one saw the committed release), past a panic"
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM job_release WHERE job_id = $1 AND release_id = $2")
            .bind(f.job.job_id)
            .bind(&id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(state, "approved", "a failing hook undoes nothing");

    // the release stays usable: its readiness reads it, and a reused approve (no new cut) runs no hook
    let st = read(&e, &f, &tok, "releases/status").await;
    assert_eq!(st["approved_waiting"], id.as_str(), "{st}");
    let at = selection_updated_at(&e, &f).await;
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["created"], false);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(log.lock().unwrap().len(), 2, "no new cut, no hook run");

    // a later cut runs the hooks again
    put_languages(&e, &f, &tok, json!(["en", "de"])).await;
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let next = b["release"]["id"].as_str().unwrap().to_string();
    let got = wait_for(&log, 4).await;
    assert_eq!(got.len(), 4, "{got:?}");
    assert_eq!(got[3], format!("recorder:{next}"));

    // a failing hook never blocks a later apply (B2 T12): the cut applies and goes live
    let path = uri(&f, &format!("releases/{next}/apply"));
    let (s, b) = send(&e.app, req("POST", &path, Some(&tok), json!({}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["applied"], true, "{b}");
    assert_eq!(b["release"]["state"], "current", "{b}");
}

// ---- T11 review I2: a stored release's identity is recomputed from what it stores now --------------------------

/// A job-scope test kind with one place whose value is always 1 (no table).
struct Constant;

impl DesignTarget for Constant {
    fn kind(&self) -> &'static str {
        "scratch_const"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, _r: &DraftRef) -> String {
        "scratch-const".to_string()
    }
    fn get<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _at: Place<'a>,
        _r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async { Ok(Some(json!(1))) })
    }
    fn set<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _at: Place<'a>,
        _r: &'a DraftRef,
        _v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async { Ok(()) })
    }
    fn places<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        _at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async { Ok(Some(vec![DraftRef::new()])) })
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        v.as_ref().map(Value::to_string)
    }
}

#[tokio::test]
async fn late_registered_kind_is_not_a_change() {
    let (e, f, tok) = setup().await;
    let at = put_target(&e, &f, &tok, sel(&["chat"]), Value::Null).await;
    let (s, b) = approve(
        &e,
        &f,
        &tok,
        json!({"apply": true, "selection_seen_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["applied"], true, "{b}");
    let id = b["release"]["id"].as_str().unwrap().to_string();

    // a plan registers a new snapshot kind after the cut (its stored hash cannot cover it)
    let mut reg = design::registry(e.state.compiler.clone());
    reg.register_scratch(Arc::new(Constant));
    let mut st = e.state.clone();
    st.design = Arc::new(reg);
    let app = agent_api::router(st);

    let (s, b) = approve_on(
        &app,
        &f,
        &tok,
        json!({"apply": false, "selection_seen_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "nothing_to_publish", "{b}");
    assert!(b["error"]["message"].as_str().unwrap().contains(&id), "{b}");
    assert_eq!(
        count(&e, "SELECT count(*) FROM job_release WHERE job_id = $1", &f).await,
        1,
        "no 0-draft cut"
    );
}
