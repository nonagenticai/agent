//! B8 T7 + T7a — the job's journey refinements (J4): hide / restore, relabel, extensions and the use case card
//! (F-44), written to B2's `job_overlay_row` object `activity` through ledger kind `activity` (Q-R3 (a)), and J13's
//! 409 `edit_on_journey_page` for that object. Real Postgres 18, a job created through `POST /api/jobs`
//! (`job/customer_care_agent@4`, default selection `retail_ecommerce`), through the HTTP router as `agent_app`.
//! The activity cases of `tests/release_refine.rs` moved here from J13.
use agent_api::design::{self, DesignOf, DraftRef, JobRef};
use agent_api::release::overlay::base_hash;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::common::design_contract::{self, ContractJob, DESIGNER};
use crate::common::{self, TestEnv, get, insert_user, person, send};

const R1: &str = "cfg-2026-10-20.1";

/// The `base_hash` a tenant row over base activity `item` carries: the hash of its projection (the item without its
/// engine-granted `ops`).
fn base_of(item: &Value) -> String {
    base_hash("activity", item.as_object().expect("an activity item")).unwrap()
}

struct Fx {
    e: TestEnv,
    f: ContractJob,
    token: String,
}

async fn setup() -> Fx {
    let e = common::env().await;
    let f = crate::b8::support::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    Fx {
        e,
        f,
        token: person(DESIGNER, &[]),
    }
}

