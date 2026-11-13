//! B5 T8a — per-job gate on/off (F-17) and customer reply text (F-16): `setGateEnabled`, `setGateCustomerReply`
//! (`routes::job_design::gates`), the `gate_setting` design target (`job_design::targets::gate_settings`, 0146) and
//! the fold (`job_design::gates::library`): a disabled question is a draft (Test skips it, Production still asks it
//! until a release), a safety question is always on, the customer text is setting ?? the question's own ?? the
//! care-bus default. Real Postgres 18, through `agent_app`.
use agent_api::design::DesignOf;
use agent_api::job_design::gates::library::{
    self, DEFAULT_BLOCK_REPLY, DEFAULT_HANDOVER_REPLY, DISABLED_REASON,
};
use agent_api::job_design::gates::stats::Stats;
use agent_api::job_design::readiness::{GATES_UNREVIEWED, GatesReadiness};
use agent_api::job_design::types::{EffGateQuestion, GateDecision};
use agent_api::platform::templates;
use agent_api::readiness::{self, AreaKey, AreaStatus, ReadOf, ReadinessContributor};
use agent_api::release::compile::job_key;
use agent_api::runtime::design::RuntimeDesign;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::b5::gates::{
    Fx, by_key, call, ledger, ok, plain_noul, questions, refused, safety_gate, setup,
    tenant_question,
};
use crate::common::{self, design_contract, send};

/// The effective questions of design `of` (the fold the runtime reads, T12).
async fn design(x: &Fx, of: DesignOf) -> Vec<EffGateQuestion> {
    let mut c = x.e.owner.acquire().await.unwrap();
    let (bases, rows) = library::of_design(
        &mut c,
        &x.e.state.design,
        x.e.state.compiler.as_ref(),
        &x.f.job_ref(),
        &of,
    )
    .await
    .unwrap();
    library::effective(&bases, &rows, &Stats::default())
}

fn find<'a>(qs: &'a [EffGateQuestion], key: &str) -> &'a EffGateQuestion {
    qs.iter().find(|q| q.key == key).expect("the question")
}

async fn settings_rows(x: &Fx) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM job_gate_question_setting WHERE job_id = $1")
        .bind(x.f.job.job_id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn disable_question_is_a_draft_and_not_asked_in_test() {
    let x = setup().await;
    let gate = plain_noul(&x).await;
    let b = ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/enabled"),
        json!({"enabled": false}),
    )
    .await);
    assert_eq!(b["enabled"], json!(false));
    assert_eq!(by_key(&questions(&x).await, &gate)["enabled"], json!(false));
    assert_eq!(
        ledger(&x).await,
        [(
            format!("gate-setting:{gate}"),
            "gate_setting".to_string(),
            "J7".to_string(),
            format!("{gate} · turned off")
        )]
    );
    // Test (draft) skips it; Production (live) still asks it
    let draft = design(&x, DesignOf::Draft).await;
    assert_eq!(
        library::skip_reason(find(&draft, &gate)),
        Some(DISABLED_REASON)
    );
    let live = design(&x, DesignOf::Live).await;
    assert!(find(&live, &gate).enabled);
    assert_eq!(library::skip_reason(find(&live, &gate)), None);
}

#[tokio::test]
async fn disable_safety_question_409_safety_gate() {
    let x = setup().await;
    let gate = safety_gate(&x).await;
    refused(
        call(
            &x,
            "PUT",
            &format!("/gates/{gate}/enabled"),
            json!({"enabled": false}),
        )
        .await,
        StatusCode::CONFLICT,
        "safety_gate",
    );
    refused(
        call(&x, "PUT", "/gates/ZZ9/enabled", json!({"enabled": false})).await,
        StatusCode::NOT_FOUND,
        "gate_not_found",
    );
    // a safety question still takes its own customer text
    let b = ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/customer-reply"),
        json!({"text": "A colleague will call you right away."}),
    )
    .await);
    assert_eq!(b["enabled"], json!(true));
}

