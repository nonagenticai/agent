//! B2 T10: release readiness (D67, J3) — `GET /api/jobs/{job}/release-readiness` and `GET …/readiness/board`
//! (crate::readiness, the rules contributor crate::release::readiness_rules).
//!
//! The job is created from a template whose industry layer authors the rule slots RET-01, WAR-01 and PAY-04
//! (`rules_support::template`, with one activity needing routes), all placeholders until a value is saved. Only the rules contributor is
//! registered in production; the providers / routes contributors here are test fakes.
use crate::common;
use crate::rules_support;

use std::sync::Arc;

use agent_api::design::DesignOf;
use agent_api::error::ApiResult;
use agent_api::readiness::{self, Evaluation, ReadOf, Readiness, ReadinessStatus};
use agent_api::release::readiness_rules::RulesReadiness;
use agent_api::release::rules;
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use http_body_util::BodyExt;
use serde_json::{Value, json};

use common::design_contract::ContractJob;
use common::{TestEnv, get, insert_user, json as req, person, send};

const DESIGNER: &str = "u-ready-designer";
const OBSERVER: &str = "u-ready-obs";
const RELEASE: &str = "cfg-2026-10-20.1";

struct Fx {
    e: TestEnv,
    f: ContractJob,
}

async fn setup() -> Fx {
    let e = common::env().await;
    let f = rules_support::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    insert_user(&e.owner, OBSERVER, f.tenant, "tenant_user").await;
    Fx { e, f }
}

fn path(fx: &Fx, rest: &str) -> String {
    format!("/api/jobs/{}{rest}", fx.f.job.job_key)
}

async fn read(fx: &Fx, rest: &str) -> Value {
    read_on(&fx.e.app, fx, rest).await
}

async fn read_on(app: &Router, fx: &Fx, rest: &str) -> Value {
    let (s, b) = send(app, get(&path(fx, rest), Some(&person(OBSERVER, &[])))).await;
    assert_eq!(s, StatusCode::OK, "{rest}: {b}");
    b
}

/// The app with `r` as its readiness registry (`AppState.readiness`), as T11/T12 tests fake the publish gate.
fn app_with(fx: &Fx, r: Readiness) -> Router {
    let mut st = fx.e.state.clone();
    st.readiness = Arc::new(r);
    agent_api::router(st)
}

/// Only the rules contributor: stays true when other plans register theirs in `readiness::registry()`.
fn rules_only() -> Readiness {
    let mut r = Readiness::default();
    r.register(Arc::new(RulesReadiness));
    r
}

/// The rule slots the job's draft selection compiles to (what a cut would store in `bundle.rules`).
async fn draft_rules(fx: &Fx) -> Value {
    let jr = fx.f.job_ref();
    let mut tx = fx.f.job.begin(&fx.e.state.db).await.unwrap();
    let slots = rules::draft_slots(&mut tx, fx.e.state.compiler.as_ref(), &jr)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(
        slots.iter().any(|s| s.id == "RET-01"),
        "the template authors RET-01"
    );
    serde_json::to_value(slots).unwrap()
}

/// An approved cut (owner) of the draft selection whose snapshot covers `rule_values` with no value stored: every
/// rule of it is a placeholder. It requires nothing (`slots`/`routes` empty).
async fn insert_approved(fx: &Fx) {
    insert_approved_with(fx, None).await;
}

/// [`insert_approved`], its snapshot holding `ret01` as RET-01's values (place `rule:RET-01`) when given.
async fn insert_approved_with(fx: &Fx, ret01: Option<Value>) {
    insert_approved_requiring(fx, ret01, &[]).await;
}

