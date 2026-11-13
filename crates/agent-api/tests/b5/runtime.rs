//! B5 T12 — what the runtime reads of the job design (`RuntimeDesign::read` → `job_design::runtime::read`), the
//! runner's per-call tool gate (`job_design::gates::hooks::ToolPoint`) and B5's readiness contributors
//! (`job_design::readiness`). Real Postgres 18, a Demo Retail job created through `POST /api/jobs`, writes through the
//! HTTP router as `agent_app`. Production reads the live design (the release snapshot, or the baseline of a job never
//! released), Test the draft (B4 owner ruling 4).
use agent_api::design::DesignOf;
use agent_api::job_design::gates::engine::ToolAsk;
use agent_api::job_design::gates::hooks::{
    GATE_BLOCKED, NOT_ENTITLED, ReplyPoint, RequestPoint, ToolPoint, failed_verdict,
};
use agent_api::job_design::readiness::{GATES_UNREVIEWED, GatesReadiness, ToolsetsReadiness};
use agent_api::job_design::toolset::{DesignToolsets, REASON_NARROWED};
use agent_api::job_design::types::GatePoint;
use agent_api::platform::templates;
use agent_api::readiness::{self, AreaKey, ReadOf, ReadinessContributor};
use agent_api::release::compile::job_key;
use agent_api::runtime::assistant::tools::{ToolsetIndex, offered};
use agent_api::runtime::catalog::OperationCatalog;
use agent_api::runtime::design::RuntimeDesign;
use agent_api::runtime::domain::OpCall;
use agent_api::runtime::gates::{
    Decision, ReplyGate, ReplyInput, RequestGate, RequestInput, VerdictCause,
};
use agent_api::runtime::runner::authorise::ToolGate;
use agent_api::runtime::scope::Env;
use agent_api::runtime::spaces::{SpaceCatalog, TemplateProcedure};
use agent_api::state::AppState;
use agent_api::turn::probe;
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::b5::laya::{Fake, TOKEN, fake};
use crate::common::design_contract::{self, ContractJob, DESIGNER};
use crate::common::{self, TestEnv, insert_user, person, send};

/// An activity of the Demo Retail bundle the seed knows the operations of, and one of its operations (R).
const ACTIVITY: &str = "delivery:where_is_my_order";
const OP: &str = "order.read";
/// The space the embedded seed maps to [`ACTIVITY`] (R15).
const SPACE: &str = "retail:logistics:track_parcel";
/// A space of the `order-support` area (in the default and the cuttable selection).
const RETURNS: &str = "retail:order-support:returns";
/// A tenant procedure name no template uses.
const PROC: &str = "runtime_track";

struct Fx {
    e: TestEnv,
    f: ContractJob,
    designer: String,
    admin: String,
    /// The fake Laya the job's `gates` use resolves to (a platform-tenant job), if any.
    fake: Option<Fake>,
}

async fn fixture(e: TestEnv, f: ContractJob, fake: Option<Fake>) -> Fx {
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    let admin = design_contract::release_manager(&e, &f).await;
    Fx {
        e,
        f,
        designer: person(DESIGNER, &[]),
        admin,
        fake,
    }
}

/// A customer job: no `gates` provider is bound (every gate point is unbound).
async fn setup() -> Fx {
    let e = common::env().await;
    let f = crate::b5::support::new_job(&e).await;
    fixture(e, f, None).await
}

/// A platform-tenant job whose `gates` use resolves to the internal Laya: the T9 fake in `mode` (as `gates_eval`).
async fn setup_laya(mode: &str) -> Fx {
    let fk = fake().await;
    let tenant = Uuid::new_v4();
    let url = fk.url(mode);
    let e = common::env_with(|vars| {
        vars.insert("AGENT_PLATFORM_TENANT_ID".into(), tenant.to_string());
        vars.insert("INTERNAL_LAYA_URL".into(), url);
        vars.insert("INTERNAL_LAYA_KEY".into(), TOKEN.into());
    })
    .await;
    let f = crate::b5::support::new_job_for(&e, tenant).await;
    fixture(e, f, Some(fk)).await
}

fn uri(x: &Fx, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", x.f.job.job_key)
}

