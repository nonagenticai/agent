//! B5 T8 — the J7 gate routes (`routes::job_design::gates`): `jobGates` (library at the draft's pins + tenant questions,
//! effective; 7-day stats from the gate log; changes and the release they ship in), `refineGateOption` (library
//! option → B5 `gate` row; GENERATED option → B2 `overlay` row), `setGateThreshold`, `setGateRule`,
//! `addGateQuestion`, `deleteGateQuestion` and their refusals (review focus 4 and 6). Real Postgres 18, a Demo Retail
//! job created through `POST /api/jobs`, through `agent_app`. Gates are picked from the job's own `jobGates` answer by
//! shape (safety, noul, choice, score, generated), so the tests follow the seed rather than hard-code it.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::common::design_contract::{self, ContractJob, DESIGNER};
use crate::common::{self, TestEnv, get, insert_user, person, send};

const OBSERVER: &str = "u-gates-observer";

pub struct Fx {
    pub e: TestEnv,
    pub f: ContractJob,
    pub designer: String,
    pub observer: String,
}

pub async fn setup() -> Fx {
    let e = common::env().await;
    let f = crate::b5::support::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    insert_user(&e.owner, OBSERVER, f.tenant, "").await;
    Fx {
        e,
        f,
        designer: person(DESIGNER, &[]),
        observer: person(OBSERVER, &[]),
    }
}

fn uri(x: &Fx, rest: &str) -> String {
    format!("/api/jobs/{}{rest}", x.f.job.job_key)
}

pub async fn call(x: &Fx, method: &str, rest: &str, body: Value) -> (StatusCode, Value) {
    send(
        &x.e.app,
        common::json(method, &uri(x, rest), Some(&x.designer), body),
    )
    .await
}

pub async fn job_gates(x: &Fx) -> Value {
    let (s, b) = send(&x.e.app, get(&uri(x, "/gates"), Some(&x.observer))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

pub async fn questions(x: &Fx) -> Vec<Value> {
    job_gates(x).await["questions"]
        .as_array()
        .expect("JobGates.questions")
        .clone()
}

pub fn by_key<'a>(qs: &'a [Value], key: &str) -> &'a Value {
    qs.iter()
        .find(|q| q["key"] == json!(key))
        .unwrap_or_else(|| panic!("no question {key}"))
}

/// The first question matching `pred` (its key).
pub async fn pick(x: &Fx, what: &str, pred: impl Fn(&Value) -> bool) -> String {
    questions(x)
        .await
        .into_iter()
        .find(|q| pred(q))
        .unwrap_or_else(|| panic!("Demo Retail has a {what} question"))["key"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn options(q: &Value) -> Vec<Value> {
    q["options"].as_array().cloned().unwrap_or_default()
}

fn all_options(q: &Value, generated: bool) -> bool {
    let o = options(q);
    !o.is_empty() && o.iter().all(|o| o["generated"] == json!(generated))
}

pub async fn safety_gate(x: &Fx) -> String {
    pick(x, "safety", |q| q["safety"] == json!(true)).await
}

/// A noul question that is not a safety question.
pub async fn plain_noul(x: &Fx) -> String {
    pick(x, "plain noul", |q| {
        q["type"] == json!("noul")
            && q["safety"] == json!(false)
            && q["origin"] == json!("platform")
    })
    .await
}

/// A choice question with a gte rule, a tunable threshold and library options only.
async fn library_choice(x: &Fx) -> String {
    pick(x, "library choice (gte)", |q| {
        q["type"] == json!("choice")
            && q["rule"]["fire_when"] == json!("gte")
            && !q["threshold_value"].is_null()
            && all_options(q, false)
            && options(q).len() >= 3
    })
    .await
}

/// A choice question whose options are all generated (at least 3); not B6, whose options are the job's languages.
async fn generated_choice(x: &Fx) -> String {
    pick(x, "generated choice", |q| {
        q["type"] == json!("choice")
            && q["key"] != json!("B6")
            && all_options(q, true)
            && options(q).len() >= 3
    })
    .await
}

#[track_caller]
pub fn refused(got: (StatusCode, Value), status: StatusCode, code: &str) {
    assert_eq!(got.0, status, "{}", got.1);
    assert_eq!(got.1["error"]["code"], json!(code), "{}", got.1);
}

#[track_caller]
pub fn ok(got: (StatusCode, Value)) -> Value {
    assert!(got.0.is_success(), "{}: {}", got.0, got.1);
    got.1
}

/// The job's ledger rows as (key, kind, page, label).
pub async fn ledger(x: &Fx) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT key, kind, page, label FROM draft_ledger WHERE job_id = $1 ORDER BY at, id",
    )
    .bind(x.f.job.job_id)
    .fetch_all(&x.e.owner)
    .await
    .unwrap()
}

