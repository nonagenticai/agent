//! B5 T9 — the Laya client (`job_design::gates::laya`), the pure evaluator (`gates::evaluate`, web `gateRules.ts`),
//! the per-point state (`gates::state`) and the internal Laya provider (D70, `providers::llm::internal_resolved`).
//! Laya is an axum fake on loopback (ok / bool / slow / 503 / 422 / malformed), never the real service.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_api::connectors::transport::Transport;
use agent_api::design::{DesignOf, JobRef};
use agent_api::job_design::gates::evaluate::{
    self, Outcome, RawAnswer, apply_rule, asked_for_tool, decide, evaluate,
};
use agent_api::job_design::gates::laya::{self, LayaAnswer, LayaClient, LayaError};
use agent_api::job_design::gates::state;
use agent_api::job_design::types::{
    EffGateQuestion, GateAction, GateDecision, GateFail, GatePoint, GateRule,
};
use agent_api::netguard::Policy;
use agent_api::providers::llm::{self, Kind, Use};
use agent_api::runtime::gates::VerdictCause;
use agent_api::tenancy::begin_scoped;
use agent_model::kinds::catalog::SideEffect;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{self, insert_tenant, insert_user, person, send};

pub const TOKEN: &str = "laya-licence-token-t9";

// ---- the fake Laya ------------------------------------------------------------------------------------------------

/// Every call the fake saw: (`Authorization`, body).
#[derive(Clone, Default)]
pub struct Seen(Arc<Mutex<Vec<(Option<String>, Value)>>>);

impl Seen {
    pub fn all(&self) -> Vec<(Option<String>, Value)> {
        self.0.lock().unwrap().clone()
    }
}

pub struct Fake {
    /// `http://127.0.0.1:<port>`.
    pub base: String,
    pub seen: Seen,
}

impl Fake {
    /// The base URL of a fake mode (`ok`, `bool`, `slow`, `down`, `invalid`, `malformed`).
    pub fn url(&self, mode: &str) -> String {
        format!("{}/{mode}", self.base)
    }
}

fn record(s: &Seen, h: &HeaderMap, body: &Value) {
    let a = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    s.0.lock().unwrap().push((a, body.clone()));
}

/// Answers every asked question: noul 0.94 (or `true` with `bool`), choice `issue` (else the first criterion) at
/// confidence 0.85, score 3 (0-based) at confidence 0.8.
fn answers(body: &Value, noul_bool: bool) -> Value {
    let mut out = serde_json::Map::new();
    for (qid, q) in body["questions"].as_object().unwrap() {
        let a = match q["type"].as_str().unwrap() {
            "noul" if noul_bool => json!({"type": "noul", "noul": true}),
            "noul" => json!({"type": "noul", "noul": 0.94}),
            "choice" => {
                let keys: Vec<&String> = q["criteria"].as_object().unwrap().keys().collect();
                let top = keys
                    .iter()
                    .find(|k| k.as_str() == "issue")
                    .unwrap_or(&keys[0])
                    .to_string();
                let mut probs = serde_json::Map::new();
                for k in &keys {
                    probs.insert(k.to_string(), json!(if **k == top { 0.9 } else { 0.05 }));
                }
                json!({"type": "choice", "choice": top, "probabilities": probs, "confidence": 0.85})
            }
            _ => {
                let n = q["criteria"].as_array().unwrap().len();
                let top = 3.min(n - 1);
                let probs: serde_json::Map<String, Value> = (0..n)
                    .map(|i| (i.to_string(), json!(if i == top { 0.8 } else { 0.05 })))
                    .collect();
                json!({"type": "score", "score": top, "legend": {}, "probabilities": probs, "confidence": 0.8})
            }
        };
        out.insert(qid.clone(), a);
    }
    json!({"model": "laya-jev-test", "answers": out, "usage": {"input_tokens": 42, "output_tokens": 3}})
}

async fn ok(State(s): State<Seen>, h: HeaderMap, Json(b): Json<Value>) -> Json<Value> {
    record(&s, &h, &b);
    Json(answers(&b, false))
}

async fn as_bool(State(s): State<Seen>, h: HeaderMap, Json(b): Json<Value>) -> Json<Value> {
    record(&s, &h, &b);
    Json(answers(&b, true))
}