#[tokio::test]
async fn reenable_to_base_drops_ledger_row() {
    let x = setup().await;
    let gate = plain_noul(&x).await;
    let rest = format!("/gates/{gate}/enabled");
    ok(call(&x, "PUT", &rest, json!({"enabled": false})).await);
    assert_eq!(settings_rows(&x).await, 1);
    let b = ok(call(&x, "PUT", &rest, json!({"enabled": true})).await);
    assert_eq!(b["enabled"], json!(true));
    assert_eq!(
        settings_rows(&x).await,
        0,
        "both fields at the base: no row"
    );
    assert!(ledger(&x).await.is_empty(), "back to live: no ledger row");
}

#[tokio::test]
async fn customer_reply_overrides_library_default() {
    let x = setup().await;
    let gate = plain_noul(&x).await;
    let q = by_key(&questions(&x).await, &gate).clone();
    // the seed's library rows carry no `customer_reply` field
    assert_eq!(q["customer_reply_default"], Value::Null);
    assert_eq!(q["customer_reply"], Value::Null);
    let b = ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/customer-reply"),
        json!({"text": "  We will get back to you.  "}),
    )
    .await);
    assert_eq!(b["customer_reply"], json!("We will get back to you."));
    assert_eq!(b["customer_reply_default"], Value::Null);
    assert_eq!(
        ledger(&x).await[0].3,
        format!("{gate} · customer reply set")
    );
    // a tenant question's own text is its default; the setting overrides it
    let mut tq = tenant_question("TQ1");
    tq["customer_reply"] = json!("Own text.");
    let b = ok(call(&x, "POST", "/gates", tq).await);
    assert_eq!(b["customer_reply"], json!("Own text."));
    assert_eq!(b["customer_reply_default"], json!("Own text."));
    let b = ok(call(
        &x,
        "PUT",
        "/gates/TQ1/customer-reply",
        json!({"text": "Setting text."}),
    )
    .await);
    assert_eq!(b["customer_reply"], json!("Setting text."));
    assert_eq!(b["customer_reply_default"], json!("Own text."));
    // saving the question's own text back is no setting
    ok(call(
        &x,
        "PUT",
        "/gates/TQ1/customer-reply",
        json!({"text": "Own text."}),
    )
    .await);
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM job_gate_question_setting WHERE job_id = $1 AND gate = 'TQ1'",
    )
    .bind(x.f.job.job_id)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn null_reply_reverts_to_default() {
    let x = setup().await;
    let gate = plain_noul(&x).await;
    let rest = format!("/gates/{gate}/customer-reply");
    ok(call(&x, "PUT", &rest, json!({"text": "Hold on."})).await);
    let b = ok(call(&x, "PUT", &rest, json!({"text": null})).await);
    assert_eq!(b["customer_reply"], Value::Null);
    assert_eq!(settings_rows(&x).await, 0);
    assert!(ledger(&x).await.is_empty());
    // the setting keeps the other field: off + reply, then reply back → still off
    ok(call(&x, "PUT", &rest, json!({"text": "Hold on."})).await);
    ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/enabled"),
        json!({"enabled": false}),
    )
    .await);
    let b = ok(call(&x, "PUT", &rest, json!({"text": null})).await);
    assert_eq!(b["enabled"], json!(false));
    assert_eq!(settings_rows(&x).await, 1);
}

