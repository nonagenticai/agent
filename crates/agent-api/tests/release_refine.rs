//! B2 T8: overlay refinements (spec §4, D8/D54) — `GET /api/jobs/{job}/{object}` (refinementLayers),
//! `POST /api/jobs/{job}/refinements/{op}` (refine) and `PUT /api/jobs/{job}/context-spaces/{key}`
//! (saveContextSpaceOverride). A refinement is a DRAFT change (ledger row, live unchanged); one test per refusal code
//! of §4. The object `activity` is read here but written on the Journey page (B8 T7, Q-R3 (a): `tests/b8/job_refine.rs`);
//! these cases run on `context_space`. Real Postgres 18 (one container for this binary, a fresh database per test), through the RLS runtime role
//! `agent_app`, on a job created through `POST /api/jobs` from [`template`]: the test offering's job layer
//! (`common::platform`) plus the rows these refinements need a base for (an entity and attribute, a context space, a
//! knowledge store, a gate with generated options, an activity), authored through the platform routes and tagged as
//! one new version.
use crate::common;

use agent_api::design::{self, DesignOf, DraftRef, JobRef};
use agent_api::release::hash::{bundle_hash, row_hash};
use axum::http::StatusCode;
use common::design_contract::{self, ContractJob, DESIGNER};
use common::{TestEnv, get, insert_user, json, person, send, super_admin};
use serde_json::{Value, json};
use std::sync::OnceLock;
use tokio::sync::Mutex;
use uuid::Uuid;

const R1: &str = "cfg-2026-10-20.1";

struct Fx {
    e: TestEnv,
    f: ContractJob,
    token: String,
}

fn template_lock() -> &'static Mutex<()> {
    static L: OnceLock<Mutex<()>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(()))
}

/// The job template (layer, version) whose bundle has a base for every refinable object: the offering's job layer
/// gets the rows below, then one tag (a job tag needs the new untagged rows). Idempotent per database.
async fn template(e: &TestEnv) -> (String, i32) {
    let o = common::platform::offering(e).await;
    let _g = template_lock().lock().await;
    let authored: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM job_template_tag \
          WHERE layer = $1 AND version > $2 AND status = 'tagged')",
    )
    .bind(o.job_layer)
    .bind(o.version)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    if !authored {
        let job: String = sqlx::query_scalar("SELECT id::text FROM layer WHERE key = $1")
            .bind(o.job_layer)
            .fetch_one(&e.owner)
            .await
            .unwrap();
        let rows = [
            ("entity", "Order", json!({"label": "Order"})),
            (
                "attribute",
                "Order.order_id",
                json!({"data_type": "string"}),
            ),
            (
                "context_space",
                "retail:order-support:order_status",
                json!({"label": "Order status", "area": common::platform::AREA,
                       "required": [], "entities": ["Order"]}),
            ),
            (
                "knowledge_store",
                "order-help",
                json!({"label": "Order help", "kind": "leaf"}),
            ),
            (
                "gate_question",
                "GT",
                json!({"group": "base", "point": "P1", "type": "choice",
                       "question": "Which theme is the customer in?",
                       "options": [
                           {"key": "need", "label": "Need", "description": "", "generated": true},
                           {"key": "issue", "label": "Issue", "description": "", "generated": true}],
                       "generated_from": null, "threshold": "top ≥ 0.5", "threshold_value": 0.5,
                       "fail": "open", "safety": false, "effect": "Tone; stage prior.",
                       "rule": {"fire_when": "in", "threshold": null, "fire_values": ["issue"],
                                "min_confidence": 0.5},
                       "action": "annotate", "tools": null}),
            ),
            (
                "activity",
                "support:track_order",
                json!({"label": "Track an order", "actor": "customer", "required": true,
                       "operations": [{"op": "knowledge.search", "required": true},
                                      {"op": "case.read", "required": true},
                                      {"op": "case.create", "required": true}]}),
            ),
        ];
        for (kind, key, fields) in rows {
            let (s, b) = send(
                &e.app,
                json(
                    "POST",
                    &format!("/api/layers/{job}/rows"),
                    Some(&super_admin()),
                    json!({"kind": kind, "key": key, "op": "add", "fields": fields}),
                ),
            )
            .await;
            assert_eq!(s, StatusCode::CREATED, "{kind} {key}: {b}");
        }
        let (s, b) = send(
            &e.app,
            json(
                "POST",
                &format!(
                    "/api/platform/templates/{}/tags",
                    o.job_layer.replace('/', ".")
                ),
                Some(&super_admin()),
                json!({}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "tag {}: {b}", o.job_layer);
    }
    let version: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM job_template_tag WHERE layer = $1 AND status = 'tagged'",
    )
    .bind(o.job_layer)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    (o.job_layer.to_owned(), version)
}

