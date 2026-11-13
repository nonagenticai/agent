//! B2 T7: the target, settings and languages routes (spec B2 §4): `GET/PUT /api/jobs/{job}/target`,
//! `GET/PUT /api/jobs/{job}/settings`, `DELETE /api/jobs/{job}/settings/{key}`, `GET/PUT /api/jobs/{job}/languages`.
//! Every write lands in the draft (D54): the live design is unchanged and the draft ledger carries the fixture label.
//! Real Postgres 18 (one container for this binary, a fresh database per test), through the RLS runtime role
//! `agent_app`, on a job created through `POST /api/jobs` (the B2 creation hook ran).
use crate::common;

use agent_api::design::{self, DesignOf, DraftRef, LedgerRow};
use agent_api::release::hash::bundle_hash;
use axum::http::StatusCode;
use common::design_contract::{self, ContractJob};
use common::{TestEnv, get, insert_user, json as req, person, send};
use serde_json::{Value, json};

/// A tenant admin of the contract tenant (every role).
const ADMIN: &str = "u-admin";

/// The draft bundle's settings for the contract job: the `job` row `common::platform::offering` authors on the base
/// layer (actor, stance).
const ACTOR: &str = "customer";
const STANCE: &str = "concise";

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

/// The template's default selection (`job/customer_care_agent@v4` `template_selection`), as stored.
fn default_sel() -> Value {
    json!({
        "profile": "retail_ecommerce",
        "industry": "retail",
        "departments": ["industry_dept/retail/customer_service"],
        "areas": [],
        "dimensions": {"channel": []},
        "region": null,
    })
}

/// A valid selection of the offering's options: the retail customer-service department, one area, `channel`, region eu.
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

fn message(b: &Value) -> &str {
    b["error"]["message"].as_str().unwrap_or("")
}

async fn put_target(e: &TestEnv, f: &ContractJob, tok: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, req("PUT", &uri(f, "target"), Some(tok), body)).await
}