#[tokio::test]
async fn reply_over_500_chars_422_invalid_reply() {
    let x = setup().await;
    let gate = plain_noul(&x).await;
    let rest = format!("/gates/{gate}/customer-reply");
    refused(
        call(&x, "PUT", &rest, json!({"text": "x".repeat(501)})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_reply",
    );
    refused(
        call(&x, "PUT", &rest, json!({"text": "   "})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_reply",
    );
    refused(
        call(&x, "PUT", &rest, json!({"text": "ok", "more": 1})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_body",
    );
    // 500 characters (not bytes) is fine
    ok(call(&x, "PUT", &rest, json!({"text": "é".repeat(500)})).await);
    let mut tq = tenant_question("TQ2");
    tq["customer_reply"] = json!("y".repeat(501));
    refused(
        call(&x, "POST", "/gates", tq).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_reply",
    );
}

#[tokio::test]
async fn handover_uses_question_customer_reply_then_default() {
    let x = setup().await;
    let gate = plain_noul(&x).await;
    let mut tq = tenant_question("TQ1");
    tq["customer_reply"] = json!("Own text.");
    ok(call(&x, "POST", "/gates", tq).await);
    ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/customer-reply"),
        json!({"text": "Setting text."}),
    )
    .await);
    let qs = design(&x, DesignOf::Draft).await;
    let text = |k: &str, d| library::customer_text(find(&qs, k), d);
    assert_eq!(
        text(&gate, GateDecision::Handover).as_deref(),
        Some("Setting text.")
    );
    assert_eq!(
        text("TQ1", GateDecision::Block).as_deref(),
        Some("Own text.")
    );
    let other = qs
        .iter()
        .find(|q| q.key != gate && q.key != "TQ1")
        .unwrap()
        .key
        .clone();
    assert_eq!(
        text(&other, GateDecision::Handover).as_deref(),
        Some(DEFAULT_HANDOVER_REPLY)
    );
    assert_eq!(
        text(&other, GateDecision::Block).as_deref(),
        Some(DEFAULT_BLOCK_REPLY)
    );
    assert_eq!(text(&other, GateDecision::Allow), None);
}

#[tokio::test]
async fn gate_setting_key_matches_fixture_ledger() {
    let e = common::env().await;
    let t = e.state.design.target("gate_setting").unwrap();
    let fixture: Value =
        serde_json::from_str(include_str!("../fixtures/design-kinds.json")).unwrap();
    let entry = fixture
        .as_array()
        .unwrap()
        .iter()
        .find(|k| k["kind"] == json!("gate_setting"))
        .expect("the fixture has gate_setting");
    assert_eq!(entry["scope"], json!("job"));
    assert_eq!(entry["projection"], json!(false));
    assert!(!t.is_projection());
    for s in entry["samples"].as_array().unwrap() {
        let r = s["ref"].as_object().unwrap().clone();
        assert_eq!(json!(t.key(&r)), s["key"]);
    }
}

#[tokio::test]
async fn gate_setting_full_contract() {
    let e = common::env().await;
    let r = |v: Value| v.as_object().cloned().unwrap();
    design_contract::run_all(
        &e,
        "gate_setting",
        &[
            (
                r(json!({"gate": "GC"})),
                json!({"enabled": false, "customer_reply": null}),
            ),
            (
                r(json!({"gate": "TQ9"})),
                json!({"enabled": null, "customer_reply": "We will call you."}),
            ),
        ],
        Some("as the library has it"),
    )
    .await;
}

// ---- reviewGates: the tenant accepts the template's gate questions as they are -----------------------------------

/// The gates area as the draft readiness reads it now: (status, `gates_unreviewed` gaps).
async fn gates_area(x: &Fx) -> (AreaStatus, usize) {
    let st = &x.e.state;
    let jr = x.f.job_ref();
    let mut tx = x.f.job.begin(&st.db).await.unwrap();
    let checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, ReadOf::Draft)
        .await
        .unwrap();
    let cx = checked.cx(&st.design, st.compiler.as_ref());
    let a = GatesReadiness.assess(&mut tx, &cx).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(a.area.key, AreaKey::Gates);
    let gaps = a
        .go_live
        .iter()
        .filter(|g| g.cause.as_deref() == Some(GATES_UNREVIEWED))
        .count();
    (a.area.status, gaps)
}