async fn setup() -> Fx {
    let e = common::env().await;
    let t = template(&e).await;
    let f = design_contract::new_job_for_on(&e, Uuid::new_v4(), t).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    Fx {
        e,
        f,
        token: person(DESIGNER, &[]),
    }
}

fn job_key(x: &Fx) -> &str {
    &x.f.job.job_key
}

async fn layers(x: &Fx, object: &str) -> (StatusCode, Value) {
    let uri = format!("/api/jobs/{}/{object}?layer=base", job_key(x));
    send(&x.e.app, get(&uri, Some(&x.token))).await
}

async fn layers_ok(x: &Fx, object: &str) -> Value {
    let (s, b) = layers(x, object).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

async fn refine(x: &Fx, op: &str, object: &str, key: &str) -> (StatusCode, Value) {
    let uri = format!("/api/jobs/{}/refinements/{op}", job_key(x));
    send(
        &x.e.app,
        json(
            "POST",
            &uri,
            Some(&x.token),
            json!({"object": object, "key": key}),
        ),
    )
    .await
}

async fn save_space(x: &Fx, key: &str, body: Value) -> (StatusCode, Value) {
    let uri = format!("/api/jobs/{}/context-spaces/{key}", job_key(x));
    send(&x.e.app, json("PUT", &uri, Some(&x.token), body)).await
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

#[track_caller]
fn refused(got: (StatusCode, Value), status: StatusCode, want: &str) -> Value {
    assert_eq!(got.0, status, "{}", got.1);
    assert_eq!(code(&got.1), want, "{}", got.1);
    got.1
}

fn key_of(item: &Value) -> String {
    item["key"].as_str().expect("an item key").to_string()
}

/// The first base item of `object` matching `pick`.
async fn base_where(x: &Fx, object: &str, pick: impl Fn(&Value) -> bool) -> Value {
    let l = layers_ok(x, object).await;
    l["base"]
        .as_array()
        .expect("base is a list")
        .iter()
        .find(|i| pick(i))
        .cloned()
        .unwrap_or_else(|| panic!("no matching {object} base item in {l}"))
}

fn tenant_row(l: &Value, key: &str) -> Option<Value> {
    l["tenant"]
        .as_array()
        .expect("tenant is a list")
        .iter()
        .find(|r| r["key"] == json!(key))
        .cloned()
}

async fn ledger(x: &Fx) -> Vec<(String, String, String)> {
    sqlx::query_as("SELECT key, page, label FROM draft_ledger WHERE job_id = $1 ORDER BY key")
        .bind(x.f.job.job_id)
        .fetch_all(&x.e.owner)
        .await
        .unwrap()
}

fn place(pairs: &[(&str, &str)]) -> DraftRef {
    let mut m = DraftRef::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), Value::String((*v).to_string()));
    }
    m
}

/// The value of `kind` at `r` in the job's design `of`.
async fn design_value(x: &Fx, of: DesignOf, kind: &str, r: &DraftRef) -> Option<Value> {
    let jr: JobRef = x.f.job_ref();
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let v = design::value(&mut tx, &x.e.state.design, &jr, &of, kind, r)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    v
}