pub fn tenant_question(key: &str) -> Value {
    json!({
        "key": key,
        "point": "P1",
        "question": "Does the customer mention a competitor?",
        "criteria": {"true": "A competitor is named", "false": "No competitor is named"},
        "rule": {"fire_when": "gte", "threshold": 0.5, "fire_values": null, "min_confidence": null},
        "action": "annotate",
        "fail": "open",
    })
}

async fn count(x: &Fx, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(x.f.job.job_id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap()
}

// ---- jobGates -------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn job_gates_lists_library_and_tenant_questions_effective() {
    let x = setup().await;
    let g = job_gates(&x).await;
    let qs = g["questions"].as_array().unwrap().clone();
    assert!(!qs.is_empty());
    assert!(
        qs.iter().all(|q| q["origin"] == json!("platform")),
        "{qs:?}"
    );
    assert!(qs.iter().any(|q| q["safety"] == json!(true)));
    assert!(qs.iter().all(|q| q["enabled"] == json!(true)));
    assert_eq!(g["window_from"], Value::Null, "no traffic yet");
    assert_eq!(g["changes"], json!([]));
    assert_eq!(g["releases"]["live"], Value::Null);
    // library = catalogue.gates at the draft's pins, in its order
    let mut c = x.e.owner.acquire().await.unwrap();
    let bases = agent_api::job_design::base::at(
        &mut c,
        &x.e.state.design,
        x.e.state.compiler.as_ref(),
        &x.f.job_ref(),
        &agent_api::design::DesignOf::Draft,
    )
    .await
    .unwrap();
    let lib: Vec<&str> = bases.gates.iter().map(|g| g.key.as_str()).collect();
    let got: Vec<&str> = qs.iter().map(|q| q["key"].as_str().unwrap()).collect();
    assert_eq!(got, lib);

    let b = ok(call(&x, "POST", "/gates", tenant_question("TQ1")).await);
    assert_eq!(b["origin"], json!("tenant"));
    let qs = questions(&x).await;
    let t = by_key(&qs, "TQ1");
    assert_eq!(t["group"], json!("tenant"));
    assert_eq!(t["layer"], json!("tenant"));
    assert_eq!(t["type"], json!("noul"));
    assert_eq!(t["updated_by"], json!(DESIGNER));
    assert_eq!(qs.len(), lib.len() + 1);
}