/// [`insert_approved_with`], its bundle requiring the Production `routes` (`object.verb{variant}@prod`).
async fn insert_approved_requiring(fx: &Fx, ret01: Option<Value>, routes: &[&str]) {
    let rules = draft_rules(fx).await;
    let selection: Value = sqlx::query_scalar("SELECT selection FROM job_target WHERE job_id = $1")
        .bind(fx.f.job.job_id)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap();
    let sha = |c: char| format!("sha256:{}", c.to_string().repeat(64));
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, snapshot_kinds)
           VALUES ($1, $2, $3, $4, $5, $6, $7, '[]', '{}', $8, '{}', 'approved', 'u-rm', now(),
               ARRAY['rule_values'])"#,
    )
    .bind(fx.f.tenant)
    .bind(fx.f.job.job_id)
    .bind(RELEASE)
    .bind(sha('a'))
    .bind(sha('b'))
    .bind(json!({"rules": rules}))
    .bind(selection)
    .bind(json!({"slots": [], "routes": routes, "test_routes": []}))
    .execute(&fx.e.owner)
    .await
    .unwrap();
    if let Some(values) = ret01 {
        sqlx::query(
            r#"INSERT INTO release_snapshot (tenant_id, job_id, release_id, key, kind, ref, snap)
               VALUES ($1, $2, $3, 'rule:RET-01', 'rule_values', '{"id": "RET-01"}', $4)"#,
        )
        .bind(fx.f.tenant)
        .bind(fx.f.job.job_id)
        .bind(RELEASE)
        .bind(json!({ "v": values }))
        .execute(&fx.e.owner)
        .await
        .unwrap();
    }
}

async fn evaluate(fx: &Fx, r: &Readiness, of: ReadOf) -> Evaluation {
    let jr = fx.f.job_ref();
    let st = &fx.e.state;
    let mut tx = fx.f.job.begin(&st.db).await.unwrap();
    let checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, of)
        .await
        .unwrap();
    let ev = r
        .evaluate(&mut tx, &st.design, st.compiler.as_ref(), &checked)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    ev
}

fn subjects(items: &Value, code: &str) -> Vec<String> {
    items
        .as_array()
        .expect("items")
        .iter()
        .filter(|i| i["code"] == code)
        .map(|i| i["subject"].as_str().unwrap().to_string())
        .collect()
}