/// A fingerprint of the draft's pinned compile: the effective-config hash plus the compiled gate catalogue (what
/// gate compilation produces; the `_reviewed` marker must change neither).
async fn compiled_bundle_hash(x: &Fx) -> String {
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let d = RuntimeDesign::load(&mut tx, &x.e.state.design, &x.f.job_ref(), &DesignOf::Draft)
        .await
        .unwrap();
    let compiled = templates::compile_pinned(&mut tx, &d.pins, &d.selection, job_key(&d.pins))
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("the template compiles: {}", e.len()));
    tx.commit().await.unwrap();
    format!(
        "{}|{}",
        compiled.effective.hash,
        serde_json::to_string(&compiled.catalogue.gates).unwrap()
    )
}

async fn review(x: &Fx) -> (StatusCode, Value) {
    call(x, "POST", "/gates/review", json!({})).await
}

#[tokio::test]
async fn review_clears_gates_unreviewed() {
    let x = setup().await;
    let (status, gaps) = gates_area(&x).await;
    assert_eq!(
        (status, gaps),
        (AreaStatus::Warn, 1),
        "a fresh template job"
    );
    let got = review(&x).await;
    assert_eq!(got.0, StatusCode::NO_CONTENT, "{}", got.1);
    let (status, gaps) = gates_area(&x).await;
    assert_eq!((status, gaps), (AreaStatus::Ok, 0));
    // the marker is a setting row at the reserved key, written by the designer
    let (gate, by): (String, String) =
        sqlx::query_as("SELECT gate, updated_by FROM job_gate_question_setting WHERE job_id = $1")
            .bind(x.f.job.job_id)
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(gate, "_reviewed");
    assert!(!by.is_empty());
    // idempotent: a second review is still one row, still reviewed
    assert_eq!(review(&x).await.0, StatusCode::NO_CONTENT);
    assert_eq!(settings_rows(&x).await, 1);
    assert_eq!(gates_area(&x).await, (AreaStatus::Ok, 0));
}

#[tokio::test]
async fn review_leaves_the_compiled_bundle_and_questions_unchanged() {
    let x = setup().await;
    let before_hash = compiled_bundle_hash(&x).await;
    let before = questions(&x).await;
    assert_eq!(review(&x).await.0, StatusCode::NO_CONTENT);
    assert_eq!(compiled_bundle_hash(&x).await, before_hash);
    assert_eq!(questions(&x).await, before, "no question changed");
    // the runtime fold ignores the marker too
    let qs = design(&x, DesignOf::Draft).await;
    assert!(qs.iter().all(|q| q.key != "_reviewed"));
}

#[tokio::test]
async fn review_then_a_real_refinement_stays_reviewed() {
    let x = setup().await;
    assert_eq!(review(&x).await.0, StatusCode::NO_CONTENT);
    let gate = plain_noul(&x).await;
    ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/enabled"),
        json!({"enabled": false}),
    )
    .await);
    assert_eq!(gates_area(&x).await, (AreaStatus::Ok, 0));
    // turning it back on removes that question's own row; the marker stays
    ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/enabled"),
        json!({"enabled": true}),
    )
    .await);
    assert_eq!(settings_rows(&x).await, 1);
    assert_eq!(gates_area(&x).await, (AreaStatus::Ok, 0));
}

async fn draft_change_audits(x: &Fx) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'job.draft_change' AND job = $1",
    )
    .bind(&x.f.job.job_key)
    .fetch_one(&x.e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn review_needs_job_designer_and_is_audited() {
    let x = setup().await;
    let (s, _) = send(
        &x.e.app,
        common::json(
            "POST",
            &format!("/api/jobs/{}/gates/review", x.f.job.job_key),
            Some(&x.observer),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(
        settings_rows(&x).await,
        0,
        "a refused review writes nothing"
    );
    let before = draft_change_audits(&x).await;
    assert_eq!(review(&x).await.0, StatusCode::NO_CONTENT);
    assert_eq!(
        draft_change_audits(&x).await,
        before + 1,
        "one draft_change audit row"
    );
    let l = ledger(&x).await;
    assert!(
        l.iter()
            .any(|(k, kind, page, _)| k == "gate-setting:_reviewed"
                && kind == "gate_setting"
                && page == "J7"),
        "{l:?}"
    );
}