/// A tenant overlay row written directly (owner): states the API cannot reach on this job.
async fn owner_row(x: &Fx, object: &str, key: &str, kind: &str) {
    let item = (kind != "tombstone").then(|| json!({"key": key, "label": key}));
    sqlx::query(
        "INSERT INTO job_overlay_row (tenant_id, job_id, object, key, kind, item, base_hash, content_hash, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, NULL, 'x', 'test')",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .bind(object)
    .bind(key)
    .bind(kind)
    .bind(item)
    .execute(&x.e.owner)
    .await
    .unwrap();
}

/// An approved release row and one open conflict of `kind` on `(object, key)` (owner). Returns the conflict id.
async fn open_conflict(x: &Fx, kind: &str, object: &str, key: &str, tenant_row: &str) -> i64 {
    let sha = |c: char| format!("sha256:{}", c.to_string().repeat(64));
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, snapshot_kinds)
           VALUES ($1, $2, $3, $4, $5, '{"sections": []}', '{}', '[]', '{}', '{}', '[]', 'approved', 'u-rm', now(),
               ARRAY['overlay'])
           ON CONFLICT DO NOTHING"#,
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .bind(R1)
    .bind(sha('a'))
    .bind(sha('b'))
    .execute(&x.e.owner)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO release_conflict (tenant_id, job_id, release_id, kind, object, key, tenant_row, detail) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, 'the base moved') RETURNING id",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .bind(R1)
    .bind(kind)
    .bind(object)
    .bind(key)
    .bind(tenant_row)
    .fetch_one(&x.e.owner)
    .await
    .unwrap()
}

fn not_required(i: &Value) -> bool {
    i.get("required") != Some(&json!(true))
}

// ---- refinementLayers -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn layers_answer_the_full_overlay_layers_shape() {
    let x = setup().await;
    for object in [
        "activity",
        "context_space",
        "ontology_attribute",
        "gate_option",
        "knowledge_store",
    ] {
        let l = layers_ok(&x, object).await;
        assert_eq!(l["object"], json!(object), "{l}");
        assert!(
            l["base"].as_array().is_some_and(|b| !b.is_empty()),
            "{object}: the draft bundle has a base: {l}"
        );
        assert_eq!(l["tenant"], json!([]), "{l}");
        assert!(l["draft_rev"].is_i64(), "{l}");
        assert!(l["entities"].is_array(), "{l}");
        let mut keys: Vec<&str> = l.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["base", "draft_rev", "entities", "object", "tenant"]);
    }
    // `?layer=tenant` answers the same full object
    let uri = format!("/api/jobs/{}/activity?layer=tenant", job_key(&x));
    let (s, b) = send(&x.e.app, get(&uri, Some(&x.token))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, layers_ok(&x, "activity").await);
}

#[tokio::test]
async fn unknown_object_404() {
    let x = setup().await;
    refused(
        layers(&x, "job_setting").await,
        StatusCode::NOT_FOUND,
        "unknown_object",
    );
    refused(
        layers(&x, "nonsense").await,
        StatusCode::NOT_FOUND,
        "unknown_object",
    );
    refused(
        refine(&x, "hide", "nonsense", "k").await,
        StatusCode::NOT_FOUND,
        "unknown_object",
    );
    assert!(ledger(&x).await.is_empty());
}

// ---- refine: draft, not live ----------------------------------------------------------------------------------------