// ---- tests ----------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn no_release_verdict_no_release() {
    let fx = setup().await;
    let b = read(&fx, "/readiness/board").await;
    assert_eq!(b["verdict"], "no_release", "{b}");
    assert!(b["release"].is_null(), "{b}");
    assert!(b["current_id"].is_null(), "{b}");
    assert_eq!(b["job"], fx.f.job.job_key.as_str());
    let keys: Vec<&str> = b["areas"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["key"].as_str().unwrap())
        .collect();
    assert_eq!(
        keys,
        [
            "providers",
            "routes",
            "tests",
            "knowledge",
            "rules",
            "gates",
            "procedures",
            "toolsets"
        ]
    );
    let go_live = b["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "go_live")
        .expect("go_live step")
        .clone();
    assert_eq!(go_live["status"], "todo", "{go_live}");
    assert_eq!(
        go_live["detail"],
        "No release yet — approve one on the Release page"
    );

    let r = read(&fx, "/release-readiness").await;
    assert!(r["release_id"].is_null(), "{r}");
    // R5 / D110: never released, Production (the baseline, taken at creation, before the fixture routes and
    // knowledge bindings) routes nothing: the template's activity needs routes (release/derive.rs derives them
    // from its operations), so routing is not ready, and routing is all that blocks (the draft's run, see the
    // publish gate of `of=draft`).
    assert_eq!(r["routing_ready"], false, "{r}");
    let blocking = r["blocking"].as_array().expect("blocking items");
    assert!(!blocking.is_empty(), "{r}");
    assert!(
        blocking
            .iter()
            .all(|i| i["code"] == "route_not_ready" || i["code"] == "knowledge_unbound"),
        "{r}"
    );
    assert!(
        subjects(&r["blocking"], "route_not_ready").contains(&"case.read@prod".to_string()),
        "{r}"
    );
    assert_eq!(r["job"], fx.f.job.job_key.as_str());

    for rest in ["/release-readiness?of=live", "/readiness/board?of=x"] {
        let (s, b) = send(
            &fx.e.app,
            get(&path(&fx, rest), Some(&person(OBSERVER, &[]))),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{rest}: {b}");
        assert_eq!(b["error"]["code"], "invalid_of", "{b}");
    }
}

#[tokio::test]
async fn rules_placeholder_is_go_live_warning_not_blocking() {
    let fx = setup().await;
    insert_approved(&fx).await;
    let r = read(&fx, "/release-readiness").await;
    assert_eq!(r["release_id"], RELEASE, "{r}");
    assert_eq!(r["blocking"], json!([]), "{r}");
    assert_eq!(r["routing_ready"], true);
    let ph = subjects(&r["warnings"], "rule_placeholder");
    assert!(ph.contains(&"RET-01".to_string()), "{r}");

    let b = read(&fx, "/readiness/board").await;
    assert_eq!(b["verdict"], "go_live_gaps", "{b}");
    assert_eq!(b["release"]["id"], RELEASE);
    assert_eq!(b["release"]["state"], "approved");
    assert_eq!(b["release"]["approved_by_name"], "u-rm");
    let gap = b["gaps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["key"] == "rule:RET-01")
        .expect("rule:RET-01 gap")
        .clone();
    assert_eq!(gap["blocks"], "go_live", "{gap}");
    assert_eq!(gap["area"], "rules");
    assert_eq!(gap["subject"], "RET-01");
    assert_eq!(gap["enforced_from"], "M4");
    assert_eq!(gap["fix_label"], "Set RET-01 values");
    assert_eq!(
        gap["href"],
        format!("/admin/jobs/{}/rules", fx.f.job.job_key)
    );
    assert!(
        b["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|g| g["blocks"] != "apply"),
        "{b}"
    );
    let rules = b["areas"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == "rules")
        .unwrap()
        .clone();
    assert_eq!(rules["status"], "warn", "{rules}");
    assert_eq!(rules["ready"], 0);
    let steps = b["steps"].as_array().unwrap();
    let step = |k: &str| steps.iter().find(|s| s["key"] == k).unwrap().clone();
    assert_eq!(step("rules")["status"], "placeholder");
    assert_eq!(step("go_live")["status"], "current");
    assert_eq!(
        step("go_live")["detail"],
        format!("{RELEASE} is approved and ready to apply")
    );

    // the publish gate passes: a placeholder never blocks (rules alone, whatever else production registers)
    let reg = rules_only();
    let jr = fx.f.job_ref();
    let st = &fx.e.state;
    let mut tx = fx.f.job.begin(&st.db).await.unwrap();
    let checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, ReadOf::Release)
        .await
        .unwrap();
    let cx = checked.cx(&st.design, st.compiler.as_ref());
    reg.gate(&mut tx, &cx, Some(RELEASE)).await.unwrap();
    let assessed = reg.assess(&mut tx, &cx).await.unwrap();
    assert!(assessed.missing.is_empty(), "{:?}", assessed.missing);
    assert!(
        assessed.go_live.iter().any(|g| g.subject == "RET-01"),
        "the placeholder is a go-live gap: {:?}",
        assessed.go_live
    );
    let items = reg
        .draft_blocking(&mut tx, &st.design, st.compiler.as_ref(), &jr)
        .await
        .unwrap();
    assert!(items.is_empty(), "{items:?}");
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn area_without_contributor_is_todo() {
    let fx = setup().await;
    let app = app_with(&fx, rules_only());
    let b = read_on(&app, &fx, "/readiness/board").await;
    for a in b["areas"].as_array().unwrap() {
        if a["key"] == "rules" {
            assert_eq!(a["status"], "warn", "{a}");
            continue;
        }
        assert_eq!(a["status"], "todo", "{a}");
        assert_eq!(a["summary"], "Not checked yet", "{a}");
        assert_eq!(a["ready"], 0, "{a}");
        assert_eq!(a["total"], 0, "{a}");
    }
    let steps = b["steps"].as_array().unwrap();
    let keys: Vec<&str> = steps.iter().map(|s| s["key"].as_str().unwrap()).collect();
    assert_eq!(
        keys,
        [
            "template",
            "operations",
            "systems",
            "instances",
            "routes",
            "mapping",
            "rules",
            "tests",
            "go_live",
            "operate"
        ]
    );
    for s in steps {
        match s["key"].as_str().unwrap() {
            "template" => assert_eq!(s["status"], "done", "{s}"),
            "rules" => assert_eq!(s["status"], "placeholder", "{s}"),
            "go_live" | "operate" => assert_eq!(s["status"], "todo", "{s}"),
            _ => {
                assert_eq!(s["status"], "todo", "{s}");
                assert_eq!(s["detail"], "Not checked yet", "{s}");
            }
        }
    }

    let ev = evaluate(&fx, &rules_only(), ReadOf::Draft).await;
    let dor: Vec<(&str, ReadinessStatus)> = ev
        .definition_of_ready
        .iter()
        .map(|s| (s.key.as_str(), s.status))
        .collect();
    assert_eq!(
        dor,
        [
            ("dor_slots", ReadinessStatus::Todo),
            ("dor_routes", ReadinessStatus::Todo),
            ("dor_tests", ReadinessStatus::Todo),
            ("dor_rules", ReadinessStatus::Placeholder),
            ("dor_gates", ReadinessStatus::Todo),
            ("dor_knowledge", ReadinessStatus::Todo),
        ]
    );
}

#[tokio::test]
async fn of_release_reads_release_snapshot_not_draft() {
    let fx = setup().await;
    insert_approved(&fx).await;
    let (s, b) = send(
        &fx.e.app,
        req(
            "PUT",
            &path(&fx, "/rules/RET-01"),
            Some(&person(DESIGNER, &[])),
            json!({"values": {"window_days": "30", "excluded_categories": "made-to-measure rugs"}}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["placeholder"], false, "{b}");

    let release = read(&fx, "/release-readiness?of=release").await;
    assert!(
        subjects(&release["warnings"], "rule_placeholder").contains(&"RET-01".to_string()),
        "the release's snapshot still has no RET-01 value: {release}"
    );
    let draft = read(&fx, "/release-readiness?of=draft").await;
    assert_eq!(draft["release_id"], RELEASE, "{draft}");
    assert!(
        !subjects(&draft["warnings"], "rule_placeholder").contains(&"RET-01".to_string()),
        "the draft has RET-01's values: {draft}"
    );
    let board = read(&fx, "/readiness/board?of=draft").await;
    assert!(
        board["gaps"]
            .as_array()
            .unwrap()
            .iter()
            .all(|g| g["key"] != "rule:RET-01"),
        "{board}"
    );
}

#[tokio::test]
async fn of_release_reads_release_snapshot_not_live() {
    let fx = setup().await;
    insert_approved_with(
        &fx,
        Some(json!({"window_days": "30", "excluded_categories": "made-to-measure rugs"})),
    )
    .await;
    let ret01 =
        |v: &Value| subjects(&v["warnings"], "rule_placeholder").contains(&"RET-01".to_string());

    let release = read(&fx, "/release-readiness?of=release").await;
    assert_eq!(release["release_id"], RELEASE, "{release}");
    assert!(
        !ret01(&release),
        "the release's snapshot holds RET-01's values: {release}"
    );
    let draft = read(&fx, "/release-readiness?of=draft").await;
    assert!(ret01(&draft), "the draft has no RET-01 values: {draft}");

    // the same checked release read as Live (the baseline / working rows) still has the placeholder
    let jr = fx.f.job_ref();
    let st = &fx.e.state;
    let mut tx = fx.f.job.begin(&st.db).await.unwrap();
    let mut checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, ReadOf::Release)
        .await
        .unwrap();
    assert_eq!(checked.design, DesignOf::Release(RELEASE.to_string()));
    checked.design = DesignOf::Live;
    let live = rules_only()
        .release_readiness(&mut tx, &st.design, st.compiler.as_ref(), &checked)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let live = serde_json::to_value(&live).unwrap();
    assert!(ret01(&live), "live has no RET-01 values: {live}");
}

#[tokio::test]
async fn checked_release_reads_the_named_release() {
    let fx = setup().await;
    insert_approved(&fx).await;
    let jr = fx.f.job_ref();
    let st = &fx.e.state;
    let mut tx = fx.f.job.begin(&st.db).await.unwrap();
    let checked = readiness::checked_release(&mut tx, &jr, RELEASE)
        .await
        .unwrap();
    assert_eq!(checked.of, ReadOf::Release);
    assert_eq!(checked.design, DesignOf::Release(RELEASE.to_string()));
    let rel = checked.release.as_ref().expect("the release");
    assert_eq!(rel.id, RELEASE);
    assert_eq!(rel.state, "approved");
    assert!(checked.selection.is_some());
    assert!(checked.rules.is_none());
    assert!(checked.requires.slots.is_empty());
    let err = readiness::checked_release(&mut tx, &jr, "cfg-2026-10-20.9")
        .await
        .unwrap_err();
    tx.commit().await.unwrap();
    assert_eq!(
        err.status_and_code(),
        (StatusCode::NOT_FOUND, "release_not_found")
    );
}

#[test]
#[should_panic(expected = "a second readiness contributor for area `rules`")]
fn second_contributor_same_area_panics() {
    let mut r = Readiness::default();
    r.register(Arc::new(RulesReadiness));
    r.register(Arc::new(RulesReadiness));
}

// ---- R5: routing is a hard publish blocker ---------------------------------------------------------------------------

/// The publish gate of the approved release, with only the production routes contributor registered.
async fn routes_gate(fx: &Fx) -> ApiResult<()> {
    let mut r = Readiness::default();
    agent_api::connectors::readiness::register(&mut r);
    let jr = fx.f.job_ref();
    let st = &fx.e.state;
    let mut tx = fx.f.job.begin(&st.db).await.unwrap();
    let checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, ReadOf::Release)
        .await
        .unwrap();
    let cx = checked.cx(&st.design, st.compiler.as_ref());
    let out = r.gate(&mut tx, &cx, Some(RELEASE)).await;
    drop(tx);
    out
}

/// The job has no route row at all (the contract job's fixture routes removed).
async fn drop_routes(fx: &Fx) {
    sqlx::query("DELETE FROM operation_route WHERE job_id = $1")
        .bind(fx.f.job.job_id)
        .execute(&fx.e.owner)
        .await
        .unwrap();
}

#[tokio::test]
async fn unrouted_operation_blocks_publish_even_without_a_route_table() {
    let fx = setup().await;
    drop_routes(&fx).await;
    insert_approved_requiring(&fx, None, &["case.read@prod"]).await;
    let err = routes_gate(&fx).await.unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (StatusCode::CONFLICT, "release_not_ready")
    );
    let body: Value = serde_json::from_slice(
        &err.into_response()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    assert_eq!(
        body["error"]["message"],
        format!("{RELEASE} is not ready: routing.case.read@prod."),
        "{body}"
    );
    assert_eq!(
        body["error"]["blocking"][0]["code"], "route_not_ready",
        "{body}"
    );
    assert_eq!(
        body["error"]["blocking"][0]["subject"], "case.read@prod",
        "{body}"
    );
}

#[tokio::test]
async fn tool_free_job_publishes_without_routes() {
    let fx = setup().await;
    drop_routes(&fx).await;
    insert_approved(&fx).await;
    routes_gate(&fx)
        .await
        .expect("a job that uses no operation needs no route");
}
