//! B8 T13 — draft validation (J17 "Test the draft"): scenarios compiled from the `use_case_card` rows of the job's
//! pinned stack (B8-4), probed on Production and on the draft through B4's `probe_turn`, judged by the job's `gates`
//! Laya. Real Postgres 18, the real router. The tenant is the platform tenant (D70), so `qualify` and `gates` resolve to
//! the internal endpoints, here ONE fake: `/v1/chat/completions` (a fixed order-status qualification) and
//! `/v1/systemone` (the judge). The judge's answer follows the scenario's `expected` text, per call number (the run
//! judges the live reply first, then the draft's): `REGRESS` yes then no, `FIXED` no then yes, `ALWAYS_FAIL` no, else yes.
use crate::common::platform;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_api::audit::Actor;
use agent_api::platform::db::{rows, tags};
use agent_api::platform::hooks::NoHooks;
use agent_api::platform::templates::TemplatePin;
use agent_api::platform_db::begin_platform;
use agent_model::kinds::{Kind, Op};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob, DESIGNER};
use crate::common::{self, TestEnv, get, insert_user, person, send};

const TOKEN: &str = "laya-test-token";
const QUALIFIED: &str =
    r#"{"context_space": "retail:order-support:order_status", "slots": {"order_id": "RET-01"}}"#;
const PLACE: &str = "setting:actor";

#[derive(Default)]
struct Judge {
    calls: Mutex<HashMap<String, usize>>,
}

async fn chat() -> axum::Json<Value> {
    axum::Json(json!({
        "choices": [{"message": {"role": "assistant", "content": QUALIFIED}}],
        "usage": {"prompt_tokens": 9, "completion_tokens": 5},
    }))
}

async fn systemone(
    axum::extract::State(j): axum::extract::State<Arc<Judge>>,
    axum::Json(body): axum::Json<Value>,
) -> axum::Json<Value> {
    let expected = body["state"]["expected"].as_str().unwrap_or("").to_owned();
    let n = {
        let mut m = j.calls.lock().unwrap();
        let c = m.entry(expected.clone()).or_default();
        *c += 1;
        *c - 1
    };
    let yes = if expected.contains("ALWAYS_FAIL") {
        false
    } else if expected.contains("REGRESS") {
        n % 2 == 0
    } else if expected.contains("FIXED") {
        n % 2 == 1
    } else {
        true
    };
    // the judge asks `same_answer`; the job's gate points (P1) ask their library questions of the same endpoint, and
    // a reply that leaves one unanswered fails the point (the closed ones hand the turn over). Answer each as
    // "nothing fires": a noul false, a choice's first option, a score of the lowest level.
    let mut answers = serde_json::Map::new();
    for (key, q) in body["questions"].as_object().into_iter().flatten() {
        let a = match (key.as_str(), q["type"].as_str()) {
            ("same_answer", _) => {
                json!({"choice": if yes { "yes" } else { "no" }, "confidence": 0.9})
            }
            (_, Some("noul")) => json!({"noul": false}),
            (_, Some("score")) => json!({"score": 0, "confidence": 1.0}),
            _ => {
                let first = q["criteria"]
                    .as_object()
                    .and_then(|c| c.keys().next().cloned())
                    .unwrap_or_default();
                json!({"choice": first, "confidence": 1.0})
            }
        };
        answers.insert(key.clone(), a);
    }
    axum::Json(json!({
        "model": "jev-test",
        "answers": answers,
        "usage": {"input_tokens": 11, "output_tokens": 3},
    }))
}

async fn fake() -> String {
    let app = axum::Router::new()
        .route("/v1/chat/completions", axum::routing::post(chat))
        .route("/v1/systemone", axum::routing::post(systemone))
        .with_state(Arc::new(Judge::default()));
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    format!("http://{addr}")
}

struct Fx {
    e: TestEnv,
    f: ContractJob,
    designer: String,
    observer: String,
    /// Activities of the job's journey, in order.
    activities: Vec<(String, String)>,
}