async fn slow(State(s): State<Seen>, h: HeaderMap, Json(b): Json<Value>) -> Json<Value> {
    record(&s, &h, &b);
    tokio::time::sleep(Duration::from_secs(2)).await;
    Json(answers(&b, false))
}

async fn down(
    State(s): State<Seen>,
    h: HeaderMap,
    Json(b): Json<Value>,
) -> (StatusCode, Json<Value>) {
    record(&s, &h, &b);
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({"detail": format!("laya backend unavailable {TOKEN}")})),
    )
}

async fn invalid(
    State(s): State<Seen>,
    h: HeaderMap,
    Json(b): Json<Value>,
) -> (StatusCode, Json<Value>) {
    record(&s, &h, &b);
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{"loc": ["body", "model"], "msg": "field required"}]})),
    )
}

async fn malformed(
    State(s): State<Seen>,
    h: HeaderMap,
    Json(b): Json<Value>,
) -> (StatusCode, String) {
    record(&s, &h, &b);
    (StatusCode::OK, format!("{{\"answers\": [{TOKEN}"))
}

pub async fn fake() -> Fake {
    let seen = Seen::default();
    let app = Router::new()
        .route("/ok/v1/systemone", post(ok))
        .route("/bool/v1/systemone", post(as_bool))
        .route("/slow/v1/systemone", post(slow))
        .route("/down/v1/systemone", post(down))
        .route("/invalid/v1/systemone", post(invalid))
        .route("/malformed/v1/systemone", post(malformed))
        .with_state(seen.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    Fake {
        base: format!("http://{addr}"),
        seen,
    }
}

pub fn client(f: &Fake, mode: &str) -> LayaClient {
    LayaClient::new(
        reqwest::Client::new(),
        &f.url(mode),
        "jev-latest",
        Some(TOKEN.to_owned()),
    )
    .unwrap()
}

// ---- questions ------------------------------------------------------------------------------------------------------

pub fn rule(
    fire_when: &str,
    threshold: Option<f64>,
    values: Option<&[&str]>,
    min_conf: Option<f64>,
) -> Value {
    json!({"fire_when": fire_when, "threshold": threshold, "fire_values": values, "min_confidence": min_conf})
}

/// One question; `options` = (key, label, description, hidden).
#[allow(clippy::too_many_arguments)]
pub fn question(
    key: &str,
    point: &str,
    kind: &str,
    options: &[(&str, &str, &str, bool)],
    rule: Value,
    fail: &str,
    action: &str,
    tools: Option<&[&str]>,
) -> EffGateQuestion {
    let options: Vec<Value> = options
        .iter()
        .map(|(k, l, d, h)| {
            json!({"key": k, "label": l, "description": d, "generated": false, "base_label": l, "hidden": h,
                   "refined": false, "verdicts": 0})
        })
        .collect();
    serde_json::from_value(json!({
        "key": key, "group": "base", "layer": "base", "point": point, "type": kind,
        "question": format!("Question {key}?"), "options": options, "generated_from": null, "threshold": "",
        "threshold_value": null, "fail": fail, "safety": fail == "closed", "effect": "", "rule": rule.clone(),
        "action": action, "tools": tools, "origin": "platform", "updated_by": null,
        "updated_at": "2026-10-22T00:00:00Z", "base_threshold_value": null, "threshold_refined": false,
        "base_rule": rule, "rule_refined": false,
        "stats": {"verdicts_7d": 0, "fail_open": 0, "fail_closed": 0, "below_threshold": 0,
                  "latency_p50_ms": null, "latency_p95_ms": null, "timeouts": 0},
    }))
    .expect("EffGateQuestion")
}

pub fn noul(
    key: &str,
    point: &str,
    fail: &str,
    action: &str,
    tools: Option<&[&str]>,
) -> EffGateQuestion {
    question(
        key,
        point,
        "noul",
        &[
            ("true", "true", "The customer threatens legal action", false),
            ("false", "false", "No legal threat", false),
        ],
        rule("gte", Some(0.5), None, None),
        fail,
        action,
        tools,
    )
}

fn choice(key: &str) -> EffGateQuestion {
    question(
        key,
        "P1",
        "choice",
        &[
            ("issue", "Issue", "Something is broken", false),
            ("need_change", "Need change", "", false),
            ("secret", "Secret", "A hidden option", true),
        ],
        rule("in", None, Some(&["issue"]), Some(0.5)),
        "open",
        "block",
        None,
    )
}

fn score(key: &str) -> EffGateQuestion {
    let levels = ["calm", "mild", "annoyed", "frustrated", "angry"];
    let labels: Vec<String> = levels
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{} · {l}", i + 1))
        .collect();
    let keys: Vec<String> = (1..=5).map(|i| i.to_string()).collect();
    let opts: Vec<(&str, &str, &str, bool)> = keys
        .iter()
        .zip(&labels)
        .zip(levels)
        .map(|((k, l), d)| (k.as_str(), l.as_str(), d, false))
        .collect();
    question(
        key,
        "P1",
        "score",
        &opts,
        rule("gte", Some(4.0), None, None),
        "open",
        "handover",
        None,
    )
}

// ---- the client ---------------------------------------------------------------------------------------------------

#[tokio::test]
async fn builds_jev_request() {
    let f = fake().await;
    let (n, c, s) = (
        noul("N1", "P1", "closed", "handover", None),
        choice("C1"),
        score("S1"),
    );
    let asked = [&n, &c, &s];
    let st = state::request_state(&["customer: hi"], "I will sue you");
    let reply = client(&f, "ok").ask(st.clone(), &asked).await.unwrap();
    let seen = f.seen.all();
    assert_eq!(seen.len(), 1, "one call per gate point");
    let (auth, body) = &seen[0];
    assert_eq!(auth.as_deref(), Some(format!("Bearer {TOKEN}").as_str()));
    assert_eq!(body["model"], "jev-latest");
    assert_eq!(body["state"], st);
    assert_eq!(
        body["questions"]["N1"],
        json!({"type": "noul", "instructions": "Question N1?",
               "criteria": {"true": "The customer threatens legal action", "false": "No legal threat"}})
    );
    assert_eq!(
        body["questions"]["C1"]["criteria"],
        json!({"issue": "Something is broken", "need_change": "Need change"}),
        "visible options only; an empty description falls back to the label"
    );
    assert_eq!(
        body["questions"]["S1"]["criteria"],
        json!(["calm", "mild", "annoyed", "frustrated", "angry"]),
        "levels in order"
    );
    assert_eq!((reply.input_tokens, reply.output_tokens), (42, 3));
    assert_eq!(reply.model.as_deref(), Some("laya-jev-test"));
    // a noul question without descriptions sends no criteria (bridge: absent is allowed)
    let bare = question(
        "N2",
        "P1",
        "noul",
        &[("true", "true", "", false), ("false", "false", "", false)],
        rule("gte", Some(0.5), None, None),
        "open",
        "block",
        None,
    );
    let r = laya::request("jev-latest", json!("x"), &[&bare]);
    assert!(
        serde_json::to_value(&r).unwrap()["questions"]["N2"]
            .get("criteria")
            .is_none()
    );
}

#[tokio::test]
async fn decodes_noul_number_and_bool() {
    let f = fake().await;
    let n = noul("N1", "P1", "open", "block", None);
    let r = client(&f, "ok").ask(json!("x"), &[&n]).await.unwrap();
    assert_eq!(r.answers["N1"], LayaAnswer::Noul { p_true: 0.94 });
    let r = client(&f, "bool").ask(json!("x"), &[&n]).await.unwrap();
    assert_eq!(r.answers["N1"], LayaAnswer::Noul { p_true: 1.0 });
    let r = laya::decode(br#"{"answers": {"N1": {"noul": false}}}"#, &[&n]).unwrap();
    assert_eq!(r.answers["N1"], LayaAnswer::Noul { p_true: 0.0 });
    for bad in [
        r#"{"answers": {"N1": {"noul": 1.5}}}"#,
        r#"{"answers": {"N1": {"noul": "yes"}}}"#,
        r#"{"answers": {}}"#,
    ] {
        assert!(
            matches!(
                laya::decode(bad.as_bytes(), &[&n]),
                Err(LayaError::BadBody(_))
            ),
            "{bad}"
        );
    }
}

#[tokio::test]
async fn score_level_is_one_based() {
    let f = fake().await;
    let s = score("S1");
    let r = client(&f, "ok").ask(json!("x"), &[&s]).await.unwrap();
    let LayaAnswer::Score {
        level,
        probabilities,
        confidence,
    } = &r.answers["S1"]
    else {
        panic!("{:?}", r.answers)
    };
    assert_eq!((*level, *confidence), (4.0, 0.8), "score 3 is level 4");
    assert_eq!(probabilities["4"], 0.8);
    assert!(!probabilities.contains_key("0"), "keys are 1-based levels");
    let a = evaluate::eval_answer(&s, &r.answers["S1"]);
    assert_eq!(
        (a.value.as_str(), a.answer.as_str()),
        ("4", "4 · frustrated")
    );
    assert!(a.fired, "level 4 ≥ 4");
    assert_eq!(a.action, Some(GateAction::Handover));
    assert_eq!(a.probabilities[0].key, "4");
    // a score above the top level is a bad body
    assert!(matches!(
        laya::decode(
            br#"{"answers": {"S1": {"score": 5, "confidence": 0.9}}}"#,
            &[&s]
        ),
        Err(LayaError::BadBody(_))
    ));
}

#[tokio::test]
async fn timeout_is_timeout_within_budget() {
    let f = fake().await;
    let n = noul("N1", "P1", "open", "block", None);
    let t0 = Instant::now();
    let e = client(&f, "slow")
        .with_timeout(Duration::from_millis(300))
        .ask(json!("x"), &[&n])
        .await
        .unwrap_err();
    assert_eq!(e, LayaError::Timeout);
    assert!(
        t0.elapsed() < Duration::from_millis(600),
        "{:?}",
        t0.elapsed()
    );
}

#[tokio::test]
async fn status_503_and_422_are_status_errors() {
    let f = fake().await;
    let n = noul("N1", "P1", "open", "block", None);
    let e = client(&f, "down").ask(json!("x"), &[&n]).await.unwrap_err();
    assert_eq!(e, LayaError::Status(503));
    let e = client(&f, "invalid")
        .ask(json!("x"), &[&n])
        .await
        .unwrap_err();
    assert_eq!(e, LayaError::Status(422));
    assert_eq!(e.code(), "status");
}

#[tokio::test]
async fn malformed_is_bad_body() {
    let f = fake().await;
    let n = noul("N1", "P1", "open", "block", None);
    let e = client(&f, "malformed")
        .ask(json!("x"), &[&n])
        .await
        .unwrap_err();
    assert!(matches!(e, LayaError::BadBody(_)), "{e:?}");
}

#[tokio::test]
async fn errors_never_contain_url_or_token() {
    let f = fake().await;
    let n = noul("N1", "P1", "open", "block", None);
    // a port that never answers HTTP: held for the whole test (a dropped listener frees the port for a parallel
    // test's fake server, which then answers 404) and every connection closed before a byte is read. A
    // bound-but-not-listening socket would do on Linux (connect refused), but macOS drops the SYN and the call
    // times out instead.
    let held = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead = held.local_addr().unwrap();
    let _resetter = tokio::spawn(async move {
        while let Ok((conn, _)) = held.accept().await {
            drop(conn);
        }
    });
    let unreachable = LayaClient::new(
        reqwest::Client::new(),
        &format!("http://{dead}/x"),
        "jev-latest",
        Some(TOKEN.to_owned()),
    )
    .unwrap();
    let mut errors = Vec::new();
    for mode in ["down", "invalid", "malformed"] {
        errors.push(
            client(&f, mode)
                .ask(json!("secret state"), &[&n])
                .await
                .unwrap_err(),
        );
    }
    errors.push(
        client(&f, "slow")
            .with_timeout(Duration::from_millis(200))
            .ask(json!("x"), &[&n])
            .await
            .unwrap_err(),
    );
    let e = unreachable.ask(json!("x"), &[&n]).await.unwrap_err();
    assert!(matches!(e, LayaError::Unreachable(_)), "{e:?}");
    errors.push(e);
    for e in &errors {
        for shown in [e.to_string(), format!("{e:?}")] {
            for leak in [
                TOKEN,
                "127.0.0.1",
                "/v1/systemone",
                "secret state",
                &dead.port().to_string(),
            ] {
                assert!(!shown.contains(leak), "{shown} contains {leak}");
            }
        }
    }
    let c = client(&f, "ok");
    assert!(!format!("{c:?}").contains(TOKEN));
}

// ---- the evaluator (gateRules.ts) ---------------------------------------------------------------------------------

fn raw(value: &str, measure: f64, confidence: f64) -> RawAnswer {
    RawAnswer {
        value: value.into(),
        measure,
        confidence,
    }
}

fn r(v: Value) -> GateRule {
    serde_json::from_value(v).unwrap()
}

#[test]
fn apply_rule_gte_lt_in() {
    let gte = r(rule("gte", Some(0.5), None, None));
    let lt = r(rule("lt", Some(0.5), None, None));
    let isin = r(rule("in", None, Some(&["issue", "need_change"]), None));
    let none = r(rule("gte", None, None, None));
    let at =
        |rule: &GateRule, a: &RawAnswer| apply_rule(rule, a, GateFail::Open, GateAction::Block);
    assert!(at(&gte, &raw("true", 0.5, 0.5)).fired, "gte is inclusive");
    assert!(!at(&gte, &raw("false", 0.49, 0.51)).fired);
    assert!(at(&lt, &raw("false", 0.49, 0.51)).fired);
    assert!(!at(&lt, &raw("true", 0.5, 0.5)).fired);
    assert!(at(&isin, &raw("need_change", 0.3, 0.3)).fired);
    assert!(!at(&isin, &raw("other", 0.9, 0.9)).fired);
    assert!(
        !at(&none, &raw("true", 1.0, 1.0)).fired,
        "no threshold never fires"
    );
    let o = at(&gte, &raw("true", 0.9, 0.9));
    assert_eq!(
        (o.fired, o.unsure, o.action),
        (true, false, Some(GateAction::Block))
    );
    let o = at(&gte, &raw("false", 0.1, 0.9));
    assert_eq!(o.action, None);
}

#[test]
fn unsure_below_min_confidence_fails_policy() {
    let rl = r(rule("in", None, Some(&["issue"]), Some(0.6)));
    let a = raw("issue", 0.55, 0.55);
    let closed = apply_rule(&rl, &a, GateFail::Closed, GateAction::Block);
    assert_eq!(
        (closed.fired, closed.unsure, closed.action),
        (false, true, Some(GateAction::Handover)),
        "unsure + fail closed → a person takes over"
    );
    let open = apply_rule(&rl, &a, GateFail::Open, GateAction::Block);
    assert_eq!((open.fired, open.unsure, open.action), (false, true, None));
    let sure = apply_rule(
        &rl,
        &raw("issue", 0.6, 0.6),
        GateFail::Open,
        GateAction::Block,
    );
    assert!(sure.fired && !sure.unsure, "at the line is sure");
}

#[test]
fn decide_handover_beats_block() {
    use GateAction::*;
    assert_eq!(
        decide([Some(Block), Some(Handover), None]),
        GateDecision::Handover
    );
    assert_eq!(
        decide([Some(Block), Some(Annotate), None]),
        GateDecision::Block
    );
    assert_eq!(decide([Some(Annotate), None]), GateDecision::Allow);
    assert_eq!(decide([]), GateDecision::Allow);
}

#[tokio::test]
async fn evaluates_a_point_and_decides() {
    let f = fake().await;
    let (n, c) = (noul("N1", "P1", "open", "block", None), choice("C1"));
    let res = evaluate(Some(&client(&f, "ok")), json!("x"), &[&n, &c]).await;
    assert_eq!(res.outcome, Outcome::Answered);
    assert_eq!(res.decision, GateDecision::Block);
    assert_eq!(res.deciding, ["N1", "C1"]);
    assert_eq!(res.answers.len(), 2);
    assert_eq!(res.answers[0].answer, "true");
    assert_eq!(res.answers[1].value, "issue");
    assert_eq!(res.input_tokens, 42);
    let v = res.verdict(Some(7));
    assert_eq!((v.cause, v.evaluation_id), (VerdictCause::Fired, Some(7)));
    assert_eq!(v.handover_reason(), None);
}

#[tokio::test]
async fn closed_question_on_timeout_hands_over() {
    let f = fake().await;
    let closed = noul("SAFE", "P1", "closed", "block", None);
    let open = noul("SOFT", "P1", "open", "block", None);
    let c = client(&f, "slow").with_timeout(Duration::from_millis(300));
    let res = evaluate(Some(&c), json!("x"), &[&closed, &open]).await;
    assert_eq!(res.outcome, Outcome::FailedClosed);
    assert_eq!(res.decision, GateDecision::Handover, "never allow");
    assert_eq!(res.deciding, ["SAFE"]);
    assert_eq!(res.error, Some("timeout"));
    assert_eq!(res.skipped.len(), 2);
    assert!(res.latency_ms < 600, "{}", res.latency_ms);
    let v = res.verdict(None);
    assert_eq!(v.cause, VerdictCause::Unavailable);
    assert_eq!(v.handover_reason(), Some("gate_unavailable"));
    // a 503 is the same
    let res = evaluate(Some(&client(&f, "down")), json!("x"), &[&closed]).await;
    assert_eq!(
        (res.outcome, res.decision),
        (Outcome::FailedClosed, GateDecision::Handover)
    );
}

#[tokio::test]
async fn open_only_on_timeout_allows() {
    let f = fake().await;
    let open = noul("SOFT", "P1", "open", "handover", None);
    let c = client(&f, "slow").with_timeout(Duration::from_millis(300));
    let res = evaluate(Some(&c), json!("x"), &[&open]).await;
    assert_eq!(
        (res.outcome, res.decision),
        (Outcome::FailedOpen, GateDecision::Allow)
    );
    assert!(res.deciding.is_empty());
    let v = res.verdict(None);
    assert_eq!(
        (v.cause, v.handover_reason()),
        (VerdictCause::Unavailable, None)
    );
}

#[test]
fn tool_point_asks_only_covering_questions() {
    let qs = vec![
        noul(
            "REFUND",
            "P2",
            "open",
            "handover",
            Some(&["refund.issue", "order.cancel{full}"]),
        ),
        noul("ANY", "P2", "open", "block", None),
        noul("EMPTY", "P2", "open", "block", Some(&[])),
        noul("REQ", "P1", "open", "block", None),
    ];
    let keys = |op: &str, se: SideEffect| -> Vec<String> {
        asked_for_tool(&qs, op, se)
            .iter()
            .map(|q| q.key.clone())
            .collect()
    };
    assert_eq!(
        keys("refund.issue", SideEffect::P),
        ["REFUND", "ANY", "EMPTY"]
    );
    assert_eq!(
        keys("refund.issue{full}", SideEffect::P),
        ["REFUND", "ANY", "EMPTY"],
        "a tool covers its variants"
    );
    assert_eq!(
        keys("order.cancel", SideEffect::X),
        ["ANY", "EMPTY"],
        "a variant tool does not cover the bare op"
    );
    assert_eq!(
        keys("order.read", SideEffect::R),
        Vec::<String>::new(),
        "no-tools questions skip reads"
    );
    assert_eq!(
        keys("refund.issue", SideEffect::R),
        ["REFUND"],
        "a listed tool is asked whatever its side effect"
    );
    assert_eq!(keys("case.update", SideEffect::W), ["ANY", "EMPTY"]);
    assert_eq!(evaluate::asked_at(&qs, GatePoint::P1).len(), 1);
}

#[tokio::test]
async fn unbound_provider_skips_all() {
    let (a, b) = (
        noul("A", "P1", "closed", "handover", None),
        noul("B", "P1", "open", "block", None),
    );
    let res = evaluate(None, json!("x"), &[&a, &b]).await;
    assert_eq!(
        (res.outcome, res.decision),
        (Outcome::Skipped, GateDecision::Allow)
    );
    assert_eq!(res.skipped.len(), 2);
    assert!(
        res.skipped
            .iter()
            .all(|s| s.reason == evaluate::NO_PROVIDER)
    );
    assert!(res.answers.is_empty() && res.deciding.is_empty());
    assert_eq!(res.verdict(None).handover_reason(), None);
    // nothing asked: no call, allow
    let res = evaluate(None, json!("x"), &[]).await;
    assert_eq!(
        (res.outcome, res.decision),
        (Outcome::Answered, GateDecision::Allow)
    );
    assert!(res.answers.is_empty() && res.skipped.is_empty());
}

// ---- the internal Laya provider (D70) -------------------------------------------------------------------------------

fn any_job() -> JobRef {
    JobRef {
        id: Uuid::new_v4(),
        key: "none".into(),
    }
}

#[tokio::test]
async fn platform_tenant_gates_resolve_to_internal_laya_when_key_set() {
    let f = fake().await;
    let tenant = Uuid::new_v4();
    let laya_url = format!("{}/", f.url("ok"));
    let e = common::env_with(|vars| {
        vars.insert("AGENT_PLATFORM_TENANT_ID".into(), tenant.to_string());
        vars.insert("INTERNAL_LAYA_URL".into(), laya_url);
        vars.insert("INTERNAL_LAYA_KEY".into(), TOKEN.into());
    })
    .await;
    insert_tenant(&e.owner, tenant, &format!("Platform {tenant}")).await;
    let mut tx = begin_scoped(&e.state.db, tenant).await.unwrap();
    let g = llm::resolve(
        &mut tx,
        &e.state,
        tenant,
        &any_job(),
        Use::Gates,
        &DesignOf::Live,
    )
    .await
    .unwrap()
    .expect("internal laya");
    drop(tx);
    assert_eq!(
        (
            g.kind,
            g.provider_id,
            g.model.as_str(),
            g.funding,
            g.base_url.as_str()
        ),
        (
            Kind::Laya,
            None,
            "jev-latest",
            "local_gpu",
            f.url("ok").as_str()
        )
    );
    assert_eq!(g.key.as_ref().map(|k| k.expose()), Some(TOKEN));
    assert!(!format!("{g:?}").contains(TOKEN));
    // and it is called with the licence token as the bearer
    let c = LayaClient::for_resolved(&g, &Policy::default(), tenant)
        .await
        .unwrap();
    let n = noul("N1", "P1", "closed", "handover", None);
    let res = evaluate(Some(&c), state::request_state::<&str>(&[], "hello"), &[&n]).await;
    assert_eq!(res.outcome, Outcome::Answered);
    assert_eq!(
        f.seen.all()[0].0.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
}

#[tokio::test]
async fn platform_tenant_gates_unbound_without_key() {
    let tenant = Uuid::new_v4();
    let e = common::env_with(|vars| {
        vars.insert("AGENT_PLATFORM_TENANT_ID".into(), tenant.to_string());
    })
    .await;
    insert_tenant(&e.owner, tenant, &format!("Platform {tenant}")).await;
    let mut tx = begin_scoped(&e.state.db, tenant).await.unwrap();
    let g = llm::resolve(
        &mut tx,
        &e.state,
        tenant,
        &any_job(),
        Use::Gates,
        &DesignOf::Live,
    )
    .await
    .unwrap();
    assert!(g.is_none());
    // qualify still resolves to the internal vLLM
    let q = llm::resolve(
        &mut tx,
        &e.state,
        tenant,
        &any_job(),
        Use::Qualify,
        &DesignOf::Live,
    )
    .await
    .unwrap()
    .expect("internal qualify");
    assert_eq!(q.kind, Kind::Openai);
}

fn with_keys(vars: &mut std::collections::HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    vars.insert(
        "PROVIDER_SECRET_KEYS".into(),
        format!(r#"{{"p1":"{key}"}}"#),
    );
    vars.insert("PROVIDER_SECRET_KEY_CURRENT".into(), "p1".into());
}

/// A customer tenant on the qualify allow-list, with the internal Laya key configured: unbound gates stay unbound
/// (no allow-list fallback for gates); its own Laya binding resolves to its provider.
#[tokio::test]
async fn customer_tenant_unaffected() {
    let f = fake().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|vars| {
        with_keys(vars);
        vars.insert("INTERACTION_LLM_TENANTS".into(), tenant.to_string());
        vars.insert("INTERNAL_LAYA_KEY".into(), TOKEN.into());
    })
    .await;
    let job = crate::b5::support::new_job_for(&e, tenant).await;
    let mut tx = begin_scoped(&e.state.db, tenant).await.unwrap();
    for of in [DesignOf::Live, DesignOf::Draft] {
        let g = llm::resolve(&mut tx, &e.state, tenant, &job.job_ref(), Use::Gates, &of)
            .await
            .unwrap();
        assert!(g.is_none(), "no allow-list fallback for gates");
    }
    drop(tx);
    // bind the tenant's own Laya (draft) through the providers API
    let mut st = e.state.clone();
    st.connectors = Transport::new(Policy {
        allow_loopback: true,
        private_targets: vec![],
    });
    let app = agent_api::router(st);
    let sub = format!("u-im-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, "integrations_manager").await;
    let im = person(&sub, &[]);
    let (s, b) = send(
        &app,
        common::json(
            "POST",
            "/api/providers",
            Some(&im),
            json!({"name": "Own Laya", "kind": "laya", "base_url": f.url("ok"), "models": ["jev-latest"],
                   "secret": "tenant-own-key"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let p: Uuid = b["id"].as_str().unwrap().parse().unwrap();
    let (s, b) = send(
        &app,
        common::json(
            "PUT",
            "/api/providers/uses/gates",
            Some(&im),
            json!({"provider_id": p, "model": "jev-latest"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut tx = begin_scoped(&e.state.db, tenant).await.unwrap();
    let g = llm::resolve(
        &mut tx,
        &e.state,
        tenant,
        &job.job_ref(),
        Use::Gates,
        &DesignOf::Draft,
    )
    .await
    .unwrap()
    .expect("the tenant binding wins");
    assert_eq!(
        (g.kind, g.provider_id, g.funding),
        (Kind::Laya, Some(p), "tenant_byo")
    );
    assert_eq!(g.key.as_ref().map(|k| k.expose()), Some("tenant-own-key"));
}

#[test]
fn config_debug_redacts_laya_key() {
    let mut vars = common::base_env("postgres://owner:pw@127.0.0.1:5432/b5_cfg");
    let cfg = agent_api::config::Config::from_lookup(|k| vars.get(k).cloned()).unwrap();
    let ip = &cfg.internal_providers;
    assert_eq!(
        (
            ip.laya_url.as_str(),
            ip.laya_model.as_str(),
            ip.laya_key.is_none()
        ),
        ("https://api.nonagentic.ai/laya", "jev-latest", true)
    );
    vars.insert("INTERNAL_LAYA_KEY".into(), TOKEN.into());
    vars.insert(
        "INTERNAL_LAYA_URL".into(),
        "https://laya.example/laya/".into(),
    );
    vars.insert("INTERNAL_LAYA_MODEL".into(), "jev-next".into());
    let cfg = agent_api::config::Config::from_lookup(|k| vars.get(k).cloned()).unwrap();
    let ip = &cfg.internal_providers;
    assert_eq!(
        (
            ip.laya_url.as_str(),
            ip.laya_model.as_str(),
            ip.laya_key.as_deref()
        ),
        ("https://laya.example/laya", "jev-next", Some(TOKEN))
    );
    for shown in [format!("{cfg:?}"), format!("{ip:?}")] {
        assert!(!shown.contains(TOKEN), "{shown}");
    }
    assert!(format!("{ip:?}").contains("laya_key: Some(\"[redacted]\")"));
    vars.insert("INTERNAL_LAYA_URL".into(), "laya.internal:9210".into());
    let e = agent_api::config::Config::from_lookup(|k| vars.get(k).cloned())
        .unwrap_err()
        .to_string();
    assert!(e.contains("INTERNAL_LAYA_URL"), "{e}");
}

#[test]
fn laya_answers_map_to_measures() {
    let n = noul("N", "P1", "open", "block", None);
    let a = evaluate::raw_answer(&n, &LayaAnswer::Noul { p_true: 0.25 });
    assert_eq!(
        (a.value.as_str(), a.measure, a.confidence),
        ("false", 0.25, 0.75)
    );
    let c = choice("C");
    let a = evaluate::raw_answer(
        &c,
        &LayaAnswer::Choice {
            choice: "issue".into(),
            probabilities: BTreeMap::new(),
            confidence: 0.4,
        },
    );
    assert_eq!(
        (a.value.as_str(), a.measure, a.confidence),
        ("issue", 0.4, 0.4)
    );
    // below min_confidence 0.5 → unsure, fail open → no action
    let ans = evaluate::eval_answer(
        &c,
        &LayaAnswer::Choice {
            choice: "issue".into(),
            probabilities: BTreeMap::from([("issue".into(), 0.6), ("need_change".into(), 0.4)]),
            confidence: 0.4,
        },
    );
    assert!(ans.unsure && !ans.fired && ans.action.is_none());
    assert_eq!(ans.probabilities[0].label, "Issue");
}