#[tokio::test]
async fn stats_window_and_counts_from_gate_answer() {
    let x = setup().await;
    let gate = library_choice(&x).await;
    let qs = questions(&x).await;
    let first = options(by_key(&qs, &gate))[0]["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let (t, j) = (x.f.tenant, x.f.job.job_id);
    for (outcome, latency, error, value, confidence, unsure, age_days) in [
        ("pass", 100, None, Some(first.as_str()), 0.92, false, 0),
        ("failed_closed", 300, Some("timeout"), None, 0.32, true, 1),
        ("pass", 50, None, Some(first.as_str()), 0.92, false, 8),
    ] {
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO gate_evaluation (tenant_id, job_id, at, point, env, conversation_id, outcome, decision, \
                                          latency_ms, error) \
             VALUES ($1, $2, now() - make_interval(days => $3), 'P1', 'prod', 'conv-1', $4, 'allow', $5, $6) \
             RETURNING id",
        )
        .bind(t)
        .bind(j)
        .bind(age_days)
        .bind(outcome)
        .bind(latency)
        .bind(error)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO gate_answer (evaluation_id, tenant_id, job_id, at, gate, answer, value, measure, \
                                      confidence, fired, unsure) \
             VALUES ($1, $2, $3, now() - make_interval(days => $4), $5, $6, $6, $7, $7, false, $8)",
        )
        .bind(id)
        .bind(t)
        .bind(j)
        .bind(age_days)
        .bind(&gate)
        .bind(value)
        .bind(confidence)
        .bind(unsure)
        .execute(&x.e.owner)
        .await
        .unwrap();
    }
    let g = job_gates(&x).await;
    assert!(g["window_from"].is_string(), "{g}");
    let q = by_key(g["questions"].as_array().unwrap(), &gate).clone();
    let s = &q["stats"];
    assert_eq!(
        s["verdicts_7d"],
        json!(2),
        "the 8-day-old answer is outside: {s}"
    );
    assert_eq!(s["fail_closed"], json!(1));
    assert_eq!(s["fail_open"], json!(0));
    assert_eq!(s["below_threshold"], json!(1));
    assert_eq!(s["timeouts"], json!(1));
    assert_eq!(s["latency_p50_ms"], json!(100));
    assert_eq!(s["latency_p95_ms"], json!(300));
    let buckets = s["confidence"].as_array().expect("choice buckets");
    assert_eq!(buckets.len(), 20);
    assert_eq!(buckets[6], json!(1));
    assert_eq!(buckets[18], json!(1));
    assert_eq!(options(&q)[0]["verdicts"], json!(1));
    // another question has no traffic
    let other = g["questions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|q| q["key"] != json!(gate))
        .unwrap();
    assert_eq!(other["stats"]["verdicts_7d"], json!(0));
    assert!(other["stats"].get("confidence").is_none());
}

#[tokio::test]
async fn changes_ship_live_approved_next_cut() {
    let x = setup().await;
    let gate = library_choice(&x).await;
    let gen_gate = generated_choice(&x).await;
    let qs = questions(&x).await;
    let opt = options(by_key(&qs, &gate))[0]["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let gen_opt = options(by_key(&qs, &gen_gate))[0]["key"]
        .as_str()
        .unwrap()
        .to_owned();
    ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/options/{opt}"),
        json!({"label": "Renamed"}),
    )
    .await);
    ok(call(
        &x,
        "PUT",
        &format!("/gates/{gen_gate}/options/{gen_opt}"),
        json!({"label": "Generated renamed"}),
    )
    .await);
    let ch = job_gates(&x).await["changes"].clone();
    let find = |ch: &Value, g: &str, field: &str| -> Value {
        ch.as_array()
            .unwrap()
            .iter()
            .find(|c| c["gate"] == json!(g) && c["field"] == json!(field))
            .cloned()
            .unwrap_or_else(|| panic!("no {field} change of {g} in {ch}"))
    };
    let c = find(&ch, &gate, "label");
    assert_eq!(c["ships"], json!("next_cut"));
    assert_eq!(c["release_id"], Value::Null);
    assert_eq!(c["to"], json!("Renamed"));
    assert_eq!(c["by"], json!(DESIGNER));
    assert_eq!(find(&ch, &gen_gate, "label")["ships"], json!("next_cut"));

    // approved and applied: live
    let live = crate::b5::support::publish(&x.e, &x.f).await;
    let g = job_gates(&x).await;
    assert_eq!(g["releases"]["live"]["id"], json!(live));
    let c = find(&g["changes"], &gate, "label");
    assert_eq!(c["ships"], json!("live"));
    assert_eq!(c["release_id"], json!(live));

    // a new change, cut but not applied: approved
    ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/threshold"),
        json!({"value": 0.9}),
    )
    .await);
    assert_eq!(
        find(&job_gates(&x).await["changes"], &gate, "threshold")["ships"],
        json!("next_cut")
    );
    let tok = design_contract::release_manager(&x.e, &x.f).await;
    let approved = design_contract::cut(&x.e, &x.f, &tok, false).await;
    let g = job_gates(&x).await;
    assert_eq!(g["releases"]["approved"]["id"], json!(approved));
    let c = find(&g["changes"], &gate, "threshold");
    assert_eq!(c["ships"], json!("approved"));
    assert_eq!(c["release_id"], json!(approved));
    assert_eq!(find(&g["changes"], &gate, "label")["ships"], json!("live"));
}

