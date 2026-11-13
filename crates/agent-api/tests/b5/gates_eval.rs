//! B5 T10 — the gate playground (`POST /api/jobs/{job}/gates/evaluate`), the runtime's verdict log
//! (`job_design::gates::engine::{evaluate, log}`) and `GET /api/jobs/{job}/gates/verdicts`. The job's tenant is the
//! platform tenant (`AGENT_PLATFORM_TENANT_ID`), so its `gates` use resolves to the internal Laya (D70), here the
//! loopback fake of `laya.rs`. Real Postgres 18; the playground through `agent_app`, the runtime run as
//! `agent_runtime`.
use agent_api::design::{CaseDesign, DesignOf};
use agent_api::job_design::gates::engine::{self, EvalInput, LogAt, ToolAsk, UsageBy};
use agent_api::job_design::gates::library;
use agent_api::job_design::gates::state;
use agent_api::job_design::gates::stats::Stats;
use agent_api::job_design::types::{EffGateQuestion, GatePoint};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::b5::laya::{Fake, TOKEN, fake, noul, question, rule};
use crate::common::design_contract::{ContractJob, DESIGNER};
use crate::common::{self, TestEnv, get, insert_user, person, send};

const OBSERVER: &str = "u-eval-observer";

struct Ex {
    e: TestEnv,
    f: ContractJob,
    fake: Fake,
    designer: String,
    observer: String,
}

/// A platform-tenant job whose internal Laya is the fake in `mode`.
async fn setup(mode: &str) -> Ex {
    let fake = fake().await;
    let tenant = Uuid::new_v4();
    let url = fake.url(mode);
    let e = common::env_with(|vars| {
        vars.insert("AGENT_PLATFORM_TENANT_ID".into(), tenant.to_string());
        vars.insert("INTERNAL_LAYA_URL".into(), url);
        vars.insert("INTERNAL_LAYA_KEY".into(), TOKEN.into());
    })
    .await;
    let f = crate::b5::support::new_job_for(&e, tenant).await;
    insert_user(&e.owner, DESIGNER, tenant, "job_designer").await;
    insert_user(&e.owner, OBSERVER, tenant, "").await;
    Ex {
        e,
        f,
        fake,
        designer: person(DESIGNER, &[]),
        observer: person(OBSERVER, &[]),
    }
}

fn uri(x: &Ex, rest: &str) -> String {
    format!("/api/jobs/{}{rest}", x.f.job.job_key)
}

async fn eval(x: &Ex, body: Value) -> (StatusCode, Value) {
    send(
        &x.e.app,
        common::json("POST", &uri(x, "/gates/evaluate"), Some(&x.observer), body),
    )
    .await
}

async fn eval_ok(x: &Ex, body: Value) -> Value {
    let (s, b) = eval(x, body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b
}

async fn count(x: &Ex, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("{sql} WHERE job_id = $1")))
        .bind(x.f.job.job_id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap()
}