async fn write(x: &Fx, method: &str, tail: &str, tok: &str, body: Value) -> Value {
    let (s, b) = send(
        &x.e.app,
        common::json(method, &uri(x, tail), Some(tok), body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{method} {tail}: {b}");
    b
}

async fn load(x: &Fx, of: DesignOf) -> RuntimeDesign {
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let d = RuntimeDesign::load(&mut tx, &x.e.state.design, &x.f.job_ref(), &of)
        .await
        .unwrap_or_else(|e| panic!("load {of:?}: {e:?}"));
    tx.commit().await.unwrap();
    d
}

/// B4's template-only spaces and procedures of `d` (the pinned compile of its pins and selection). Compared with
/// the activity cleared on both sides: the v8 template names the activities itself, and the runtime's space
/// takes them from the job design (R15, asserted separately).
async fn template_only(x: &Fx, d: &RuntimeDesign) -> (SpaceCatalog, Vec<TemplateProcedure>) {
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let compiled = templates::compile_pinned(&mut tx, &d.pins, &d.selection, job_key(&d.pins))
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("the template compiles: {}", e.len()));
    tx.commit().await.unwrap();
    let content = &compiled.effective.job_content;
    (
        SpaceCatalog::from_items(&content.context_spaces),
        TemplateProcedure::all_from_items(&content.procedures),
    )
}

fn without_activity(c: &SpaceCatalog) -> SpaceCatalog {
    SpaceCatalog::new(
        c.spaces()
            .iter()
            .cloned()
            .map(|mut s| {
                s.activity.clear();
                s
            })
            .collect(),
    )
}

fn call(op: &str, activity: Option<&str>, input: Value) -> OpCall {
    call_as("procedure", op, activity, input)
}

fn call_as(role: &str, op: &str, activity: Option<&str>, input: Value) -> OpCall {
    serde_json::from_value(json!({
        "call_id": "opc_00000000-0000-0000-0000-000000000001", "case_id": "CS-T12", "conversation_id": "c-t12",
        "phase": 0, "step": "s", "attempt": 1, "agent_role": role, "activity": activity,
        "operation": op, "env": "test", "initiator": "procedure@t12", "deadline_ms": 0, "input": input,
    }))
    .unwrap()
}

async fn narrow(x: &Fx) {
    write(
        x,
        "POST",
        "toolsets/procedure/narrowings",
        &x.designer,
        json!({"activity": ACTIVITY, "op": OP}),
    )
    .await;
}

/// A tenant noul question (`POST …/gates`, 201) at `point` that fires on the fake's 0.94.
async fn add_question(x: &Fx, key: &str, point: &str, action: &str, fail: &str, tools: &[&str]) {
    let body = json!({
        "key": key,
        "point": point,
        "question": "Does the customer mention a competitor?",
        "criteria": {"true": "A competitor is named", "false": "No competitor is named"},
        "rule": {"fire_when": "gte", "threshold": 0.5, "fire_values": null, "min_confidence": null},
        "action": action,
        "fail": fail,
        "tools": (!tools.is_empty()).then_some(tools),
    });
    let (s, b) = send(
        &x.e.app,
        common::json("POST", &uri(x, "gates"), Some(&x.designer), body),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

async fn disable(x: &Fx, gate: &str) {
    write(
        x,
        "PUT",
        &format!("gates/{gate}/enabled"),
        &x.designer,
        json!({"enabled": false}),
    )
    .await;
}

/// The bodies the fake Laya received.
fn seen(x: &Fx) -> Vec<Value> {
    x.fake
        .as_ref()
        .expect("a Laya job")
        .seen
        .all()
        .into_iter()
        .map(|(_, b)| b)
        .collect()
}

fn asked(body: &Value) -> Vec<String> {
    body["questions"]
        .as_object()
        .map(|q| q.keys().cloned().collect())
        .unwrap_or_default()
}

async fn gate_rows(x: &Fx, point: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM gate_evaluation WHERE job_id = $1 AND point = $2")
        .bind(x.f.job.job_id)
        .bind(point)
        .fetch_one(&x.e.owner)
        .await
        .unwrap()
}

/// `x`'s state on a CLOSED pool: every database call inside a hook fails.
async fn broken_state(x: &Fx) -> AppState {
    let mut st = x.e.state.clone();
    let pool = common::runtime_pool(&x.e.owner).await;
    pool.close().await;
    st.db = pool;
    st
}

async fn p1(st: &AppState, x: &Fx, d: &RuntimeDesign) -> agent_api::runtime::gates::GateVerdict {
    let jr = x.f.job_ref();
    RequestPoint
        .check(
            st,
            RequestInput {
                tenant: x.f.tenant,
                job: &jr,
                env: Env::Test,
                conversation_id: "conv-t12",
                text: "Your competitor is cheaper",
                design: d,
            },
        )
        .await
}

#[tokio::test]
async fn job_without_job_design_rows_gets_template_content() {
    let x = setup().await;
    for of in [DesignOf::Live, DesignOf::Draft] {
        let d = load(&x, of.clone()).await;
        assert!(
            d.template_errors.is_empty(),
            "{of:?}: {:?}",
            d.template_errors
        );
        assert!(!d.spaces.spaces().is_empty(), "{of:?}: never empty");
        assert!(!d.procedures.is_empty(), "{of:?}: never empty");
        let (spaces, procedures) = template_only(&x, &d).await;
        assert_eq!(
            without_activity(&d.spaces),
            without_activity(&spaces),
            "{of:?}"
        );
        assert_eq!(d.procedures, procedures, "{of:?}");
        // R15: the seed's activity reaches the runtime's space (the route hinter and N2 read it)
        assert_eq!(
            d.spaces.by_key(SPACE).map(|s| s.activity.as_str()),
            Some(ACTIVITY),
            "{of:?}"
        );
        // the job design is read too: the library gates and the toolsets at the pins
        assert!(!d.job_design.gates.questions().is_empty(), "{of:?}");
        assert!(
            DesignToolsets
                .decide(&d, "procedure", Some(ACTIVITY), OP)
                .granted,
            "{of:?}"
        );
    }
}

/// Live CS-58 / audit of agent#64: the plan B4's `StaticProcedures` makes for a `track_parcel` request runs only
/// operations (and variants) the space's activity grants. The runner's tool gate (`ToolPoint`, as `bin/runner`
/// installs it) passes every step and tool requirement of that plan, in test and prod; `order.cancel` is refused `not_entitled`. Seed v8 (R2) folds the old `delivery:track_parcel` (variant read only) and
/// `purchase:order_status` (bare read) grants into `delivery:where_is_my_order`, so the no-variant `delivery.read` is
/// granted now; the control is an operation outside the grant.
#[tokio::test]
async fn the_track_parcel_plan_runs_only_what_its_activity_grants() {
    use agent_api::runtime::domain::QualifiedRequest;
    use agent_api::runtime::orchestrator::{ProcedureSource, StaticProcedures};
    let x = setup().await;
    let st = &x.e.state;
    for of in [DesignOf::Draft, DesignOf::Live] {
        let d = load(&x, of.clone()).await;
        let activity = d.spaces.by_key(SPACE).map(|s| s.activity.clone());
        assert_eq!(activity.as_deref(), Some(ACTIVITY), "{of:?}");
        let req: QualifiedRequest = serde_json::from_value(json!({
            "case_id": "CS-58", "conversation_id": "c-58", "area": "logistics", "context_space": SPACE,
            "request_type": "track_parcel", "activity": activity, "slots": {"tracking_ref": "RET-01"},
            "identity_level": 1
        }))
        .unwrap();
        let (procedure, phases) = StaticProcedures
            .plan(&d, &req)
            .unwrap_or_else(|| panic!("{of:?}: track_parcel has a plan"));
        assert_eq!(
            procedure.map(|p| p.name).as_deref(),
            Some("order_status"),
            "{of:?}"
        );
        let mut keys: Vec<String> = phases
            .iter()
            .flat_map(|p| p.steps.iter().map(|s| s.op_key()))
            .collect();
        assert!(!keys.is_empty(), "{of:?}");
        keys.extend(StaticProcedures.checks(&d, &req).tool_requirements);
        for key in &keys {
            let op = key.split('{').next().unwrap();
            assert_eq!(
                ToolPoint
                    .check(
                        st,
                        x.f.tenant,
                        &d,
                        &call(op, Some(ACTIVITY), json!({})),
                        key
                    )
                    .await,
                Ok(()),
                "{of:?}: {key}"
            );
        }
        assert_eq!(
            ToolPoint
                .check(
                    st,
                    x.f.tenant,
                    &d,
                    &call("order.cancel", Some(ACTIVITY), json!({})),
                    "order.cancel"
                )
                .await,
            Err(NOT_ENTITLED.to_owned()),
            "{of:?}: the control — an operation outside the activity's grant is refused"
        );
    }
}

#[tokio::test]
async fn prod_reads_snapshot_not_draft() {
    let x = setup().await;
    let live = load(&x, DesignOf::Live).await;
    let base_slots = live
        .spaces
        .by_key(RETURNS)
        .expect("the returns space")
        .slots
        .clone();
    let required = vec!["order_id".to_owned(), "t12_reason".to_owned()];
    assert_ne!(base_slots, required);
    // in the draft: a narrowing, a `required` override (B2's overlay) and a published tenant version
    narrow(&x).await;
    write(
        &x,
        "PUT",
        &format!("context-spaces/{RETURNS}"),
        &x.designer,
        json!({ "required": required }),
    )
    .await;
    write(
        &x,
        "POST",
        "procedures",
        &x.designer,
        json!({
            "name": PROC, "version": 1, "context_space": SPACE, "description": "Track it",
            "steps": [{"step_number": 1, "phase": 0, "name": "Read the order", "operation": OP, "variant": null,
                       "modality": "observe", "side_effect": "R", "identity_level": "0", "confirm_back": false,
                       "gate": false, "args": {"order_id": "$order_id"}}],
        }),
    )
    .await;
    for to in ["CANARY", "PUBLISHED"] {
        write(
            &x,
            "POST",
            &format!("procedures/{PROC}/versions/1/status"),
            &x.admin,
            json!({ "to": to }),
        )
        .await;
    }
    let test = load(&x, DesignOf::Draft).await;
    let prod = load(&x, DesignOf::Live).await;
    // toolsets
    let t = DesignToolsets.decide(&test, "procedure", Some(ACTIVITY), OP);
    assert_eq!(
        (t.granted, t.reason.as_deref()),
        (false, Some(REASON_NARROWED))
    );
    assert!(DesignToolsets.allows(&prod, "procedure", Some(ACTIVITY), OP));
    // spaces
    assert_eq!(test.spaces.by_key(RETURNS).unwrap().slots, required);
    assert_eq!(prod.spaces.by_key(RETURNS).unwrap().slots, base_slots);
    // procedures
    assert!(
        test.procedures
            .iter()
            .any(|p| p.name == PROC && p.version == 1)
    );
    assert!(!prod.procedures.iter().any(|p| p.name == PROC));

    // after apply, Production reads the release snapshot: both changes
    crate::b5::support::publish(&x.e, &x.f).await;
    let prod = load(&x, DesignOf::Live).await;
    assert_eq!(prod.spaces.by_key(RETURNS).unwrap().slots, required);
    let p = prod
        .procedures
        .iter()
        .find(|p| p.name == PROC)
        .expect("the published tenant version runs in Production");
    assert_eq!(p.step_defs.len(), 1);
    assert_eq!(p.step_defs[0].operation, OP);
}

#[tokio::test]
async fn tool_gate_refuses_a_governed_role_without_a_known_activity_and_defers_an_ungoverned_role()
{
    let x = setup().await;
    narrow(&x).await;
    let st = &x.e.state;
    let tenant = x.f.tenant;
    let test = load(&x, DesignOf::Draft).await;
    let prod = load(&x, DesignOf::Live).await;
    let none = json!({});
    // refused: a known activity whose toolset excludes the op (the same refusal the assistant's offer implies)
    assert_eq!(
        ToolPoint
            .check(
                st,
                tenant,
                &test,
                &call(OP, Some(ACTIVITY), none.clone()),
                OP
            )
            .await,
        Err(NOT_ENTITLED.to_owned())
    );
    let offer = offered(
        &test.catalog,
        &test,
        &DesignToolsets,
        "procedure",
        Some(ACTIVITY),
    );
    assert!(!offer.iter().any(|t| t.op_key == OP), "not offered either");
    // fail-closed: the design has toolsets for the role, so no activity or an activity it has no toolset for
    // is refused too, and not offered
    for activity in [None, Some(""), Some("nope:nope")] {
        assert_eq!(
            ToolPoint
                .check(st, tenant, &test, &call(OP, activity, none.clone()), OP)
                .await,
            Err(NOT_ENTITLED.to_owned()),
            "{activity:?}"
        );
        assert!(!DesignToolsets.allows(&test, "procedure", activity, OP));
        assert!(!DesignToolsets.allows(&test, "assistant", activity, OP));
    }
    let none_offered = offered(&test.catalog, &test, &DesignToolsets, "assistant", None);
    assert!(none_offered.is_empty(), "no activity: nothing offered");
    // a role the design has no toolset for defers to B4 (allowed)
    assert!(DesignToolsets.allows(&test, "nobody", None, OP));
    assert_eq!(
        ToolPoint
            .check(
                st,
                tenant,
                &test,
                &call_as("nobody", OP, None, none.clone()),
                OP
            )
            .await,
        Ok(())
    );
    // Production grants it; no gates provider is bound, so P2 asks nothing and the call passes
    assert_eq!(
        ToolPoint
            .check(st, tenant, &prod, &call(OP, Some(ACTIVITY), none), OP)
            .await,
        Ok(())
    );
}

// ---- the hooks against the fake Laya (T9) ------------------------------------------------------------------------------

/// A case planned before agent#65 carries an empty activity on a mapped space: its calls are judged by the activity
/// the design maps the case's context space to (exactly as if it carried it); an unmapped or unknown space is refused.
#[tokio::test]
async fn a_legacy_call_with_an_empty_activity_takes_its_context_spaces_activity() {
    let x = setup().await;
    narrow(&x).await;
    let st = &x.e.state;
    let tenant = x.f.tenant;
    let d = load(&x, DesignOf::Draft).await;
    let legacy = |case: &str| {
        let mut c = call(OP, Some(""), json!({}));
        c.case_id = case.to_owned();
        c
    };
    for (case, space) in [
        ("CS-9001", Some(SPACE)),
        ("CS-9002", Some("nope:nope")),
        ("CS-9003", None),
    ] {
        sqlx::query(
            "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev, context_space) \
             VALUES ($1, $2, $3, 'c-legacy', 'web_chat', 'test', 1, $4)",
        )
        .bind(tenant)
        .bind(x.f.job.job_id)
        .bind(case)
        .bind(space)
        .execute(&x.e.owner)
        .await
        .unwrap();
    }
    // the mapped space: judged as its activity would be (OP is narrowed for `procedure` there: refused; an op the
    // activity grants passes)
    let with_activity = ToolPoint
        .check(st, tenant, &d, &call(OP, Some(ACTIVITY), json!({})), OP)
        .await;
    assert_eq!(
        ToolPoint
            .check(st, tenant, &d, &legacy("CS-9001"), OP)
            .await,
        with_activity
    );
    let granted = d
        .job_design
        .toolsets
        .activity_grant(ACTIVITY)
        .into_iter()
        .find(|k| DesignToolsets.allows(&d, "procedure", Some(ACTIVITY), k))
        .expect("the activity grants something");
    assert_eq!(
        ToolPoint
            .check(st, tenant, &d, &legacy("CS-9001"), &granted)
            .await,
        Ok(())
    );
    // unmapped space, no space, unknown case: refused
    for case in ["CS-9002", "CS-9003", "CS-9999"] {
        assert_eq!(
            ToolPoint
                .check(st, tenant, &d, &legacy(case), &granted)
                .await,
            Err(NOT_ENTITLED.to_owned()),
            "{case}"
        );
    }
}

#[tokio::test]
async fn p1_asks_laya_logs_and_never_sends_a_disabled_question() {
    let x = setup_laya("ok").await;
    add_question(&x, "TPA", "P1", "annotate", "open", &[]).await;
    add_question(&x, "TPB", "P1", "annotate", "open", &[]).await;
    disable(&x, "TPB").await;
    let d = load(&x, DesignOf::Draft).await;
    let before = seen(&x).len();
    let v = p1(&x.e.state, &x, &d).await;
    let bodies = seen(&x);
    assert_eq!(bodies.len(), before + 1, "one Laya call per point");
    let q = asked(bodies.last().unwrap());
    assert!(q.contains(&"TPA".to_owned()), "{q:?}");
    assert!(
        !q.contains(&"TPB".to_owned()),
        "a disabled question is never sent: {q:?}"
    );
    assert_eq!(v.cause, VerdictCause::Fired);
    assert!(v.evaluation_id.is_some(), "the verdict is logged");
    assert_eq!(gate_rows(&x, "P1").await, 1);
}

#[tokio::test]
async fn p1_laya_down_hands_over_only_with_a_closed_question() {
    let x = setup_laya("down").await;
    add_question(&x, "TPO", "P1", "annotate", "open", &[]).await;
    let d = load(&x, DesignOf::Draft).await;
    let v = p1(&x.e.state, &x, &d).await;
    assert_eq!(v.cause, VerdictCause::Unavailable);
    // handover exactly when an ASKED P1 question fails closed
    let want = failed_verdict(&d, GatePoint::P1, None);
    assert_eq!(v.decision, want.decision, "{v:?}");
    assert_eq!(v.deciding, want.deciding);
    add_question(&x, "TPC", "P1", "annotate", "closed", &[]).await;
    let d = load(&x, DesignOf::Draft).await;
    let v = p1(&x.e.state, &x, &d).await;
    assert_eq!(v.decision, Decision::Handover);
    assert_eq!(v.handover_reason(), Some("gate_unavailable"));
    assert!(v.deciding.contains(&"TPC".to_owned()), "{v:?}");
}

#[tokio::test]
async fn p2_evaluated_per_call_with_args_and_only_covering_questions() {
    let x = setup_laya("ok").await;
    add_question(&x, "TPT", "P2", "handover", "open", &[OP]).await;
    let d = load(&x, DesignOf::Draft).await;
    let st = &x.e.state;
    let before = seen(&x).len();
    for id in ["A-1", "B-2"] {
        let c = call(OP, Some(ACTIVITY), json!({"order_id": id}));
        assert_eq!(
            ToolPoint.check(st, x.f.tenant, &d, &c, OP).await,
            Err(GATE_BLOCKED.to_owned()),
            "the P2 question fires: the call is refused"
        );
    }
    let bodies = seen(&x);
    assert_eq!(
        bodies.len(),
        before + 2,
        "one Laya call per CALL, never per turn"
    );
    for (b, id) in bodies[before..].iter().zip(["A-1", "B-2"]) {
        assert_eq!(b["state"]["op"]["key"], json!(OP));
        assert_eq!(b["state"]["op"]["args"]["order_id"], json!(id));
        assert!(asked(b).contains(&"TPT".to_owned()));
    }
    assert_eq!(gate_rows(&x, "P2").await, 2);
    // an operation the question does not cover: it is not asked
    let other = "knowledge.search";
    let _ = ToolPoint
        .check(
            st,
            x.f.tenant,
            &d,
            &call(other, Some(ACTIVITY), json!({})),
            other,
        )
        .await;
    for b in &seen(&x)[before + 2..] {
        assert!(!asked(b).contains(&"TPT".to_owned()), "{b}");
    }
}

#[tokio::test]
async fn r1_asks_laya_about_the_reply() {
    let x = setup_laya("ok").await;
    add_question(&x, "TRB", "R1", "block", "open", &[]).await;
    let d = load(&x, DesignOf::Draft).await;
    let jr = x.f.job_ref();
    let ops = vec![(OP.to_owned(), "ok".to_owned())];
    let v = ReplyPoint
        .check(
            &x.e.state,
            ReplyInput {
                tenant: x.f.tenant,
                job: &jr,
                env: Env::Test,
                conversation_id: "conv-r1",
                case_id: "CS-R1",
                reply: "Your parcel arrives tomorrow.",
                operations: &ops,
                design: &d,
            },
        )
        .await;
    assert_ne!(v.decision, Decision::Allow, "{v:?}");
    assert!(v.deciding.contains(&"TRB".to_owned()), "{v:?}");
    let b = seen(&x).last().cloned().unwrap();
    assert_eq!(b["state"]["reply"], json!("Your parcel arrives tomorrow."));
    assert!(asked(&b).contains(&"TRB".to_owned()));
    assert_eq!(gate_rows(&x, "R1").await, 1);
}

#[tokio::test]
async fn hook_db_error_fails_closed_only_when_a_closed_question_would_be_asked() {
    let x = setup().await;
    add_question(&x, "TOO", "P2", "handover", "open", &[OP]).await;
    let broken = broken_state(&x).await;
    let d = load(&x, DesignOf::Draft).await;
    let side_effect = d.catalog.get(OP).expect("order.read").side_effect;
    let ask = Some(ToolAsk::Call {
        op_key: OP,
        side_effect,
    });
    let c = call(OP, Some(ACTIVITY), json!({}));
    // no closed question covers the call: failed open (allowed)
    let want = failed_verdict(&d, GatePoint::P2, ask);
    let got = ToolPoint.check(&broken, x.f.tenant, &d, &c, OP).await;
    if want.deciding.is_empty() {
        assert_eq!(got, Ok(()), "open questions only: the call passes");
    } else {
        assert_eq!(got, Err("gate_unavailable".to_owned()));
    }
    // a closed question that covers it: failed closed (handover)
    add_question(&x, "TOC", "P2", "handover", "closed", &[OP]).await;
    let d = load(&x, DesignOf::Draft).await;
    let want = failed_verdict(&d, GatePoint::P2, ask);
    assert!(want.deciding.contains(&"TOC".to_owned()), "{want:?}");
    assert!(!want.deciding.contains(&"TOO".to_owned()), "{want:?}");
    assert_eq!(
        ToolPoint.check(&broken, x.f.tenant, &d, &c, OP).await,
        Err("gate_unavailable".to_owned())
    );
    // P1 the same way
    let v = p1(&broken, &x, &d).await;
    assert_eq!(v.cause, VerdictCause::Unavailable);
    assert_eq!(v.decision, failed_verdict(&d, GatePoint::P1, None).decision);
}

#[tokio::test]
async fn probe_writes_gate_rows_only_when_gates_are_bound() {
    for bound in [false, true] {
        let x = if bound {
            setup_laya("ok").await
        } else {
            setup().await
        };
        add_question(&x, "TPA", "P1", "annotate", "open", &[]).await;
        let d = load(&x, DesignOf::Draft).await;
        let _ = probe::probe_turn(
            &x.e.state,
            &x.f.job.t,
            &x.f.job_ref(),
            d.case.clone(),
            "Where is my parcel?",
        )
        .await;
        let rows = gate_rows(&x, "P1").await;
        if bound {
            assert_eq!(rows, 1, "a bound P1 is logged");
        } else {
            assert_eq!(rows, 0, "no gates provider: no gate, no row");
        }
    }
}

#[tokio::test]
async fn readiness_counts_gates_and_narrowings() {
    let x = setup().await;
    narrow(&x).await;
    let st = &x.e.state;
    let jr = x.f.job_ref();
    let mut tx = x.f.job.begin(&st.db).await.unwrap();
    let checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, ReadOf::Draft)
        .await
        .unwrap();
    let cx = checked.cx(&st.design, st.compiler.as_ref());
    let gates = GatesReadiness.assess(&mut tx, &cx).await.unwrap();
    let toolsets = ToolsetsReadiness.assess(&mut tx, &cx).await.unwrap();
    tx.commit().await.unwrap();
    assert!(gates.area.total > 0, "{:?}", gates.area);
    assert_eq!(gates.area.ready, 0, "nothing reviewed yet");
    assert_eq!(gates.go_live.len(), 1);
    assert_eq!(gates.go_live[0].cause.as_deref(), Some(GATES_UNREVIEWED));
    assert!(gates.blocking.is_empty());
    assert!(toolsets.area.total > 0);
    assert!(
        toolsets.area.summary.ends_with("1 narrowing by you"),
        "{}",
        toolsets.area.summary
    );
    assert!(readiness::registry().areas().contains(&AreaKey::Procedures));
    // the production board runs them (the draft holds a narrowing, no gate review)
    let ev = st
        .readiness
        .evaluate(
            &mut x.f.job.begin(&st.db).await.unwrap(),
            &st.design,
            st.compiler.as_ref(),
            &checked,
        )
        .await
        .unwrap();
    assert!(
        ev.board
            .gaps
            .iter()
            .any(|g| g.cause.as_deref() == Some(GATES_UNREVIEWED)),
        "{:?}",
        ev.board.gaps
    );
}