// ---- refineGateOption -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn hide_safety_option_409_safety_gate() {
    let x = setup().await;
    let gate = safety_gate(&x).await;
    refused(
        call(
            &x,
            "PUT",
            &format!("/gates/{gate}/options/true"),
            json!({"hidden": true}),
        )
        .await,
        StatusCode::CONFLICT,
        "safety_gate",
    );
    // relabelling a safety option is allowed
    ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/options/true"),
        json!({"label": "yes"}),
    )
    .await);
}

#[tokio::test]
async fn hide_to_one_option_is_409() {
    let x = setup().await;
    // a two-option question: hiding either leaves one
    let noul = plain_noul(&x).await;
    refused(
        call(
            &x,
            "PUT",
            &format!("/gates/{noul}/options/true"),
            json!({"hidden": true}),
        )
        .await,
        StatusCode::CONFLICT,
        "too_few_options",
    );
    // generated options: B2 tombstones count as hidden
    let gen_gate = generated_choice(&x).await;
    let keys: Vec<String> = options(by_key(&questions(&x).await, &gen_gate))
        .iter()
        .map(|o| o["key"].as_str().unwrap().to_owned())
        .collect();
    for k in &keys[..keys.len() - 2] {
        let b = ok(call(
            &x,
            "PUT",
            &format!("/gates/{gen_gate}/options/{k}"),
            json!({"hidden": true}),
        )
        .await);
        assert!(
            options(&b)
                .iter()
                .any(|o| o["key"] == json!(k) && o["hidden"] == json!(true)),
            "{b}"
        );
    }
    let tombstones = count(
        &x,
        "SELECT count(*) FROM job_overlay_row WHERE job_id = $1 AND object = 'gate_option' AND kind = 'tombstone'",
    )
    .await;
    assert_eq!(tombstones, (keys.len() - 2) as i64);
    let last = &keys[keys.len() - 2];
    refused(
        call(
            &x,
            "PUT",
            &format!("/gates/{gen_gate}/options/{last}"),
            json!({"hidden": true}),
        )
        .await,
        StatusCode::CONFLICT,
        "too_few_options",
    );
    // a library choice question: B5 hides count the same way
    let gate = library_choice(&x).await;
    let keys: Vec<String> = options(by_key(&questions(&x).await, &gate))
        .iter()
        .map(|o| o["key"].as_str().unwrap().to_owned())
        .collect();
    for k in &keys[..keys.len() - 2] {
        ok(call(
            &x,
            "PUT",
            &format!("/gates/{gate}/options/{k}"),
            json!({"hidden": true}),
        )
        .await);
    }
    refused(
        call(
            &x,
            "PUT",
            &format!("/gates/{gate}/options/{}", keys[keys.len() - 1]),
            json!({"hidden": true}),
        )
        .await,
        StatusCode::CONFLICT,
        "too_few_options",
    );
}