/// The effective questions of design `of` (the fold the playground and the runtime ask).
async fn design(x: &Ex, of: DesignOf) -> Vec<EffGateQuestion> {
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

/// A P1 question that is not a safety question (it may be turned off).
async fn p1_optional(x: &Ex) -> String {
    design(x, DesignOf::Draft)
        .await
        .into_iter()
        .find(|q| q.point == GatePoint::P1 && !q.safety)
        .expect("a non-safety P1 question")
        .key
}

async fn disable(x: &Ex, gate: &str) {
    let (s, b) = send(
        &x.e.app,
        common::json(
            "PUT",
            &uri(x, &format!("/gates/{gate}/enabled")),
            Some(&x.designer),
            json!({"enabled": false}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

fn keys(v: &Value, field: &str) -> Vec<String> {
    v[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["gate"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn playground_against_draft_when_draft_differs() {
    let x = setup("ok").await;
    let b = eval_ok(
        &x,
        json!({"point": "request", "text": "Where is my parcel?"}),
    )
    .await;
    assert_eq!(
        (&b["draft_differs"], &b["ran_against"], &b["gate_point"]),
        (&json!(false), &json!("live"), &json!("P1"))
    );
    assert!(!keys(&b, "answers").is_empty(), "{b}");
    let gate = p1_optional(&x).await;
    disable(&x, &gate).await;
    // the UI default: the draft, since it differs at this point
    let b = eval_ok(
        &x,
        json!({"point": "request", "text": "Where is my parcel?"}),
    )
    .await;
    assert_eq!(
        (&b["draft_differs"], &b["ran_against"]),
        (&json!(true), &json!("draft"))
    );
    assert!(!keys(&b, "answers").contains(&gate));
    let skipped = b["skipped"].as_array().unwrap();
    assert!(
        skipped
            .iter()
            .any(|s| s["gate"] == json!(gate) && s["reason"] == json!(library::DISABLED_REASON)),
        "{b}"
    );
    // another point is not affected by a P1 change
    let b = eval_ok(
        &x,
        json!({"point": "response", "text": "Your refund is on its way."}),
    )
    .await;
    assert_eq!(b["draft_differs"], json!(false), "{b}");
    // the call carried the licence token
    assert_eq!(
        x.fake.seen.all()[0].0.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
}

#[tokio::test]
async fn playground_against_live_explicit() {
    let x = setup("ok").await;
    let gate = p1_optional(&x).await;
    disable(&x, &gate).await;
    let b = eval_ok(
        &x,
        json!({"point": "request", "text": "Where is my parcel?", "against": "live"}),
    )
    .await;
    assert_eq!(
        (&b["draft_differs"], &b["ran_against"]),
        (&json!(true), &json!("live"))
    );
    assert!(
        keys(&b, "answers").contains(&gate),
        "live still asks it: {b}"
    );
}

#[tokio::test]
async fn playground_writes_no_verdict_rows() {
    let x = setup("ok").await;
    for point in ["request", "tool", "response"] {
        eval_ok(
            &x,
            json!({"point": point, "text": "I want my money back", "tool": "refund.issue"}),
        )
        .await;
    }
    assert_eq!(count(&x, "SELECT count(*) FROM gate_evaluation").await, 0);
    assert_eq!(count(&x, "SELECT count(*) FROM gate_answer").await, 0);
}

#[tokio::test]
async fn playground_refusals() {
    let x = setup("ok").await;
    let (s, b) = eval(&x, json!({"point": "request", "text": "   "})).await;
    assert_eq!(
        (s, &b["error"]["code"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &json!("empty_text"))
    );
    let (s, b) = eval(&x, json!({"point": "P1", "text": "hello"})).await;
    assert_eq!(
        (s, &b["error"]["code"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid_point"))
    );
    let (s, b) = eval(&x, json!({"point": "request", "text": "hello", "extra": 1})).await;
    assert_eq!(
        (s, &b["error"]["code"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &json!("invalid_body"))
    );
}

/// A choice question whose options are all hidden (generated per case) is not asked; nor is a disabled question,
/// nor a P2 question listing other tools; a tool-less P2 question is asked in the playground but not before a read.
#[test]
fn playground_skips_generated_option_questions_without_options() {
    let mut per_case = question(
        "GX",
        "P1",
        "choice",
        &[("a", "A", "", true), ("b", "B", "", true)],
        rule("in", None, Some(&["a"]), None),
        "open",
        "annotate",
        None,
    );
    per_case.generated_from = Some("the case's orders".into());
    let mut off = noul("OFF", "P1", "open", "block", None);
    off.enabled = false;
    let asked = noul("ON", "P1", "open", "block", None);
    let qs = [per_case, off, asked];
    let sel = engine::select(&qs, GatePoint::P1, None);
    assert_eq!(
        sel.asked.iter().map(|q| q.key.as_str()).collect::<Vec<_>>(),
        ["ON"]
    );
    let reasons: Vec<(&str, &str)> = sel
        .skipped
        .iter()
        .map(|s| (s.gate.as_str(), s.reason.as_str()))
        .collect();
    assert_eq!(
        reasons,
        [
            ("GX", "options are built per case from the case's orders"),
            ("OFF", library::DISABLED_REASON)
        ]
    );
    let only_refund = noul("RF", "P2", "open", "block", Some(&["refund.issue"]));
    let any = noul("ANY", "P2", "open", "block", None);
    let p2 = [only_refund, any];
    let named = |t| {
        engine::select(&p2, GatePoint::P2, Some(ToolAsk::Named(t)))
            .asked
            .iter()
            .map(|q| q.key.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(named(Some("refund.issue{full}")), ["RF", "ANY"]);
    assert_eq!(named(Some("order.read")), ["ANY"]);
    let sel = engine::select(&p2, GatePoint::P2, Some(ToolAsk::Named(None)));
    assert_eq!(
        sel.skipped[0].reason,
        "asked only for refund.issue; no tool given"
    );
    let read = engine::select(
        &p2,
        GatePoint::P2,
        Some(ToolAsk::Call {
            op_key: "order.read",
            side_effect: agent_model::kinds::catalog::SideEffect::R,
        }),
    );
    assert!(read.asked.is_empty());
}

#[tokio::test]
async fn playground_laya_down_reports_skipped_and_fail_policy() {
    let x = setup("down").await;
    let b = eval_ok(&x, json!({"point": "request", "text": "I will sue you"})).await;
    assert_eq!(b["answers"], json!([]), "{b}");
    let p1: Vec<EffGateQuestion> = design(&x, DesignOf::Live)
        .await
        .into_iter()
        .filter(|q| q.point == GatePoint::P1 && q.enabled)
        .collect();
    let unavailable: Vec<String> = b["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| {
            s["reason"]
                .as_str()
                .unwrap()
                .starts_with("Laya unavailable")
        })
        .map(|s| s["gate"].as_str().unwrap().to_owned())
        .collect();
    assert!(!unavailable.is_empty(), "{b}");
    let closed: Vec<String> = p1
        .iter()
        .filter(|q| {
            unavailable.contains(&q.key) && q.fail == agent_api::job_design::types::GateFail::Closed
        })
        .map(|q| q.key.clone())
        .collect();
    let want = if closed.is_empty() {
        "allow"
    } else {
        "handover"
    };
    assert_eq!(
        b["decision"],
        json!(want),
        "never allow with a closed question: {b}"
    );
    assert_eq!(b["deciding"], json!(closed));
}

#[tokio::test]
async fn runtime_evaluate_logs_evaluation_and_answers_without_text() {
    let x = setup("ok").await;
    let mut st = x.e.state.clone();
    st.db = common::runtime_pool(&x.e.owner).await;
    let qs = design(&x, DesignOf::Draft).await;
    let jr = x.f.job_ref();
    let case = CaseDesign::Draft { draft_rev: 0 };
    let secret = "SECRET-CUSTOMER-TEXT-7731";
    let run = engine::evaluate(
        &st,
        EvalInput {
            tenant: x.f.tenant,
            job: &jr,
            of: &DesignOf::Draft,
            questions: &qs,
            point: GatePoint::P1,
            tool: None,
            state: state::request_state(&["hello"], secret),
            usage: UsageBy {
                actor: "callers:web",
                activity: None,
                conversation_id: Some("conv-t10"),
                case_id: Some("C-1"),
            },
            log: Some(LogAt {
                case: &case,
                conversation_id: "conv-t10",
                case_id: Some("C-1"),
            }),
        },
    )
    .await
    .unwrap_or_else(|e| panic!("evaluate: {e:?}"));
    assert!(run.called && !run.asked.is_empty());
    let id = run.evaluation_id.expect("logged");
    assert_eq!(run.verdict().evaluation_id, Some(id));
    let (env, point, conv, case_id, outcome, model): (String, String, String, Option<String>, String, Option<String>) =
        sqlx::query_as(
            "SELECT env, point, conversation_id, case_id, outcome, model FROM gate_evaluation WHERE id = $1",
        )
        .bind(id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(
        (
            env.as_str(),
            point.as_str(),
            conv.as_str(),
            case_id.as_deref(),
            model.as_deref()
        ),
        ("test", "P1", "conv-t10", Some("C-1"), Some("laya-jev-test"))
    );
    assert!(["pass", "fired"].contains(&outcome.as_str()), "{outcome}");
    let mut gates: Vec<String> =
        sqlx::query_scalar("SELECT gate FROM gate_answer WHERE evaluation_id = $1")
            .bind(id)
            .fetch_all(&x.e.owner)
            .await
            .unwrap();
    gates.sort();
    let mut asked = run.asked.clone();
    asked.sort();
    assert_eq!(gates, asked);
    let leaked: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM gate_evaluation e WHERE e.job_id = $1 AND row_to_json(e)::text LIKE $2) \
              + (SELECT count(*) FROM gate_answer a WHERE a.job_id = $1 AND row_to_json(a)::text LIKE $2)",
    )
    .bind(x.f.job.job_id)
    .bind(format!("%{secret}%"))
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(leaked, 0, "the gate log never holds the customer text");
    // the verdict log is what gateVerdicts serves
    let (s, b) = send(
        &x.e.app,
        get(&uri(&x, "/gates/verdicts"), Some(&x.observer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b.as_array().unwrap().len(), run.asked.len());
    assert_eq!(b[0]["conversation_id"], json!("conv-t10"));
    assert_eq!(b[0]["case_id"], json!("C-1"));
}

/// One gate-log evaluation with one answer, `age` minutes old.
async fn logged(x: &Ex, age: i32, gate: &str, answer: Option<&str>, decision: &str) -> i64 {
    let (t, j) = (x.f.tenant, x.f.job.job_id);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO gate_evaluation (tenant_id, job_id, at, point, env, conversation_id, outcome, decision) \
         VALUES ($1, $2, now() - make_interval(mins => $3), 'P1', 'prod', 'conv-v', 'pass', $4) RETURNING id",
    )
    .bind(t)
    .bind(j)
    .bind(age)
    .bind(decision)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO gate_answer (evaluation_id, tenant_id, job_id, at, gate, answer, value, confidence, unsure) \
         VALUES ($1, $2, $3, now() - make_interval(mins => $4), $5, $6, $6, 0.42, true)",
    )
    .bind(id)
    .bind(t)
    .bind(j)
    .bind(age)
    .bind(gate)
    .bind(answer)
    .execute(&x.e.owner)
    .await
    .unwrap();
    id
}

#[tokio::test]
async fn verdicts_newest_first_limit() {
    let x = setup("ok").await;
    let qs = design(&x, DesignOf::Draft).await;
    let q = &qs[0];
    let old = logged(&x, 30, &q.key, Some("Issue"), "allow").await;
    let mid = logged(&x, 20, &q.key, None, "handover").await;
    let new = logged(&x, 10, "ZZGONE", Some("true"), "block").await;
    let (s, b) = send(
        &x.e.app,
        get(&uri(&x, "/gates/verdicts?limit=2"), Some(&x.observer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let rows = b.as_array().unwrap();
    assert_eq!(rows.len(), 2, "{b}");
    assert_eq!(rows[0]["id"], json!(format!("{new}:ZZGONE")));
    assert_eq!(
        rows[0]["question"],
        json!("ZZGONE"),
        "a gone question shows its key"
    );
    assert_eq!(rows[0]["action"], json!("block"));
    assert_eq!(rows[0]["case_id"], json!(""));
    assert_eq!(rows[1]["id"], json!(format!("{mid}:{}", q.key)));
    assert_eq!(
        rows[1]["question"],
        json!(q.question),
        "the draft's question text"
    );
    assert_eq!(rows[1]["answer"], json!("no answer"));
    assert_eq!(rows[1]["action"], json!("handover"));
    assert_eq!(rows[1]["below_threshold"], json!(true));
    assert_eq!(rows[1]["point"], json!("P1"));
    assert_eq!(rows[1]["confidence"], json!(0.42));
    // the default page holds all three
    let (_, b) = send(
        &x.e.app,
        get(&uri(&x, "/gates/verdicts"), Some(&x.observer)),
    )
    .await;
    let ids: Vec<&str> = b
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(ids[2], format!("{old}:{}", q.key));
}

#[tokio::test]
async fn verdicts_limit_out_of_range_422() {
    let x = setup("ok").await;
    for bad in ["0", "501", "abc", "1.5"] {
        let (s, b) = send(
            &x.e.app,
            get(
                &uri(&x, &format!("/gates/verdicts?limit={bad}")),
                Some(&x.observer),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
        assert_eq!(b["error"]["code"], json!("invalid_limit"));
    }
    let (s, _) = send(
        &x.e.app,
        get(&uri(&x, "/gates/verdicts?limit=500"), Some(&x.observer)),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn usage_recorded_once_per_laya_call() {
    let x = setup("ok").await;
    eval_ok(
        &x,
        json!({"point": "request", "text": "Where is my parcel?"}),
    )
    .await;
    let calls = x.fake.seen.all().len();
    assert_eq!(calls, 1, "one Laya call per point");
    let rows: Vec<(String, String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT source, use, provider_id, funding, tokens_in, tokens_out FROM usage_event WHERE job_id = $1",
    )
    .bind(x.f.job.job_id)
    .fetch_all(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [(
            "gate".to_string(),
            "gate_p1".to_string(),
            "internal".to_string(),
            "local_gpu".to_string(),
            42,
            3
        )]
    );
    assert_eq!(
        count(&x, "SELECT count(*) FROM metering_outbox").await,
        0,
        "Laya is billed by license-authz: no outbox row"
    );
    eval_ok(&x, json!({"point": "response", "text": "Done."})).await;
    assert_eq!(
        count(&x, "SELECT count(*) FROM usage_event").await,
        i64::try_from(x.fake.seen.all().len()).unwrap()
    );
}