#[tokio::test]
async fn refine_is_draft() {
    let x = setup().await;
    let base = base_where(&x, "context_space", not_required).await;
    let key = key_of(&base);
    let rev0 = layers_ok(&x, "context_space").await["draft_rev"]
        .as_i64()
        .unwrap();

    let (s, b) = refine(&x, "hide", "context_space", &key).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"draft_rev": rev0 + 1}));

    // the tenant row: a tombstone over the base item, hashed with row_hash
    let l = layers_ok(&x, "context_space").await;
    assert_eq!(l["draft_rev"], json!(rev0 + 1));
    let row = tenant_row(&l, &key).expect("a tenant row");
    let h = row_hash(&base, &[]).unwrap();
    assert_eq!(row["kind"], json!("tombstone"), "{row}");
    assert_eq!(row["item"], Value::Null);
    assert_eq!(row["base_hash"], json!(h));
    assert_eq!(row["content_hash"], json!(format!("tomb-{h}")));
    assert_eq!(row["updated_by"], json!(DESIGNER));
    assert_eq!(row["updated_by_name"], json!(DESIGNER));

    // ledger row present; live unchanged, draft has it
    assert_eq!(
        ledger(&x).await,
        [(
            format!("overlay:context_space:{key}"),
            "J13".to_string(),
            format!("context space {key} hidden")
        )]
    );
    let r = place(&[("object", "context_space"), ("key", &key)]);
    assert_eq!(design_value(&x, DesignOf::Live, "overlay", &r).await, None);
    let draft = design_value(&x, DesignOf::Draft, "overlay", &r)
        .await
        .expect("a draft value");
    assert_eq!(draft["kind"], json!("tombstone"));
}

#[tokio::test]
async fn hide_then_unhide_drops_ledger_row() {
    let x = setup().await;
    let key = key_of(&base_where(&x, "context_space", not_required).await);
    let (s, b) = refine(&x, "hide", "context_space", &key).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ledger(&x).await.len(), 1);

    let (s, b) = refine(&x, "unhide", "context_space", &key).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(ledger(&x).await.is_empty(), "back to live drops the row");
    assert_eq!(layers_ok(&x, "context_space").await["tenant"], json!([]));
}

// ---- refine refusals ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn invalid_op_422() {
    let x = setup().await;
    let key = key_of(&base_where(&x, "context_space", not_required).await);
    refused(
        refine(&x, "delete", "context_space", &key).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_op",
    );
}

#[tokio::test]
async fn not_in_release_409() {
    let x = setup().await;
    let b = refused(
        refine(&x, "hide", "context_space", "no:such_space").await,
        StatusCode::CONFLICT,
        "not_in_release",
    );
    assert_eq!(
        b["error"]["message"],
        json!("“no:such_space” is not in the release; remove your own row with Revert.")
    );
}

#[tokio::test]
async fn already_hidden_409() {
    let x = setup().await;
    let key = key_of(&base_where(&x, "context_space", not_required).await);
    assert_eq!(
        refine(&x, "hide", "context_space", &key).await.0,
        StatusCode::OK
    );
    refused(
        refine(&x, "hide", "context_space", &key).await,
        StatusCode::CONFLICT,
        "already_hidden",
    );
}

#[tokio::test]
async fn not_hidden_409() {
    let x = setup().await;
    let key = key_of(&base_where(&x, "context_space", not_required).await);
    refused(
        refine(&x, "unhide", "context_space", &key).await,
        StatusCode::CONFLICT,
        "not_hidden",
    );
}

#[tokio::test]
async fn nothing_to_revert_409() {
    let x = setup().await;
    let key = key_of(&base_where(&x, "context_space", not_required).await);
    refused(
        refine(&x, "revert", "context_space", &key).await,
        StatusCode::CONFLICT,
        "nothing_to_revert",
    );
}

#[tokio::test]
async fn nothing_to_rebase_409() {
    let x = setup().await;
    let key = key_of(&base_where(&x, "context_space", not_required).await);
    refused(
        refine(&x, "rebase", "context_space", &key).await,
        StatusCode::CONFLICT,
        "nothing_to_rebase",
    );
    // a hide of an item the release removed: nothing to rebase onto
    owner_row(&x, "context_space", "gone:item", "tombstone").await;
    let b = refused(
        refine(&x, "rebase", "context_space", "gone:item").await,
        StatusCode::CONFLICT,
        "nothing_to_rebase",
    );
    assert_eq!(
        b["error"]["message"],
        json!("The release removed “gone:item”; there is nothing to hide. Revert instead.")
    );
}