#[tokio::test]
async fn refine_unknown_gate_or_option_404() {
    let x = setup().await;
    refused(
        call(&x, "PUT", "/gates/ZZ9/options/a", json!({"hidden": false})).await,
        StatusCode::NOT_FOUND,
        "gate_not_found",
    );
    let gate = library_choice(&x).await;
    refused(
        call(
            &x,
            "PUT",
            &format!("/gates/{gate}/options/zz_none"),
            json!({"label": "x"}),
        )
        .await,
        StatusCode::NOT_FOUND,
        "option_not_found",
    );
    // an observer cannot write
    let (s, _) = send(
        &x.e.app,
        common::json(
            "PUT",
            &uri(&x, &format!("/gates/{gate}/threshold")),
            Some(&x.observer),
            json!({"value": null}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn generated_option_refinement_is_overlay_row() {
    let x = setup().await;
    let gate = generated_choice(&x).await;
    let opt = options(by_key(&questions(&x).await, &gate))[0]["key"]
        .as_str()
        .unwrap()
        .to_owned();
    let b = ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/options/{opt}"),
        json!({"label": "Renamed"}),
    )
    .await);
    let o = options(&b)
        .into_iter()
        .find(|o| o["key"] == json!(opt))
        .unwrap();
    assert_eq!(o["label"], json!("Renamed"));
    assert_eq!(o["refined"], json!(true));
    assert_eq!(o["generated"], json!(true));
    assert_eq!(
        count(
            &x,
            "SELECT count(*) FROM job_gate_option_refinement WHERE job_id = $1"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &x,
            "SELECT count(*) FROM job_overlay_row WHERE job_id = $1 AND object = 'gate_option' AND kind = 'override'"
        )
        .await,
        1
    );
    let l = ledger(&x).await;
    assert_eq!(l.len(), 1, "{l:?}");
    assert_eq!(l[0].1, "overlay");
    assert_eq!(l[0].2, "J7");
}

#[tokio::test]
async fn relabel_to_base_label_clears_refinement() {
    let x = setup().await;
    let gate = library_choice(&x).await;
    let o = options(by_key(&questions(&x).await, &gate))[0].clone();
    let (opt, base) = (
        o["key"].as_str().unwrap().to_owned(),
        o["label"].as_str().unwrap().to_owned(),
    );
    let rest = format!("/gates/{gate}/options/{opt}");
    let b = ok(call(&x, "PUT", &rest, json!({"label": "  Renamed  "})).await);
    let o = options(&b)
        .into_iter()
        .find(|o| o["key"] == json!(opt))
        .unwrap();
    assert_eq!(o["label"], json!("Renamed"));
    assert_eq!(o["base_label"], json!(base));
    let l = ledger(&x).await;
    assert_eq!(
        l,
        [(
            format!("gate:{gate}:{opt}"),
            "gate".to_string(),
            "J7".to_string(),
            format!("{gate} · “{base}” relabelled “Renamed”")
        )]
    );
    let b = ok(call(&x, "PUT", &rest, json!({"label": base})).await);
    let o = options(&b)
        .into_iter()
        .find(|o| o["key"] == json!(opt))
        .unwrap();
    assert_eq!(o["refined"], json!(false));
    assert_eq!(
        count(
            &x,
            "SELECT count(*) FROM job_gate_option_refinement WHERE job_id = $1"
        )
        .await,
        0
    );
    assert!(ledger(&x).await.is_empty(), "back to live: no ledger row");
}

// ---- threshold / rule -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn threshold_on_noul_409_threshold_not_tunable() {
    let x = setup().await;
    let noul = plain_noul(&x).await;
    refused(
        call(
            &x,
            "PUT",
            &format!("/gates/{noul}/threshold"),
            json!({"value": 0.7}),
        )
        .await,
        StatusCode::CONFLICT,
        "threshold_not_tunable",
    );
    let safety = safety_gate(&x).await;
    refused(
        call(
            &x,
            "PUT",
            &format!("/gates/{safety}/threshold"),
            json!({"value": 0.7}),
        )
        .await,
        StatusCode::CONFLICT,
        "threshold_not_tunable",
    );
}

#[tokio::test]
async fn score_threshold_out_of_range_422() {
    let x = setup().await;
    let score = pick(&x, "score", |q| {
        q["type"] == json!("score") && !q["threshold_value"].is_null()
    })
    .await;
    let n = options(by_key(&questions(&x).await, &score)).len();
    for bad in [json!(0), json!(n + 1), json!(2.5)] {
        refused(
            call(
                &x,
                "PUT",
                &format!("/gates/{score}/threshold"),
                json!({"value": bad}),
            )
            .await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_threshold",
        );
    }
    let b = ok(call(
        &x,
        "PUT",
        &format!("/gates/{score}/threshold"),
        json!({"value": 2}),
    )
    .await);
    assert_eq!(b["threshold_value"], json!(2.0));
    assert_eq!(b["threshold_refined"], json!(true));
    assert_eq!(b["rule"]["threshold"], json!(2.0));
    let l = ledger(&x).await;
    assert_eq!(l[0].0, format!("gate:{score}:threshold"));
    assert_eq!(l[0].3, format!("{score} · threshold → 2"));
    // back to the template value: no row, no ledger row
    let base = b["base_threshold_value"].clone();
    let b = ok(call(
        &x,
        "PUT",
        &format!("/gates/{score}/threshold"),
        json!({"value": base}),
    )
    .await);
    assert_eq!(b["threshold_refined"], json!(false));
    assert!(ledger(&x).await.is_empty());
}

#[tokio::test]
async fn rule_on_safety_409() {
    let x = setup().await;
    let safety = safety_gate(&x).await;
    refused(
        call(
            &x,
            "PUT",
            &format!("/gates/{safety}/rule"),
            json!({"rule": {"fire_when": "gte", "threshold": 0.9, "fire_values": null, "min_confidence": null}}),
        )
        .await,
        StatusCode::CONFLICT,
        "safety_gate",
    );
}

#[tokio::test]
async fn rule_in_on_noul_422_invalid_rule() {
    let x = setup().await;
    let noul = plain_noul(&x).await;
    refused(
        call(
            &x,
            "PUT",
            &format!("/gates/{noul}/rule"),
            json!({"rule": {"fire_when": "in", "threshold": null, "fire_values": ["true"], "min_confidence": null}}),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_rule",
    );
    refused(
        call(
            &x,
            "PUT",
            &format!("/gates/{noul}/rule"),
            json!({"rule": {"fire_when": "gte", "threshold": 1.5, "fire_values": null, "min_confidence": null}}),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_rule",
    );
    let b = ok(call(
        &x,
        "PUT",
        &format!("/gates/{noul}/rule"),
        json!({"rule": {"fire_when": "gte", "threshold": 0.8, "fire_values": null, "min_confidence": 0.6}}),
    )
    .await);
    assert_eq!(b["rule_refined"], json!(true));
    assert_eq!(b["rule"]["threshold"], json!(0.8));
}

#[tokio::test]
async fn threshold_then_rule_later_wins() {
    let x = setup().await;
    let gate = library_choice(&x).await;
    let b = ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/threshold"),
        json!({"value": 0.85}),
    )
    .await);
    assert_eq!(b["threshold_refined"], json!(true));
    assert_eq!(b["rule_refined"], json!(false));
    assert_eq!(b["rule"]["threshold"], json!(0.85));
    let b = ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/rule"),
        json!({"rule": {"fire_when": "lt", "threshold": 0.3, "fire_values": ["x"], "min_confidence": null}}),
    )
    .await);
    assert_eq!(
        b["threshold_refined"],
        json!(false),
        "the rule cleared the threshold"
    );
    assert_eq!(b["rule_refined"], json!(true));
    assert_eq!(b["rule"]["fire_when"], json!("lt"));
    assert_eq!(b["rule"]["fire_values"], Value::Null, "normalised");
    let (t, r): (Option<f64>, Option<Value>) = sqlx::query_as(
        "SELECT threshold_value::float8, rule FROM job_gate_rule_refinement WHERE job_id = $1 AND gate = $2",
    )
    .bind(x.f.job.job_id)
    .bind(&gate)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(t, None);
    assert_eq!(r.unwrap()["fire_when"], json!("lt"));
    // the threshold again: it clears the rule
    let b = ok(call(
        &x,
        "PUT",
        &format!("/gates/{gate}/threshold"),
        json!({"value": 0.75}),
    )
    .await);
    assert_eq!(b["rule_refined"], json!(false));
    assert_eq!(b["threshold_refined"], json!(true));
    let l = ledger(&x).await;
    assert_eq!(l.len(), 1, "one place: {l:?}");
    assert_eq!(l[0].3, format!("{gate} · threshold → 0.75"));
}