/// The value of `kind` (at `{}`) in the job's `of` design.
async fn design_value(e: &TestEnv, f: &ContractJob, of: DesignOf, kind: &str) -> Option<Value> {
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let v = design::value(
        &mut tx,
        &e.state.design,
        &f.job_ref(),
        &of,
        kind,
        &DraftRef::new(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    v
}

async fn ledger_row(e: &TestEnv, f: &ContractJob, key: &str) -> Option<LedgerRow> {
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let r = design::ledger::find(&mut tx, &f.job_ref(), key)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    r
}

async fn selection_audit(e: &TestEnv, f: &ContractJob) -> Vec<Value> {
    sqlx::query_scalar(
        "SELECT details FROM audit_event \
          WHERE tenant_id = $1 AND action = 'job.selection_update' AND job = $2 ORDER BY id",
    )
    .bind(f.tenant)
    .bind(&f.job.job_key)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn save_target_is_draft_not_live() {
    let (e, f, tok) = setup().await;
    let (s, b) = send(&e.app, get(&uri(&f, "target"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["job"], json!(f.job.job_key));
    assert_eq!(b["selection"], default_sel());
    assert_eq!(b["selection_updated_by"], Value::Null);
    assert_eq!(b["selection_updated_by_name"], json!("—"));
    assert_eq!(b["selection_updated_at"], Value::Null);
    assert_eq!(b["selection_error"], Value::Null);
    assert_eq!(b["history"], json!([]));
    assert!(
        b["options"]["profiles"]
            .as_array()
            .is_some_and(|p| !p.is_empty()),
        "{b}"
    );

    let (s, b) = put_target(
        &e,
        &f,
        &tok,
        json!({"selection": sel(&["chat"]), "expected_updated_at": null}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let first = "area + order-support · channel + chat · region + eu";
    assert_eq!(b["selection"], sel(&["chat"]));
    assert_eq!(b["selection_updated_by"], json!(ADMIN));
    assert_eq!(b["selection_updated_by_name"], json!(ADMIN));
    assert_eq!(b["history"].as_array().map(Vec::len), Some(1), "{b}");
    let h = &b["history"][0];
    assert_eq!(h["action"], json!("job.selection_update"));
    assert_eq!(h["actor"], json!(ADMIN));
    assert_eq!(h["actor_name"], json!(ADMIN));
    assert_eq!(h["change"], json!(first));
    assert_eq!(h["old"], default_sel());
    assert_eq!(h["new"], sel(&["chat"]));
    let at = b["selection_updated_at"]
        .as_str()
        .expect("selection_updated_at is set by a save")
        .to_string();

    // D54: Production still runs the template default; the draft has the new selection
    assert_eq!(
        design_value(&e, &f, DesignOf::Live, "selection").await,
        Some(default_sel())
    );
    assert_eq!(
        design_value(&e, &f, DesignOf::Draft, "selection").await,
        Some(sel(&["chat"]))
    );
    let row = ledger_row(&e, &f, "selection")
        .await
        .expect("the save left a selection draft row");
    assert_eq!(row.label, format!("Selection: {first}"));
    assert_eq!(
        (row.kind.as_str(), row.area.as_str(), row.page.as_str()),
        ("selection", "selection", "J14")
    );
    assert_eq!(row.by_sub, ADMIN);
    let audit = selection_audit(&e, &f).await;
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0]["change"], json!(first));
    assert_eq!(audit[0]["old"], default_sel());
    assert_eq!(audit[0]["new"], sel(&["chat"]));

    // a second save: history newest first; the label still reads against live
    let (s, b) = put_target(
        &e,
        &f,
        &tok,
        json!({"selection": sel(&["chat", "whatsapp"]), "expected_updated_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["history"].as_array().map(Vec::len), Some(2), "{b}");
    assert_eq!(b["history"][0]["change"], json!("channel + whatsapp"));
    assert_eq!(b["history"][1]["change"], json!(first));
    let row = ledger_row(&e, &f, "selection").await.expect("draft row");
    assert_eq!(
        row.label,
        "Selection: area + order-support · channel + chat · channel + whatsapp · region + eu"
    );
    assert_eq!(
        design_value(&e, &f, DesignOf::Live, "selection").await,
        Some(default_sel())
    );

    // the same selection in another order changes nothing
    let at = b["selection_updated_at"].as_str().unwrap().to_string();
    let (s, b) = put_target(
        &e,
        &f,
        &tok,
        json!({"selection": sel(&["whatsapp", "chat"]), "expected_updated_at": at}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["history"].as_array().map(Vec::len), Some(2), "{b}");
    assert_eq!(b["selection_updated_at"], json!(at));
    assert_eq!(selection_audit(&e, &f).await.len(), 2);
}

#[tokio::test]
async fn stale_expected_updated_at_409() {
    let (e, f, tok) = setup().await;
    let (s, b) = put_target(
        &e,
        &f,
        &tok,
        json!({"selection": sel(&["chat"]), "expected_updated_at": null}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let at = b["selection_updated_at"].as_str().unwrap().to_string();

    // a client that read before that save
    let (s, b) = put_target(
        &e,
        &f,
        &tok,
        json!({"selection": sel(&["email"]), "expected_updated_at": null}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "selection_changed");
    assert_eq!(
        message(&b),
        "Someone else changed the selection; reload and try again."
    );
    for stale in ["2020-01-01T00:00:00Z", "not a time", ""] {
        let (s, b) = put_target(
            &e,
            &f,
            &tok,
            json!({"selection": sel(&["email"]), "expected_updated_at": stale}),
        )
        .await;
        assert_eq!(s, StatusCode::CONFLICT, "{stale}: {b}");
        assert_eq!(code(&b), "selection_changed");
    }

    // nothing of the refused saves landed
    let (s, b) = send(&e.app, get(&uri(&f, "target"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["selection"], sel(&["chat"]));
    assert_eq!(b["selection_updated_at"], json!(at));
    assert_eq!(b["history"].as_array().map(Vec::len), Some(1), "{b}");
    assert_eq!(selection_audit(&e, &f).await.len(), 1);
}

/// A job with a release keeps its template stack: a move to another industry/department is refused, because apply
/// copies the release's pins back into `jobs.pins` (release/apply.rs).
#[tokio::test]
async fn a_released_job_cannot_move_to_another_industry() {
    let (e, f, tok) = setup().await;
    design_contract::publish(&e, &f).await;
    let pins_before: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE key = $1")
        .bind(&f.job.job_key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let (s, before) = send(&e.app, get(&uri(&f, "target"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{before}");
    let audit_before = selection_audit(&e, &f).await.len();
    let seen = before["selection_updated_at"].clone();
    let mut other = default_sel();
    other["industry"] = json!("tjd_other");
    other["departments"] = json!(["industry_dept/tjd_other/tjd_dept"]);
    let (s, b) = put_target(
        &e,
        &f,
        &tok,
        json!({"selection": other, "expected_updated_at": seen}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "hierarchy_immutable");
    let pins_after: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE key = $1")
        .bind(&f.job.job_key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(pins_after, pins_before, "the stack is unchanged");
    let (s, after) = send(&e.app, get(&uri(&f, "target"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{after}");
    assert_eq!(after["selection"], before["selection"]);
    assert_eq!(
        after["selection_updated_at"],
        before["selection_updated_at"]
    );
    assert_eq!(selection_audit(&e, &f).await.len(), audit_before);
}

#[tokio::test]
async fn invalid_selection_422() {
    let (e, f, tok) = setup().await;
    let mut other_dept = sel(&["chat"]);
    other_dept["areas"] = json!(["bills"]);
    let mut no_profile = sel(&["chat"]);
    no_profile["profile"] = json!("nope");
    let mut no_dept = sel(&["chat"]);
    no_dept["departments"] = json!([]);
    no_dept["areas"] = json!([]);
    let mut no_channel = sel(&["fax"]);
    no_channel["areas"] = json!([]);
    for (bad, want_code, want) in [
        (
            other_dept,
            "invalid_selection",
            "Areas outside the chosen departments: bills.",
        ),
        (no_profile, "invalid_selection", "Unknown profile “nope”."),
        (
            no_dept,
            "hierarchy_incomplete",
            "Choose at least one department of the job's industry.",
        ),
        (
            no_channel,
            "invalid_selection",
            "Dimension channel names values that no longer exist: fax.",
        ),
    ] {
        let (s, b) = put_target(
            &e,
            &f,
            &tok,
            json!({"selection": bad, "expected_updated_at": null}),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{want}: {b}");
        assert_eq!(code(&b), want_code);
        assert_eq!(message(&b), want);
    }

    // not a selection at all: the body extractor refuses it
    let (s, b) = put_target(
        &e,
        &f,
        &tok,
        json!({"selection": "retail", "expected_updated_at": null}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(code(&b), "invalid_body");

    // nothing was recorded
    assert!(ledger_row(&e, &f, "selection").await.is_none());
    assert!(selection_audit(&e, &f).await.is_empty());
    assert_eq!(
        design_value(&e, &f, DesignOf::Draft, "selection").await,
        Some(default_sel())
    );
}

#[tokio::test]
async fn setting_hide_then_revert_drops_row() {
    let (e, f, tok) = setup().await;
    let (s, b) = send(&e.app, get(&uri(&f, "settings"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["override"], json!({}));
    // KH T4 (D108): the stance is the identity instruction, not a tenant setting
    let base = json!({"actor": ACTOR, "stance": STANCE});
    assert_eq!(b["base"], base, "the draft bundle's settings");
    assert_eq!(b["effective"], base, "no override: effective is the base");
    let rev0 = b["draft_rev"].as_i64().expect("draft_rev");

    // hide: an override of null, in the draft only
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &uri(&f, "settings"),
            Some(&tok),
            json!({"stance": null}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["override"], json!({"stance": null}));
    assert_eq!(b["effective"]["stance"], Value::Null);
    assert_eq!(b["base"], base, "a hide leaves the base");
    assert_eq!(b["effective"]["actor"], json!(ACTOR));
    assert!(b["draft_rev"].as_i64().unwrap() > rev0, "{b}");
    let row = ledger_row(&e, &f, "setting:stance")
        .await
        .expect("a hide is a draft change");
    assert_eq!(row.label, "Setting stance hidden");
    assert_eq!(
        (row.kind.as_str(), row.area.as_str(), row.page.as_str()),
        ("setting", "settings", "J13")
    );
    let mut r = DraftRef::new();
    r.insert("key".into(), json!("stance"));
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let live = design::value(
        &mut tx,
        &e.state.design,
        &f.job_ref(),
        &DesignOf::Live,
        "setting",
        &r,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(live, None, "live keeps the release value");

    // revert: the override goes and so does the ledger row (the draft is the live value again)
    let (s, b) = send(
        &e.app,
        req(
            "DELETE",
            &uri(&f, "settings/stance"),
            Some(&tok),
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["override"], json!({}));
    assert_eq!(b["effective"]["stance"], json!(STANCE));
    assert_eq!(b["base"], base);
    assert!(ledger_row(&e, &f, "setting:stance").await.is_none());

    // nothing left to revert
    let (s, b) = send(
        &e.app,
        req(
            "DELETE",
            &uri(&f, "settings/stance"),
            Some(&tok),
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "nothing_to_revert");
    assert_eq!(message(&b), "Setting stance uses the release's value.");

    // a change is stored trimmed, with the hash of the base it overrides
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &uri(&f, "settings"),
            Some(&tok),
            json!({"actor": "  Lumen care  "}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["override"], json!({"actor": "Lumen care"}));
    assert_eq!(b["effective"]["actor"], json!("Lumen care"));
    let row = ledger_row(&e, &f, "setting:actor")
        .await
        .expect("draft row");
    assert_eq!(row.label, "Setting actor changed");
    let stored: (Option<String>, Option<String>, String) = sqlx::query_as(
        "SELECT value, base_hash, updated_by FROM job_setting_override WHERE job_id = $1 AND key = 'actor'",
    )
    .bind(f.job.job_id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(stored.0.as_deref(), Some("Lumen care"));
    let want = bundle_hash(&Value::String(ACTOR.to_string())).unwrap();
    assert_eq!(stored.1.as_deref(), Some(want.as_str()), "the base's hash");
    assert_eq!(b["base"]["actor"], json!(ACTOR));
    assert_eq!(stored.2, ADMIN);

    // refusals: the whole patch is checked before any write
    for (patch, want_code) in [
        (json!({"tone": "x"}), "unknown_setting"),
        (json!({"preamble": "x"}), "unknown_setting"),
        (json!({"stance": "   "}), "invalid_body"),
        (json!({"stance": "x".repeat(2001)}), "invalid_body"),
        (json!({"stance": "calm", "zz": null}), "unknown_setting"),
    ] {
        let (s, b) = send(&e.app, req("PUT", &uri(&f, "settings"), Some(&tok), patch)).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(code(&b), want_code, "{b}");
    }
    assert!(ledger_row(&e, &f, "setting:stance").await.is_none());
}

/// A release row and one open `job_setting` conflict on `key` (owner). Returns the conflict id.
async fn open_setting_conflict(e: &TestEnv, f: &ContractJob, key: &str) -> i64 {
    let sha = |c: char| format!("sha256:{}", c.to_string().repeat(64));
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, snapshot_kinds)
           VALUES ($1, $2, 'cfg-2026-10-20.1', $3, $4, '{"sections": []}', '{}', '[]', '{}', '{}', '[]', 'approved',
               'u-rm', now(), ARRAY['overlay'])"#,
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(sha('a'))
    .bind(sha('b'))
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query_scalar(
        "INSERT INTO release_conflict (tenant_id, job_id, release_id, kind, object, key, tenant_row, detail) \
         VALUES ($1, $2, 'cfg-2026-10-20.1', 'job_setting', 'job_setting', $3, 'override', 'the base moved') \
         RETURNING id",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(key)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

/// Fixture `refineIn(…, 'revert', 'job_setting', key)`: the DELETE decides the setting's open conflicts `reverted`
/// (in the draft; the conflict stays open until a cut) and links them to the ledger row that carries the revert.
#[tokio::test]
async fn setting_revert_decides_open_conflicts() {
    let (e, f, tok) = setup().await;
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &uri(&f, "settings"),
            Some(&tok),
            json!({"actor": "Lumen care"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let id = open_setting_conflict(&e, &f, "actor").await;

    let (s, b) = send(
        &e.app,
        req(
            "DELETE",
            &uri(&f, "settings/actor"),
            Some(&tok),
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["override"], json!({}));
    assert_eq!(b["effective"]["actor"], json!(ACTOR));
    // the draft is the live value again: no ledger row, so the decision links to none
    assert!(ledger_row(&e, &f, "setting:actor").await.is_none());
    let (status, decision): (String, Value) =
        sqlx::query_as("SELECT status, decision FROM release_conflict WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(status, "open", "decided in draft, still open");
    assert_eq!(decision["choice"], json!("reverted"), "{decision}");
    assert_eq!(decision["draft_key"], json!("setting:actor"));
    assert_eq!(decision["draft_id"], Value::Null);
    assert_eq!(decision["by"], json!(ADMIN));

    // an unknown key is refused as the refinement route refuses it
    let (s, b) = send(
        &e.app,
        req("DELETE", &uri(&f, "settings/tone"), Some(&tok), Value::Null),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "unknown_setting");
}

#[tokio::test]
async fn languages_validation_422s() {
    let (e, f, tok) = setup().await;
    let (s, b) = send(&e.app, get(&uri(&f, "languages"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["live"], json!(["en"]));
    assert_eq!(b["draft"], json!(["en"]));
    assert_eq!(b["unpublished"], json!(false));
    // the offering's job has no required knowledge kind, so every language is fully covered (language_level)
    assert_eq!(
        b["options"],
        json!([
            {"key": "nl", "label": "Dutch", "knowledge": "full"},
            {"key": "en", "label": "English", "knowledge": "full"},
            {"key": "fr", "label": "French", "knowledge": "full"},
            {"key": "de", "label": "German", "knowledge": "full"},
        ])
    );

    let put = |langs: Value| {
        req(
            "PUT",
            &uri(&f, "languages"),
            Some(&tok),
            json!({"languages": langs}),
        )
    };
    let (s, b) = send(&e.app, put(json!([]))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_body");
    assert_eq!(message(&b), "A job serves at least one language.");
    let (s, b) = send(&e.app, put(json!(["xx", "de", "yy"]))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "unknown_language");
    assert_eq!(message(&b), "No platform language “xx, yy”.");
    let (s, b) = send(&e.app, put(json!("en"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(code(&b), "invalid_body");
    assert!(ledger_row(&e, &f, "languages").await.is_none());

    // normalised (trimmed, lowercased, deduplicated), saved to the draft only
    let (s, b) = send(&e.app, put(json!([" DE ", "en", "de"]))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["draft"], json!(["de", "en"]));
    assert_eq!(b["live"], json!(["en"]));
    assert_eq!(b["unpublished"], json!(true));
    let row = ledger_row(&e, &f, "languages").await.expect("draft row");
    assert_eq!(row.label, "Languages: + de");
    assert_eq!(
        (row.kind.as_str(), row.area.as_str(), row.page.as_str()),
        ("languages", "settings", "J13")
    );

    let (s, b) = send(&e.app, put(json!(["fr"]))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = ledger_row(&e, &f, "languages").await.expect("draft row");
    assert_eq!(row.label, "Languages: + fr · − en");

    // back to the live value: the draft row goes
    let (s, b) = send(&e.app, put(json!(["en"]))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["unpublished"], json!(false));
    assert!(ledger_row(&e, &f, "languages").await.is_none());
}

#[tokio::test]
async fn tenant_user_403_on_writes() {
    let (e, f, tok) = setup().await;
    insert_user(&e.owner, "u-obs", f.tenant, "tenant_user").await;
    insert_user(&e.owner, "u-jd", f.tenant, "job_designer").await;
    insert_user(&e.owner, "u-rm", f.tenant, "release_manager").await;
    let (obs, jd, rm) = (
        person("u-obs", &[]),
        person("u-jd", &[]),
        person("u-rm", &[]),
    );

    // an observer reads all three
    for tail in ["target", "settings", "languages"] {
        let (s, b) = send(&e.app, get(&uri(&f, tail), Some(&obs))).await;
        assert_eq!(s, StatusCode::OK, "{tail}: {b}");
    }

    let target = json!({"selection": sel(&["chat"]), "expected_updated_at": null});
    let cases = [
        (&obs, "PUT", "target", target.clone(), "release_manager"),
        (&jd, "PUT", "target", target.clone(), "release_manager"),
        (
            &obs,
            "PUT",
            "settings",
            json!({"actor": "x"}),
            "job_designer",
        ),
        (
            &rm,
            "PUT",
            "settings",
            json!({"actor": "x"}),
            "job_designer",
        ),
        (
            &obs,
            "DELETE",
            "settings/actor",
            Value::Null,
            "job_designer",
        ),
        (&rm, "DELETE", "settings/actor", Value::Null, "job_designer"),
        (
            &obs,
            "PUT",
            "languages",
            json!({"languages": ["de"]}),
            "job_designer",
        ),
        (
            &rm,
            "PUT",
            "languages",
            json!({"languages": ["de"]}),
            "job_designer",
        ),
    ];
    for (who, method, tail, body, role) in cases {
        let (s, b) = send(&e.app, req(method, &uri(&f, tail), Some(who), body)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {tail}: {b}");
        assert_eq!(code(&b), "forbidden", "{b}");
        assert_eq!(
            b["error"]["missing_role"],
            json!(role),
            "{method} {tail}: {b}"
        );
    }
    for key in ["selection", "setting:actor", "languages"] {
        assert!(ledger_row(&e, &f, key).await.is_none(), "{key}");
    }

    // each role can make its own writes
    let (s, b) = put_target(&e, &f, &rm, target).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &uri(&f, "settings"),
            Some(&jd),
            json!({"actor": "x"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &uri(&f, "languages"),
            Some(&jd),
            json!({"languages": ["de"]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // the tenant admin holds both roles
    let (s, b) = send(
        &e.app,
        req(
            "DELETE",
            &uri(&f, "settings/actor"),
            Some(&tok),
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}