#[tokio::test]
async fn parent_missing_409() {
    let x = setup().await;
    owner_row(&x, "context_space", "orphan:item", "extension").await;
    open_conflict(
        &x,
        "parent_missing",
        "context_space",
        "orphan:item",
        "extension",
    )
    .await;
    refused(
        refine(&x, "rebase", "context_space", "orphan:item").await,
        StatusCode::CONFLICT,
        "parent_missing",
    );
}

#[tokio::test]
async fn rebase_without_base_makes_an_extension_and_saves_the_decision() {
    let x = setup().await;
    owner_row(&x, "context_space", "mine:item", "override").await;
    let id = open_conflict(&x, "base_removed", "context_space", "mine:item", "override").await;
    let (s, b) = refine(&x, "rebase", "context_space", "mine:item").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = tenant_row(&layers_ok(&x, "context_space").await, "mine:item").unwrap();
    assert_eq!(row["kind"], json!("extension"));
    assert_eq!(row["base_hash"], Value::Null);
    let ledger_id: i64 =
        sqlx::query_scalar("SELECT id FROM draft_ledger WHERE job_id = $1 AND key = $2")
            .bind(x.f.job.job_id)
            .bind("overlay:context_space:mine:item")
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    let (status, decision): (String, Value) =
        sqlx::query_as("SELECT status, decision FROM release_conflict WHERE id = $1")
            .bind(id)
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(status, "open", "decided in draft, still open");
    assert_eq!(decision["choice"], json!("rebased"));
    assert_eq!(
        decision["draft_key"],
        json!("overlay:context_space:mine:item")
    );
    assert_eq!(decision["draft_id"], json!(format!("d-{ledger_id}")));
    assert_eq!(decision["by"], json!(DESIGNER));
}

#[tokio::test]
async fn rebase_with_a_base_restamps_base_hash_and_keeps_the_kind() {
    let x = setup().await;
    let base = base_where(&x, "context_space", not_required).await;
    let key = key_of(&base);
    assert_eq!(
        refine(&x, "hide", "context_space", &key).await.0,
        StatusCode::OK
    );
    sqlx::query(
        "UPDATE job_overlay_row SET base_hash = 'sha256:old' WHERE job_id = $1 AND object = 'context_space' AND key = $2",
    )
    .bind(x.f.job.job_id)
    .bind(&key)
    .execute(&x.e.owner)
    .await
    .unwrap();
    let (s, b) = refine(&x, "rebase", "context_space", &key).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = tenant_row(&layers_ok(&x, "context_space").await, &key).unwrap();
    assert_eq!(row["kind"], json!("tombstone"), "{row}");
    assert_eq!(
        row["base_hash"],
        json!(row_hash(&base, &[]).unwrap()),
        "{row}"
    );
}