// ---- tenant questions -----------------------------------------------------------------------------------------------

#[tokio::test]
async fn add_question_noul_only_with_criteria() {
    let x = setup().await;
    let mut blank = tenant_question("TQ1");
    blank["criteria"]["false"] = json!("  ");
    refused(
        call(&x, "POST", "/gates", blank).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_criteria",
    );
    let mut short = tenant_question("TQ1");
    short["question"] = json!("Why?");
    refused(
        call(&x, "POST", "/gates", short).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_question",
    );
    refused(
        call(&x, "POST", "/gates", tenant_question("1X")).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_key",
    );
    let mut bad_rule = tenant_question("TQ1");
    bad_rule["rule"] = json!({"fire_when": "in", "threshold": null, "fire_values": ["true"], "min_confidence": null});
    refused(
        call(&x, "POST", "/gates", bad_rule).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_rule",
    );
    let mut extra = tenant_question("TQ1");
    extra["type"] = json!("choice");
    refused(
        call(&x, "POST", "/gates", extra).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_body",
    );
    let (s, b) = call(&x, "POST", "/gates", tenant_question("tq1")).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["key"], json!("TQ1"), "keys are upper-cased");
    assert_eq!(b["type"], json!("noul"));
    assert_eq!(b["threshold"], json!("any true"));
    assert_eq!(b["threshold_value"], Value::Null);
    assert_eq!(b["effect"], json!("Fires → annotate."));
    assert_eq!(b["tools"], Value::Null, "tools only at P2");
    let o = options(&b);
    assert_eq!(o.len(), 2);
    assert_eq!(o[0]["key"], json!("true"));
    assert_eq!(o[0]["description"], json!("A competitor is named"));
    assert_eq!(o[1]["description"], json!("No competitor is named"));
    assert_eq!(
        ledger(&x).await,
        [(
            "gate-question:TQ1".to_string(),
            "gate_question".to_string(),
            "J7".to_string(),
            "TQ1 · question added".to_string()
        )]
    );
}