/// [`setup`] (kept as the name the required-activity test reads; `support::content` marks `delivery:where_is_my_order` required).
async fn setup_latest() -> Fx {
    let e = common::env().await;
    let f = crate::b8::support::new_job(&e).await;
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

async fn call(x: &Fx, method: &str, tail: &str, body: Value) -> (StatusCode, Value) {
    let uri = format!("/api/jobs/{}/refinements/journey/{tail}", job_key(x));
    send(&x.e.app, common::json(method, &uri, Some(&x.token), body)).await
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

#[track_caller]
fn ok(got: (StatusCode, Value)) -> Value {
    assert_eq!(got.0, StatusCode::OK, "{}", got.1);
    got.1
}

/// The draft bundle's base activities (`GET /api/jobs/{job}/activity`, J13's read).
async fn base_activities(x: &Fx) -> Vec<Value> {
    let uri = format!("/api/jobs/{}/activity", job_key(x));
    let (s, b) = send(&x.e.app, get(&uri, Some(&x.token))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["base"].as_array().expect("base is a list").clone()
}

async fn draft_rev(x: &Fx) -> i64 {
    let uri = format!("/api/jobs/{}/activity", job_key(x));
    let (s, b) = send(&x.e.app, get(&uri, Some(&x.token))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["draft_rev"].as_i64().expect("a draft_rev")
}

fn key_of(item: &Value) -> String {
    item["key"].as_str().expect("an item key").to_string()
}

fn not_required(i: &Value) -> bool {
    i.get("required") != Some(&json!(true))
}

/// The tenant's `activity` row (owner): `(kind, item, base_hash, content_hash)`.
async fn row(x: &Fx, key: &str) -> Option<(String, Option<Value>, Option<String>, Option<String>)> {
    sqlx::query_as(
        "SELECT kind, item, base_hash, content_hash FROM job_overlay_row \
          WHERE job_id = $1 AND object = 'activity' AND key = $2",
    )
    .bind(x.f.job.job_id)
    .bind(key)
    .fetch_optional(&x.e.owner)
    .await
    .unwrap()
}

async fn ledger(x: &Fx) -> Vec<(String, String, String)> {
    sqlx::query_as("SELECT key, page, label FROM draft_ledger WHERE job_id = $1 ORDER BY key")
        .bind(x.f.job.job_id)
        .fetch_all(&x.e.owner)
        .await
        .unwrap()
}

fn place(key: &str) -> DraftRef {
    let mut m = DraftRef::new();
    m.insert("key".to_string(), Value::String(key.to_string()));
    m
}

/// The value of kind `activity` at `key` in the job's design `of`.
async fn design_value(x: &Fx, of: DesignOf, key: &str) -> Option<Value> {
    let jr: JobRef = x.f.job_ref();
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let v = design::value(
        &mut tx,
        &x.e.state.design,
        &jr,
        &of,
        "activity",
        &place(key),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    v
}

/// Hide the first base activity the template lets go (not required, and K48 keeps every required operation
/// covered). Returns its key and base item.
async fn hide_first(x: &Fx) -> (String, Value) {
    for base in base_activities(x).await.into_iter().filter(not_required) {
        let key = key_of(&base);
        let (s, b) = call(x, "POST", "hide", json!({"key": key})).await;
        match (s, code(&b)) {
            (StatusCode::OK, _) => return (key, base),
            (StatusCode::UNPROCESSABLE_ENTITY, "required_operation") => {}
            other => panic!("hide {key}: {other:?} {b}"),
        }
    }
    panic!("no base activity can be hidden");
}

/// The first base activity (any), for relabel and card.
async fn first_base(x: &Fx) -> Value {
    base_activities(x)
        .await
        .into_iter()
        .next()
        .expect("the draft bundle has activities")
}

// ---- hide / restore ---------------------------------------------------------------------------------------------------

#[tokio::test]
async fn hide_writes_tombstone_draft() {
    let x = setup().await;
    let rev0 = draft_rev(&x).await;
    let (key, base) = hide_first(&x).await;
    assert_eq!(draft_rev(&x).await, rev0 + 1);

    // the tenant's row is a tombstone over the base item, hashed with row_hash
    let (kind, item, base_hash, content_hash) = row(&x, &key).await.expect("a tenant row");
    let h = base_of(&base);
    assert_eq!(kind, "tombstone");
    assert_eq!(item, None);
    assert_eq!(base_hash, Some(h.clone()));
    assert_eq!(content_hash, Some(format!("tomb-{h}")));

    // one ledger row, kind activity, page J4
    assert_eq!(
        ledger(&x).await,
        [(
            format!("journey:{key}"),
            "J4".to_string(),
            format!("hid {key}")
        )]
    );
    let kind: String = sqlx::query_scalar("SELECT kind FROM draft_ledger WHERE job_id = $1")
        .bind(x.f.job.job_id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(kind, "activity");

    // again, and an unknown key
    let (s, b) = call(&x, "POST", "hide", json!({"key": key})).await;
    refused((s, b), StatusCode::CONFLICT, "already_hidden");
    refused(
        call(&x, "POST", "hide", json!({"key": "no:such_activity"})).await,
        StatusCode::NOT_FOUND,
        "activity_not_found",
    );
}

#[tokio::test]
async fn journey_refinement_is_snapshotted_once_under_activity() {
    let x = setup().await;
    let (key, _) = hide_first(&x).await;
    let places = design_contract::places(&x.e, &x.f, "activity")
        .await
        .unwrap();
    assert!(places.contains(&place(&key)));
    let overlays = design_contract::places(&x.e, &x.f, "overlay")
        .await
        .unwrap();
    assert!(
        overlays
            .iter()
            .all(|r| r.get("object") != Some(&json!("activity")))
    );

    let jr = x.f.job_ref();
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let snapshot = design::take_snapshot(&mut tx, &x.e.state.design, &jr, &DesignOf::Draft)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let rows: Vec<_> = snapshot
        .iter()
        .filter(|r| r.r#ref.get("key") == Some(&json!(key)))
        .collect();
    assert_eq!(rows.len(), 1, "one snapshot row per journey refinement");
    assert_eq!(rows[0].kind, "activity");
    assert_eq!(rows[0].key, format!("journey:{key}"));
    assert_eq!(rows[0].snap.as_ref().unwrap()["kind"], "tombstone");
}

#[tokio::test]
async fn restore_removes_tombstone() {
    let x = setup().await;
    let (key, _) = hide_first(&x).await;
    ok(call(&x, "POST", "restore", json!({"key": key})).await);
    assert!(row(&x, &key).await.is_none());
    assert!(ledger(&x).await.is_empty(), "back to live drops the row");

    refused(
        call(&x, "POST", "restore", json!({"key": key})).await,
        StatusCode::CONFLICT,
        "not_refined",
    );
}

// Owner 2026-10-26: the template marks activities required (`ActivityFields.required`, `support::content` sets it on
// `delivery:where_is_my_order`); a tenant cannot hide one.
#[tokio::test]
async fn required_activity_cannot_be_hidden_422() {
    let x = setup_latest().await;
    let required = base_activities(&x)
        .await
        .into_iter()
        .find(|i| i["required"] == json!(true))
        .expect("the template has a required activity");
    assert!(
        ["delivery:where_is_my_order", "support:return"].contains(&key_of(&required).as_str()),
        "{required}"
    );
    refused(
        call(&x, "POST", "hide", json!({"key": key_of(&required)})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "required_activity",
    );
    assert!(ledger(&x).await.is_empty());
    assert!(row(&x, &key_of(&required)).await.is_none());
}

// ---- relabel ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn relabel_is_override() {
    let x = setup().await;
    let base = first_base(&x).await;
    let key = key_of(&base);
    ok(call(
        &x,
        "PUT",
        &format!("labels/{key}"),
        json!({"label": "  Our own label "}),
    )
    .await);

    let (kind, item, base_hash, _) = row(&x, &key).await.expect("a tenant row");
    assert_eq!(kind, "override");
    assert_eq!(item, Some(json!({"key": key, "label": "Our own label"})));
    assert_eq!(base_hash, Some(base_of(&base)));
    assert_eq!(
        ledger(&x).await,
        [(
            format!("journey:{key}"),
            "J4".to_string(),
            format!("relabelled {key} → “Our own label”")
        )]
    );

    // restore undoes the relabel (there is no hide)
    ok(call(&x, "POST", "restore", json!({"key": key})).await);
    assert!(row(&x, &key).await.is_none());
    assert!(ledger(&x).await.is_empty());

    // a label equal to the template's is no relabel
    let label = base["label"].as_str().unwrap().to_string();
    ok(call(
        &x,
        "PUT",
        &format!("labels/{key}"),
        json!({"label": "Other"}),
    )
    .await);
    ok(call(&x, "PUT", &format!("labels/{key}"), json!({"label": label})).await);
    assert!(row(&x, &key).await.is_none());

    refused(
        call(&x, "PUT", &format!("labels/{key}"), json!({"label": "  "})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "label_required",
    );
    refused(
        call(&x, "PUT", "labels/no:such_activity", json!({"label": "x"})).await,
        StatusCode::NOT_FOUND,
        "activity_not_found",
    );
}

// ---- extensions -------------------------------------------------------------------------------------------------------

fn extension(slug: &str) -> Value {
    json!({
        "stage": "support", "slug": slug, "label": " Gift cards ", "actor": "customer",
        "trigger": "asks about a gift card"
    })
}

#[tokio::test]
async fn extension_add_remove() {
    let x = setup().await;
    let key = "support:gift_cards";
    ok(call(&x, "POST", "extensions", extension("gift_cards")).await);

    let (kind, item, base_hash, content_hash) = row(&x, key).await.expect("a tenant row");
    assert_eq!(kind, "extension");
    assert_eq!(base_hash, None);
    assert!(content_hash.is_some());
    let item = item.expect("an item");
    assert_eq!(item["key"], json!(key));
    assert_eq!(item["stage"], json!("support"));
    assert_eq!(item["actor"], json!("customer"));
    assert_eq!(item["label"], json!("Gift cards"));
    assert_eq!(item["trigger"], json!("asks about a gift card"));
    assert_eq!(item["objects"][0]["object"], json!("knowledge"));
    assert_eq!(
        item["operations"],
        json!([{"op": "knowledge.search", "variant": null, "required": true}])
    );
    assert_eq!(
        ledger(&x).await,
        [(
            format!("journey:{key}"),
            "J4".to_string(),
            format!("added extension {key}")
        )]
    );

    // refusals
    refused(
        call(&x, "POST", "extensions", extension("gift_cards")).await,
        StatusCode::CONFLICT,
        "activity_exists",
    );
    refused(
        call(&x, "POST", "extensions", extension("Bad Slug")).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_key",
    );
    let mut blank = extension("other_cards");
    blank["label"] = json!("   ");
    refused(
        call(&x, "POST", "extensions", blank).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "label_required",
    );
    // your own activity is removed, never hidden; relabelling it changes its own label
    refused(
        call(&x, "POST", "hide", json!({"key": key})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "is_extension",
    );
    ok(call(
        &x,
        "PUT",
        &format!("labels/{key}"),
        json!({"label": "Vouchers"}),
    )
    .await);
    let (kind, item, ..) = row(&x, key).await.unwrap();
    assert_eq!(kind, "extension");
    assert_eq!(item.unwrap()["label"], json!("Vouchers"));

    ok(call(&x, "DELETE", &format!("extensions/{key}"), Value::Null).await);
    assert!(row(&x, key).await.is_none());
    assert!(ledger(&x).await.is_empty());
    refused(
        call(&x, "DELETE", &format!("extensions/{key}"), Value::Null).await,
        StatusCode::NOT_FOUND,
        "extension_not_found",
    );
}

// ---- the use case card (T7a, F-44) --------------------------------------------------------------------------------------

#[tokio::test]
async fn card_refine_and_reset() {
    let x = setup().await;
    let base = first_base(&x).await;
    let key = key_of(&base);
    let uri = format!("cards/{key}");

    ok(call(
        &x,
        "PUT",
        &uri,
        json!({"summary": "Start a return", "examples": ["I want to return my shoes"]}),
    )
    .await);
    let (kind, item, base_hash, _) = row(&x, &key).await.expect("a tenant row");
    assert_eq!(kind, "override");
    assert_eq!(base_hash, Some(base_of(&base)));
    assert_eq!(
        item,
        Some(json!({"key": key, "card": {
            "summary": "Start a return", "examples": ["I want to return my shoes"]}}))
    );
    assert_eq!(
        ledger(&x).await,
        [(
            format!("journey:{key}"),
            "J4".to_string(),
            format!("refined the use case card of {key}")
        )]
    );

    // a second refinement merges into the card; a null field drops that field
    let metric = json!({"key": "returns_started", "label": "Returns started", "target": "90%", "current": null});
    ok(call(&x, "PUT", &uri, json!({"metric": metric, "examples": null})).await);
    let (_, item, ..) = row(&x, &key).await.unwrap();
    assert_eq!(
        item.unwrap()["card"],
        json!({"summary": "Start a return", "metric": metric})
    );

    // a relabel and a card live in the one override
    ok(call(
        &x,
        "PUT",
        &format!("labels/{key}"),
        json!({"label": "Returns"}),
    )
    .await);
    let (_, item, ..) = row(&x, &key).await.unwrap();
    let item = item.unwrap();
    assert_eq!(item["label"], json!("Returns"));
    assert!(item["card"].is_object());

    // refusals
    refused(
        call(&x, "PUT", &uri, json!({"scenarios": []})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "scenarios_not_refinable",
    );
    refused(
        call(&x, "PUT", &uri, json!({"bogus": 1})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_card",
    );
    refused(
        call(&x, "PUT", &uri, json!({})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_card",
    );
    refused(
        call(&x, "PUT", &uri, json!({"metric": "90%"})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_card",
    );
    refused(
        call(&x, "PUT", "cards/no:such_activity", json!({"summary": "s"})).await,
        StatusCode::NOT_FOUND,
        "activity_not_found",
    );

    // reset drops the card and keeps the relabel; a second reset has nothing to undo
    ok(call(&x, "DELETE", &uri, Value::Null).await);
    let (_, item, ..) = row(&x, &key).await.unwrap();
    assert_eq!(item, Some(json!({"key": key, "label": "Returns"})));
    refused(
        call(&x, "DELETE", &uri, Value::Null).await,
        StatusCode::CONFLICT,
        "not_refined",
    );
    // with nothing else refined, the row goes
    ok(call(&x, "POST", "restore", json!({"key": key})).await);
    assert!(row(&x, &key).await.is_none());
    assert!(ledger(&x).await.is_empty());
}

// ---- D54 and the roles --------------------------------------------------------------------------------------------------

#[tokio::test]
async fn live_unchanged_until_release() {
    let x = setup().await;
    let (key, _) = hide_first(&x).await;
    assert_eq!(design_value(&x, DesignOf::Live, &key).await, None);
    let draft = design_value(&x, DesignOf::Draft, &key)
        .await
        .expect("a draft value");
    assert_eq!(draft["kind"], json!("tombstone"));

    // a relabel and a card are drafts too
    let other = base_activities(&x)
        .await
        .into_iter()
        .map(|i| key_of(&i))
        .find(|k| *k != key)
        .expect("a second activity");
    ok(call(
        &x,
        "PUT",
        &format!("labels/{other}"),
        json!({"label": "Mine"}),
    )
    .await);
    ok(call(
        &x,
        "PUT",
        &format!("cards/{other}"),
        json!({"summary": "s"}),
    )
    .await);
    assert_eq!(design_value(&x, DesignOf::Live, &other).await, None);
    let draft = design_value(&x, DesignOf::Draft, &other).await.unwrap();
    assert_eq!(draft["kind"], json!("override"));
    assert_eq!(draft["item"]["label"], json!("Mine"));
}

#[tokio::test]
async fn journey_writes_need_job_designer() {
    let x = setup().await;
    insert_user(&x.e.owner, "u-obs", x.f.tenant, "tenant_user").await;
    let obs = person("u-obs", &[]);
    let uri = format!("/api/jobs/{}/refinements/journey/hide", job_key(&x));
    let (s, b) = send(
        &x.e.app,
        common::json("POST", &uri, Some(&obs), json!({"key": "support:a"})),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], json!("job_designer"), "{b}");
}

// ---- J13 no longer writes activities ------------------------------------------------------------------------------------

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

#[tokio::test]
async fn j13_activity_refine_409_edit_on_journey_page() {
    let x = setup().await;
    let key = key_of(&first_base(&x).await);
    let uri = format!("/api/jobs/{}/refinements", job_key(&x));
    let post = |op: &str, body: Value| {
        let (app, token) = (x.e.app.clone(), x.token.clone());
        let uri = format!("{uri}/{op}");
        async move { send(&app, common::json("POST", &uri, Some(&token), body)).await }
    };

    for op in ["hide", "unhide", "revert", "rebase"] {
        let b = refused(
            post(op, json!({"object": "activity", "key": key})).await,
            StatusCode::CONFLICT,
            "edit_on_journey_page",
        );
        assert!(
            b["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains("Journey page")),
            "{b}"
        );
    }
    // an unknown object is still 404, before the activity rule
    refused(
        post("hide", json!({"object": "nonsense", "key": "k"})).await,
        StatusCode::NOT_FOUND,
        "unknown_object",
    );

    // resolveConflict of an activity conflict (F-43: B2's base_removed) is the Journey page's too
    sqlx::query(
        "INSERT INTO job_overlay_row (tenant_id, job_id, object, key, kind, item, base_hash, content_hash, updated_by) \
         VALUES ($1, $2, 'activity', 'mine:item', 'override', '{\"key\": \"mine:item\"}', NULL, 'x', 'test')",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .execute(&x.e.owner)
    .await
    .unwrap();
    let id = open_conflict(&x, "base_removed", "activity", "mine:item", "override").await;
    for op in ["revert", "rebase"] {
        refused(
            post(op, json!({"conflict": id})).await,
            StatusCode::CONFLICT,
            "edit_on_journey_page",
        );
    }
    let status: String = sqlx::query_scalar("SELECT status FROM release_conflict WHERE id = $1")
        .bind(id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(status, "open");
    assert!(ledger(&x).await.is_empty(), "nothing was written");
}