async fn setup() -> Fx {
    let base = fake().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| {
        v.insert("AGENT_PLATFORM_TENANT_ID".into(), tenant.to_string());
        v.insert("INTERNAL_LLM_URL".into(), format!("{base}/v1"));
        v.insert("INTERNAL_LAYA_URL".into(), base.clone());
        v.insert("INTERNAL_LAYA_KEY".into(), TOKEN.into());
    })
    .await;
    let t = crate::b8::support::content(&e).await;
    let f = design_contract::new_job_for_on(&e, tenant, t).await;
    crate::b8::support::route_content_ops(&e, &f).await;
    insert_user(&e.owner, DESIGNER, tenant, "job_designer").await;
    insert_user(&e.owner, "u-dv-observer", tenant, "").await;
    // the stub instance answers `order.read` for the qualified order-status request
    platform::stub_shop(&e, tenant, agent_api::runtime::routes::DEMO_STUB_INSTANCE).await;
    // `stub_shop` writes no route rows: the probes run in the Test env, where `order.read` goes to that instance
    sqlx::query(
        "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, instance_id, \
                                      updated_by) \
         VALUES ($1, $2, 'order', 'read', '*', 'test', 'instance', $3, 'test') \
         ON CONFLICT (tenant_id, job_id, object, verb, variant, env) \
         DO UPDATE SET target_kind = 'instance', instance_id = EXCLUDED.instance_id",
    )
    .bind(tenant)
    .bind(f.job.job_id)
    .bind(agent_api::runtime::routes::DEMO_STUB_INSTANCE)
    .execute(&e.owner)
    .await
    .unwrap();
    let mut x = Fx {
        e,
        f,
        designer: person(DESIGNER, &[]),
        observer: person("u-dv-observer", &[]),
        activities: Vec::new(),
    };
    let (s, b) = send(&x.e.app, get(&uri(&x, "journey"), Some(&x.designer))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    x.activities = b["activities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["key"].as_str().unwrap().to_owned(),
                a["label"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert!(x.activities.len() >= 3, "{:?}", x.activities);
    x
}

fn uri(x: &Fx, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", x.f.job.job_key)
}

async fn call(x: &Fx, method: &str, tail: &str, tok: &str, body: Value) -> (StatusCode, Value) {
    send(
        &x.e.app,
        common::json(method, &uri(x, tail), Some(tok), body),
    )
    .await
}

async fn draft(x: &Fx) -> Value {
    let (s, b) = send(
        &x.e.app,
        get(&uri(x, "validation/draft"), Some(&x.observer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

async fn run(x: &Fx) -> (StatusCode, Value) {
    call(x, "POST", "validation/draft/run", &x.designer, json!({})).await
}

fn scenario(id: &str, expected: &str, place: Option<&str>) -> Value {
    let mut s = json!({
        "id": id, "style": "plain", "phrasing": "Where is my order RET-01?", "expected": expected,
        "cites": [],
    });
    if let Some(p) = place {
        s["place"] = json!(p);
    }
    s
}

/// Give the activity `key` a `use_case_card` with `scenarios` in the job's pinned department layer: a new tag of
/// the family (tags are immutable) that the job is re-pinned to.
async fn add_cards(x: &Fx, cards: &[(&str, Vec<Value>)]) {
    if cards.is_empty() {
        // no seeded cards exist any more: nothing to clear, nothing to tag (a tag with no new row is refused)
        return;
    }
    let pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1")
        .bind(x.f.job.job_id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    let mut pins: Vec<TemplatePin> = serde_json::from_value(pins).unwrap();
    let dept = pins
        .iter()
        .find(|p| p.layer.starts_with("industry_dept/"))
        .expect("the job pins a department layer");
    let family = dept.layer.clone();
    let actor = Actor::system("service:agent-api-b8-t13-test");
    let mut tx = begin_platform(&x.e.state.db).await.unwrap();
    // seed v8 ships Demo Retail's use_case_cards (3 scenarios each) in this family: the fixture's cards REPLACE
    // them, so a card the test does not name is tombstoned and the draft holds exactly the scenarios given
    let seeded: Vec<(String, String)> = rows::family_head_rows(&mut tx, &family)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.kind == Kind::UseCaseCard.as_str() && r.op != Op::Tombstone.as_str())
        .map(|r| (r.layer_key, r.key))
        .collect();
    for (layer, key) in seeded {
        if cards.iter().any(|(k, _)| *k == key) {
            continue;
        }
        rows::write_revision(
            &mut tx,
            &actor,
            &layer,
            Kind::UseCaseCard,
            &key,
            Op::Tombstone,
            json!({}),
            "b8-t13-author",
        )
        .await
        .unwrap_or_else(|e| panic!("clear card {key}: {e:?}"));
    }
    for (key, scenarios) in cards {
        rows::write_revision(
            &mut tx,
            &actor,
            &family,
            Kind::UseCaseCard,
            key,
            Op::Add,
            json!({"summary": "a card", "scenarios": scenarios}),
            "b8-t13-author",
        )
        .await
        .unwrap_or_else(|e| panic!("card {key}: {e:?}"));
    }
    let tag = tags::tag_family(
        &mut tx,
        &actor,
        &family,
        "t13 cards",
        "b8-t13-author",
        &NoHooks,
        false,
    )
    .await
    .expect("tag the family");
    tx.commit().await.unwrap();
    // re-pin to the new tag's own stack (`stack_rule`: base and every base_dept head under the family, D116), which
    // the compile requires of a template cut after the base departments; pins the new stack does not name are kept.
    // The family is no `job_template_tag` (the job family specialises the department), so the cut is read from the
    // tag itself: its stack plus the family at the new version
    let mut cut: Vec<TemplatePin> = serde_json::from_value(tag.stack.clone()).unwrap();
    cut.push(TemplatePin {
        layer: family.clone(),
        version: tag.version,
    });
    pins.retain(|p| !cut.iter().any(|n| n.layer == p.layer));
    pins.extend(cut);
    sqlx::query("UPDATE jobs SET pins = $2 WHERE id = $1")
        .bind(x.f.job.job_id)
        .bind(serde_json::to_value(&pins).unwrap())
        .execute(&x.e.owner)
        .await
        .unwrap();
}

async fn usage_count(x: &Fx, use_: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM usage_event WHERE job_id = $1 AND source = 'validation' AND use = $2",
    )
    .bind(x.f.job.job_id)
    .bind(use_)
    .fetch_one(&x.e.owner)
    .await
    .unwrap()
}

fn ids(v: &Value) -> Vec<String> {
    v["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["id"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn scenarios_come_from_cards() {
    let x = setup().await;
    let a = x.activities[0].0.clone();
    // no card is authored yet: the draft holds no scenarios
    assert!(draft(&x).await["scenarios"].as_array().unwrap().is_empty());
    add_cards(
        &x,
        &[(
            &a,
            vec![
                scenario("s1", "gives the order status", Some(PLACE)),
                scenario("s2", "gives the order status again", None),
            ],
        )],
    )
    .await;
    let v = draft(&x).await;
    assert_eq!(ids(&v), [format!("{a}/s1"), format!("{a}/s2")]);
    let s = &v["scenarios"][0];
    assert_eq!(s["activity"], a.as_str());
    assert_eq!(s["activity_label"], x.activities[0].1.as_str());
    assert_eq!(s["place"], PLACE);
    assert_eq!(s["phrasing"], "Where is my order RET-01?");
    assert_eq!(v["scenarios"][1]["place"], Value::Null);
    assert_eq!(v["state"], "never");
    assert_eq!(v["runs"], json!([]));
    assert_eq!(v["impacts"], json!([]));
    assert_eq!(v["current"]["changes"], 0);
    // an observer reads; running needs job_designer
    let (st, b) = call(&x, "POST", "validation/draft/run", &x.observer, json!({})).await;
    assert_eq!(st, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "job_designer", "{b}");
    // no card, no run
    let y = setup().await;
    add_cards(&y, &[]).await;
    let (st, b) = run(&y).await;
    assert_eq!(st, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "no_scenarios", "{b}");
}

#[tokio::test]
async fn tenant_card_override_changes_scenarios() {
    let x = setup().await;
    let (a, b) = (x.activities[0].0.clone(), x.activities[1].0.clone());
    add_cards(
        &x,
        &[
            (
                &a,
                vec![scenario("s1", "ok", None), scenario("s2", "ok", None)],
            ),
            (&b, vec![scenario("s1", "ok", None)]),
        ],
    )
    .await;
    assert_eq!(ids(&draft(&x).await).len(), 3);
    // relabelling an activity is what its scenarios are listed under
    let (st, body) = call(
        &x,
        "PUT",
        &format!("refinements/journey/labels/{b}"),
        &x.designer,
        json!({"label": "Renamed by the tenant"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let v = draft(&x).await;
    let sb = v["scenarios"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["activity"] == b.as_str())
        .unwrap();
    assert_eq!(sb["activity_label"], "Renamed by the tenant");
    // hiding an activity takes its scenarios out of the test
    let mut hidden = None;
    for key in [&a, &b] {
        let (st, body) = call(
            &x,
            "POST",
            "refinements/journey/hide",
            &x.designer,
            json!({"key": key}),
        )
        .await;
        if st == StatusCode::OK {
            hidden = Some(key.clone());
            break;
        }
        assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    }
    let hidden = hidden.expect("one of the two activities can be hidden");
    let left = ids(&draft(&x).await);
    assert!(
        left.iter().all(|i| !i.starts_with(&format!("{hidden}/"))),
        "{left:?}"
    );
    assert!(left.len() < 3, "{left:?}");
}

#[tokio::test]
async fn run_probes_each_scenario_and_meters() {
    let x = setup().await;
    // the order-status activity: its space's procedure answers "Your order is …" through the fake model (v8)
    let a = x
        .activities
        .iter()
        .map(|(k, _)| k.clone())
        .find(|k| k == "delivery:where_is_my_order")
        .unwrap_or_else(|| x.activities[0].0.clone());
    add_cards(
        &x,
        &[(
            &a,
            vec![
                scenario("s1", "gives the status", None),
                scenario("s2", "gives it again", None),
            ],
        )],
    )
    .await;
    let rel = design_contract::publish(&x.e, &x.f).await;
    let (st, v) = run(&x).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["state"], "current", "{v}");
    let r = &v["runs"][0];
    assert_eq!(r["id"], "tr-1");
    assert_eq!(r["env"], "test");
    assert_eq!(r["base_release"], rel.as_str());
    assert_eq!(r["results"].as_array().unwrap().len(), 2);
    assert_eq!(r["summary"]["total"], 2);
    assert_eq!(r["summary"]["pass"], 2);
    assert_eq!(r["summary"]["regressions"], 0);
    let res = &r["results"][0];
    assert_eq!(
        (res["live"].as_str(), res["draft"].as_str()),
        (Some("pass"), Some("pass"))
    );
    assert!(
        res["reply_draft"]
            .as_str()
            .unwrap()
            .starts_with("Your order is "),
        "{res}"
    );
    // 2 scenarios x (live + draft): four probes (one qualify call each) and four judge calls, all metered
    assert_eq!(usage_count(&x, "judge").await, 4);
    assert_eq!(usage_count(&x, "qualify").await, 4);
    let audit: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = 'validation.draft_run'",
    )
    .bind(x.f.tenant)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(audit, 1);
    // a draft change after the run makes the run stale and names the change
    design_contract::tracked_set(
        &x.e,
        &x.f,
        "setting",
        &[("key".to_string(), json!("actor"))].into_iter().collect(),
        Some(json!({"value": "Lumen care", "base_hash": "sha256:b"})),
    )
    .await;
    let v = draft(&x).await;
    assert_eq!(v["state"], "stale", "{v}");
    assert_eq!(v["changed_since"].as_array().unwrap().len(), 1, "{v}");
    assert_eq!(v["impacts"][0]["untested"], true);
    // and a second run is tr-2
    let (st, v) = run(&x).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["runs"][0]["id"], "tr-2");
    assert_eq!(v["state"], "current");
}

#[tokio::test]
async fn failing_scenario_names_its_place() {
    let x = setup().await;
    let a = x.activities[0].0.clone();
    add_cards(
        &x,
        &[(
            &a,
            vec![
                scenario("s1", "REGRESS the status", Some(PLACE)),
                scenario("s2", "FIXED the status", Some(PLACE)),
                scenario("s3", "ALWAYS_FAIL the status", Some(PLACE)),
            ],
        )],
    )
    .await;
    design_contract::publish(&x.e, &x.f).await;
    design_contract::tracked_set(
        &x.e,
        &x.f,
        "setting",
        &[("key".to_string(), json!("actor"))].into_iter().collect(),
        Some(json!({"value": "Lumen care", "base_hash": "sha256:b"})),
    )
    .await;
    let (st, v) = run(&x).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let r = &v["runs"][0];
    let by = |id: &str| {
        r["results"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["scenario"] == format!("{a}/{id}"))
            .unwrap()
            .clone()
    };
    let (s1, s2, s3) = (by("s1"), by("s2"), by("s3"));
    assert_eq!(s1["diff"], "regression");
    assert_eq!(s1["cause"]["key"], PLACE, "{s1}");
    assert_eq!(s2["diff"], "fixed");
    assert_eq!(s2["cause"]["key"], PLACE, "{s2}");
    assert_eq!(
        (s3["diff"].as_str(), s3["draft"].as_str()),
        (Some("same"), Some("fail"))
    );
    assert_eq!(s3["cause"], Value::Null);
    let sum = &r["summary"];
    assert_eq!(
        (
            sum["pass"].as_u64(),
            sum["fail"].as_u64(),
            sum["regressions"].as_u64(),
            sum["fixed"].as_u64(),
            sum["live_pass"].as_u64()
        ),
        (Some(1), Some(2), Some(1), Some(1), Some(1))
    );
    // the change row says what the run found at its place
    let i = &v["impacts"][0];
    assert_eq!(i["key"], PLACE);
    assert_eq!(
        (
            i["covered"].as_u64(),
            i["fixed"].as_u64(),
            i["regressions"].as_u64()
        ),
        (Some(3), Some(1), Some(1))
    );
    assert_eq!(i["untested"], false);
}

#[tokio::test]
async fn other_tenant_404() {
    let x = setup().await;
    let a = x.activities[0].0.clone();
    add_cards(&x, &[(&a, vec![scenario("s1", "ok", None)])]).await;
    design_contract::publish(&x.e, &x.f).await;
    let (st, v) = run(&x).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // another tenant's member, on the same job key: its own job, so none of this tenant's scenarios or runs
    let g = design_contract::new_job(&x.e).await;
    insert_user(&x.e.owner, "u-dv-other", g.tenant, "job_designer").await;
    let other = person("u-dv-other", &[]);
    let path = |tail: &str| format!("/api/jobs/{}/{tail}", g.job.job_key);
    let (st, b) = send(&x.e.app, get(&path("validation/draft"), Some(&other))).await;
    assert_eq!(st, StatusCode::OK, "{b}");
    // its own job pins the seeded cards, never this tenant's replaced one ("ok")
    assert!(b["runs"].as_array().unwrap().is_empty(), "{b}");
    assert!(
        b["scenarios"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s["expected"] != "ok"),
        "{b}"
    );
    // a job that is not the member's tenant's at all
    let (st, b) = send(
        &x.e.app,
        get("/api/jobs/no_such_job/validation/draft", Some(&other)),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "{b}");
    let mut tx = agent_api::tenancy::begin_scoped(&x.e.state.db, g.tenant)
        .await
        .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM draft_test_run")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 0);
}