#[tokio::test]
async fn add_existing_key_409_gate_exists() {
    let x = setup().await;
    let lib = safety_gate(&x).await;
    refused(
        call(&x, "POST", "/gates", tenant_question(&lib.to_lowercase())).await,
        StatusCode::CONFLICT,
        "gate_exists",
    );
    ok(call(&x, "POST", "/gates", tenant_question("TQ1")).await);
    refused(
        call(&x, "POST", "/gates", tenant_question("TQ1")).await,
        StatusCode::CONFLICT,
        "gate_exists",
    );
}

#[tokio::test]
async fn delete_platform_question_409() {
    let x = setup().await;
    let lib = plain_noul(&x).await;
    refused(
        call(&x, "DELETE", &format!("/gates/{lib}"), Value::Null).await,
        StatusCode::CONFLICT,
        "platform_question",
    );
    refused(
        call(&x, "DELETE", "/gates/ZZ9", Value::Null).await,
        StatusCode::NOT_FOUND,
        "gate_not_found",
    );
}

#[tokio::test]
async fn delete_tenant_question_ok() {
    let x = setup().await;
    ok(call(&x, "POST", "/gates", tenant_question("TQ1")).await);
    let (s, b) = call(&x, "DELETE", "/gates/TQ1", Value::Null).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert!(questions(&x).await.iter().all(|q| q["key"] != json!("TQ1")));
    assert!(
        ledger(&x).await.is_empty(),
        "added then deleted: back to live"
    );
}