#[tokio::test]
async fn unhide_with_an_open_conflict_saves_reverted() {
    let x = setup().await;
    let key = key_of(&base_where(&x, "context_space", not_required).await);
    assert_eq!(
        refine(&x, "hide", "context_space", &key).await.0,
        StatusCode::OK
    );
    let id = open_conflict(&x, "base_changed", "context_space", &key, "tombstone").await;
    let (s, b) = refine(&x, "unhide", "context_space", &key).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let decision: Value = sqlx::query_scalar("SELECT decision FROM release_conflict WHERE id = $1")
        .bind(id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(decision["choice"], json!("reverted"), "{decision}");
    assert_eq!(
        decision["draft_key"],
        json!(format!("overlay:context_space:{key}"))
    );
    // back to live: no ledger row carries the choice (fixture linkDecisions → null)
    assert!(ledger(&x).await.is_empty());
    assert_eq!(decision["draft_id"], Value::Null, "{decision}");
}

#[tokio::test]
async fn static_settings_route_wins_over_object() {
    let x = setup().await;
    let uri = format!("/api/jobs/{}/settings", job_key(&x));
    let (s, b) = send(&x.e.app, get(&uri, Some(&x.token))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(
        b["override"].is_object() && b["effective"].is_object(),
        "{b}"
    );
}

#[tokio::test]
async fn observer_reads_layers_but_refine_is_403() {
    let x = setup().await;
    insert_user(&x.e.owner, "u-obs", x.f.tenant, "tenant_user").await;
    let obs = person("u-obs", &[]);
    let uri = format!("/api/jobs/{}/activity", job_key(&x));
    let (s, b) = send(&x.e.app, get(&uri, Some(&obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let uri = format!("/api/jobs/{}/refinements/hide", job_key(&x));
    let (s, b) = send(
        &x.e.app,
        json(
            "POST",
            &uri,
            Some(&obs),
            json!({"object": "context_space", "key": "k"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], json!("job_designer"), "{b}");
}

// ---- job_setting routes to the setting target -----------------------------------------------------------------------

#[tokio::test]
async fn job_setting_hide_unhide_and_refusals() {
    let x = setup().await;
    refused(
        refine(&x, "hide", "job_setting", "colour").await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown_setting",
    );
    // KH T4 (D108): the preamble is the job's identity instruction, not a tenant setting
    refused(
        refine(&x, "hide", "job_setting", "preamble").await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown_setting",
    );
    refused(
        refine(&x, "revert", "job_setting", "stance").await,
        StatusCode::CONFLICT,
        "nothing_to_revert",
    );
    refused(
        refine(&x, "rebase", "job_setting", "stance").await,
        StatusCode::CONFLICT,
        "nothing_to_rebase",
    );
    refused(
        refine(&x, "unhide", "job_setting", "stance").await,
        StatusCode::CONFLICT,
        "not_hidden",
    );

    let (s, b) = refine(&x, "hide", "job_setting", "stance").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (value, by): (Option<String>, String) = sqlx::query_as(
        "SELECT value, updated_by FROM job_setting_override WHERE job_id = $1 AND key = 'stance'",
    )
    .bind(x.f.job.job_id)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!((value, by.as_str()), (None, DESIGNER));
    assert_eq!(
        ledger(&x).await,
        [(
            "setting:stance".to_string(),
            "J13".to_string(),
            "Setting stance hidden".to_string()
        )]
    );
    let r = place(&[("key", "stance")]);
    assert_eq!(design_value(&x, DesignOf::Live, "setting", &r).await, None);

    let (s, b) = refine(&x, "unhide", "job_setting", "stance").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(ledger(&x).await.is_empty());
}

/// T8 review I1 ruling: "Keep mine" on a job_setting conflict re-stamps `base_hash` onto the new base. The setting
/// target compares `(value, base_hash)`, so against a live value that holds the old override this is a recorded draft
/// change, `draft_rev` moves, and the decision points at its ledger row.
#[tokio::test]
async fn job_setting_rebase_is_a_draft_change_and_links_the_decision() {
    let x = setup().await;
    let uri = format!("/api/jobs/{}/settings", job_key(&x));
    let (s, view) = send(&x.e.app, get(&uri, Some(&x.token))).await;
    assert_eq!(s, StatusCode::OK, "{view}");
    let want_hash: Option<String> = view["base"]["stance"]
        .as_str()
        .map(|v| bundle_hash(&json!(v)).unwrap());

    // the released state: an override over an older base, in the live baseline and the working row
    sqlx::query(
        "INSERT INTO job_setting_override (tenant_id, job_id, key, value, base_hash, updated_by) \
         VALUES ($1, $2, 'stance', 'Mine', 'sha256:old', 'test')",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .execute(&x.e.owner)
    .await
    .unwrap();
    let old = json!({"value": "Mine", "base_hash": "sha256:old"});
    // release_baseline refuses UPDATE (release_snapshot_immutable): swap the row as the owner
    let mut tx = x.e.owner.begin().await.unwrap();
    let (tenant, kind, r#ref): (Uuid, String, Value) = sqlx::query_as(
        "DELETE FROM release_baseline WHERE job_id = $1 AND key = 'setting:stance' \
         RETURNING tenant_id, kind, ref",
    )
    .bind(x.f.job.job_id)
    .fetch_one(&mut *tx)
    .await
    .expect("the job's baseline holds setting:stance");
    sqlx::query(
        "INSERT INTO release_baseline (tenant_id, job_id, key, kind, ref, snap) \
         VALUES ($1, $2, 'setting:stance', $3, $4, $5)",
    )
    .bind(tenant)
    .bind(x.f.job.job_id)
    .bind(kind)
    .bind(r#ref)
    .bind(json!({ "v": old.clone() }))
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let r = place(&[("key", "stance")]);
    assert_eq!(
        design_value(&x, DesignOf::Live, "setting", &r).await,
        Some(old.clone())
    );
    let id = open_conflict(&x, "job_setting", "job_setting", "stance", "override").await;
    let rev0 = layers_ok(&x, "activity").await["draft_rev"]
        .as_i64()
        .unwrap();

    let (s, b) = refine(&x, "rebase", "job_setting", "stance").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({"draft_rev": rev0 + 1}),
        "a rebase is a draft change"
    );

    let (value, base_hash): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT value, base_hash FROM job_setting_override WHERE job_id = $1 AND key = 'stance'",
    )
    .bind(x.f.job.job_id)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(value.as_deref(), Some("Mine"), "the value is kept");
    assert_eq!(base_hash, want_hash, "stamped onto the current base");
    assert_eq!(
        design_value(&x, DesignOf::Live, "setting", &r).await,
        Some(old),
        "live unchanged"
    );

    let ledger_id: i64 = sqlx::query_scalar(
        "SELECT id FROM draft_ledger WHERE job_id = $1 AND key = 'setting:stance'",
    )
    .bind(x.f.job.job_id)
    .fetch_one(&x.e.owner)
    .await
    .expect("a ledger row carries the rebase");
    let decision: Value = sqlx::query_scalar("SELECT decision FROM release_conflict WHERE id = $1")
        .bind(id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(decision["choice"], json!("rebased"), "{decision}");
    assert_eq!(decision["draft_key"], json!("setting:stance"));
    assert_eq!(decision["draft_id"], json!(format!("d-{ledger_id}")));
}

// ---- saveContextSpaceOverride ---------------------------------------------------------------------------------------

#[tokio::test]
async fn context_space_override_copy_on_write_keeps_base_hash() {
    let x = setup().await;
    let base = base_where(&x, "context_space", |_| true).await;
    let key = key_of(&base);
    let h = row_hash(&base, &[]).unwrap();

    let (s, b) = save_space(&x, &key, json!({"label": "Mine"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = tenant_row(&layers_ok(&x, "context_space").await, &key).expect("an override");
    assert_eq!(row["kind"], json!("override"));
    assert_eq!(row["base_hash"], json!(h));
    assert_eq!(row["item"]["label"], json!("Mine"));
    assert_eq!(
        row["item"]["required"], base["required"],
        "copied from base"
    );
    assert_eq!(
        row["content_hash"],
        json!(row_hash(&row["item"], &[]).unwrap())
    );

    let (s, b) = save_space(&x, &key, json!({"required": ["order_id"]})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let again = tenant_row(&layers_ok(&x, "context_space").await, &key).unwrap();
    assert_eq!(again["kind"], json!("override"));
    assert_eq!(
        again["base_hash"],
        json!(h),
        "an update keeps the base hash"
    );
    assert_eq!(again["item"]["label"], json!("Mine"), "edits stack");
    assert_eq!(again["item"]["required"], json!(["order_id"]));
    assert_ne!(again["content_hash"], row["content_hash"]);

    // B5 T7: a required-only patch is J12's `space_required` (a projection of the same overlay place)
    assert_eq!(
        ledger(&x).await,
        [
            (
                format!("overlay:context_space:{key}"),
                "J12".to_string(),
                format!("Context space {key} changed")
            ),
            (
                format!("space:{key}"),
                "J12".to_string(),
                "Mine: required fields order_id".to_string()
            ),
        ]
    );
    let r = place(&[("object", "context_space"), ("key", &key)]);
    assert_eq!(design_value(&x, DesignOf::Live, "overlay", &r).await, None);
}

#[tokio::test]
async fn context_space_invalid_body_422() {
    let x = setup().await;
    let key = key_of(&base_where(&x, "context_space", |_| true).await);
    for (body, msg) in [
        (json!({}), "Nothing to change."),
        (json!({"label": "  "}), "Label is required."),
        (
            json!({"label": "x".repeat(121)}),
            "Label is at most 120 characters.",
        ),
        (
            // with the label: B2's rules (a required-only patch is B5 J12's: 422 `invalid_slot`)
            json!({"label": "Returns", "required": ["order_id", "Bad-Slot"]}),
            "Required fields are snake_case names (not Bad-Slot).",
        ),
    ] {
        let b = refused(
            save_space(&x, &key, body).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_body",
        );
        assert_eq!(b["error"]["message"], json!(msg));
    }
    assert!(ledger(&x).await.is_empty());
}

#[tokio::test]
async fn context_space_unknown_entity_422() {
    let x = setup().await;
    let key = key_of(&base_where(&x, "context_space", |_| true).await);
    let b = refused(
        save_space(&x, &key, json!({"entities": ["NoSuchEntity"]})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown_entity",
    );
    assert_eq!(b["error"]["message"], json!("Unknown entity NoSuchEntity."));
    // a listed entity is accepted (retail has an ontology base, so the list is never empty)
    let known = layers_ok(&x, "context_space").await["entities"].clone();
    assert!(
        known.as_array().is_some_and(|a| !a.is_empty()),
        "the draft bundle lists entities: {known}"
    );
    let (s, b) = save_space(&x, &key, json!({ "entities": [known[0].clone()] })).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = tenant_row(&layers_ok(&x, "context_space").await, &key).expect("an override");
    assert_eq!(row["item"]["entities"], json!([known[0].clone()]));
}

#[tokio::test]
async fn context_space_edit_of_an_extension_keeps_its_kind() {
    let x = setup().await;
    owner_row(&x, "context_space", "mine_space", "extension").await;
    let (s, b) = save_space(&x, "mine_space", json!({"label": "Mine, edited"})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = tenant_row(&layers_ok(&x, "context_space").await, "mine_space").unwrap();
    assert_eq!(row["kind"], json!("extension"), "{row}");
    assert_eq!(row["base_hash"], Value::Null, "{row}");
    assert_eq!(row["item"]["label"], json!("Mine, edited"));
    assert_eq!(
        row["content_hash"],
        json!(row_hash(&row["item"], &[]).unwrap())
    );
    assert_eq!(ledger(&x).await.len(), 1);
}

#[tokio::test]
async fn context_space_not_found_404() {
    let x = setup().await;
    refused(
        save_space(&x, "no-such-space", json!({"label": "x"})).await,
        StatusCode::NOT_FOUND,
        "context_space_not_found",
    );
}

#[tokio::test]
async fn context_space_hidden_409() {
    let x = setup().await;
    let key = key_of(&base_where(&x, "context_space", |_| true).await);
    let (s, b) = refine(&x, "hide", "context_space", &key).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    refused(
        save_space(&x, &key, json!({"label": "x"})).await,
        StatusCode::CONFLICT,
        "hidden",
    );
}

#[tokio::test]
async fn foreign_job_is_not_found() {
    let x = setup().await;
    let uri = format!("/api/jobs/{}/activity", Uuid::new_v4());
    let (s, b) = send(&x.e.app, get(&uri, Some(&x.token))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "job_not_found");
}
