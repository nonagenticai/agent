//! B4 over B5's procedures (`runtime::orchestrator::DesignProcedures`, acceptance A7 / A13 / A16): a PUBLISHED
//! tenant version runs its own steps and checks, read through `RuntimeDesign::load` (Draft in test, Live in prod),
//! ONLY for the space it was published under; a CANARY version never runs; a space without a designed procedure
//! plans exactly what `StaticProcedures` plans; a tenant version named like a builtin but published under another
//! space (or none) never runs for this one — the builtin's steps do, under the builtin's version; a designed step
//! outside the request space's activity grant, or one that does not map, refuses the procedure at plan time. Real
//! Postgres 18, a Demo Retail job created through `POST /api/jobs`; the versions are written through the
//! `procedure` design target (so a step outside the grant can be stored: the HTTP status route would refuse to
//! publish it, and this file tests that the plan refuses it too).
use agent_api::design::{DesignOf, DraftRef};
use agent_api::runtime::design::RuntimeDesign;
use agent_api::runtime::domain::QualifiedRequest;
use agent_api::runtime::orchestrator::{DesignProcedures, ProcedureSource, StaticProcedures};
use serde_json::{Value, json};

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv};

/// The space the designed procedure serves, in both the Draft design and the released one (`publish` cuts with the
/// order-support-only selection, so a logistics or payments space has no activity in Live and every designed step
/// in it is refused). The seed (R15) maps it to `purchase:order_status`, which grants `order.read` and
/// `delivery.read`; its builtin (`order_status`, one `order.read`) is not what the designed procedure plans.
const S: &str = "retail:order-support:order_status";
/// The designed second step: inside S's grant, outside RETURNS'.
const TRACK: &str = "delivery.read";
/// A space with a builtin (`return_request`): `support:return_initiation`, which grants `order.read` and
/// `case.create{return}` (not `payment.create` / `payment.refund`).
const RETURNS: &str = "retail:order-support:returns";
const REFUND: &str = "refund_fix";

struct Fx {
    e: TestEnv,
    f: ContractJob,
}

async fn setup() -> Fx {
    let e = common::env().await;
    let f = crate::b5::support::new_job(&e).await;
    Fx { e, f }
}

async fn load(x: &Fx, of: DesignOf) -> RuntimeDesign {
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let d = RuntimeDesign::load(&mut tx, &x.e.state.design, &x.f.job_ref(), &of)
        .await
        .unwrap_or_else(|e| panic!("load {of:?}: {e:?}"));
    tx.commit().await.unwrap();
    d
}

fn step(number: u32, phase: u32, name: &str, op: &str, effect: &str) -> Value {
    json!({
        "step_number": number, "phase": phase, "name": name, "operation": op, "variant": null,
        "modality": if effect == "R" { "observe" } else { "do" }, "side_effect": effect,
        "identity_level": "2", "confirm_back": false, "gate": false, "args": {"order_id": "$order_id"},
    })
}

/// order.read (phase 0), then delivery.read (phase 1, capped at two per case): inside S's grant.
fn refund_steps() -> Value {
    let mut refund = step(2, 1, "refund", "delivery.read", "R");
    refund["max_per_case"] = json!(2);
    json!([step(1, 0, "order", "order.read", "R"), refund])
}

/// order.read (phase 0), then case.create{return} (W, phase 1): inside RETURNS' grant.
fn return_steps() -> Value {
    let mut case = step(2, 1, "case", "case.create", "W");
    case["variant"] = json!("return");
    json!([step(1, 0, "order", "order.read", "R"), case])
}

fn version(v: u32, status: &str, space: &str, steps: Value, checks: bool) -> Value {
    let (criteria, tools) = if checks {
        (
            json!([{"id": "refunded", "fact": "steps.refund.outcome", "op": "eq", "value": "ok"}]),
            json!([TRACK]),
        )
    } else {
        (json!([]), json!([]))
    };
    json!({
        "version": v, "status": status, "context_space": space, "description": "Refund a payment",
        "reviewers": [], "steps": steps, "released_in": null,
        "success_criteria": criteria, "tool_requirements": tools, "pddl_domain": null,
    })
}

/// The tenant versions of procedure `name` become `versions` (a tracked draft write).
async fn put(x: &Fx, name: &str, versions: Value) {
    let mut r = DraftRef::new();
    r.insert("name".into(), json!(name));
    design_contract::tracked_set(&x.e, &x.f, "procedure", &r, Some(versions)).await;
}

fn request(space: &str) -> QualifiedRequest {
    serde_json::from_value(json!({
        "case_id": "CS-DP", "conversation_id": "conv-dp", "area": space.split(':').nth(1).unwrap(),
        "context_space": space, "request_type": space.rsplit(':').next().unwrap(),
        "slots": {"order_id": "ORD-1"}, "identity_level": 2, "message_id": "m-dp"
    }))
    .unwrap()
}

/// `name@version` and the op keys of each phase.
fn shape(d: &RuntimeDesign, space: &str) -> (Option<String>, Vec<Vec<String>>) {
    let (p, phases) = DesignProcedures
        .plan(d, &request(space))
        .unwrap_or_else(|| panic!("{space} has a plan"));
    (
        p.map(|p| p.label()),
        phases
            .iter()
            .map(|ph| ph.steps.iter().map(|s| s.op_key()).collect())
            .collect(),
    )
}

#[track_caller]
fn assert_designed_refund(d: &RuntimeDesign, of: &str) {
    assert_eq!(
        shape(d, S),
        (
            Some(format!("{REFUND}@1")),
            vec![vec!["order.read".to_owned()], vec![TRACK.to_owned()]]
        ),
        "{of}: the designed steps, grouped by phase"
    );
    let (_, phases) = DesignProcedures.plan(d, &request(S)).unwrap();
    let refund = &phases[1].steps[0];
    assert_eq!(
        (refund.side_effect.as_deref(), refund.max_per_case),
        (Some("R"), Some(2)),
        "{of}"
    );
    assert_eq!(refund.args["order_id"], json!("$order_id"), "{of}");
    let checks = DesignProcedures.checks(d, &request(S));
    assert_eq!(checks.tool_requirements, [TRACK], "{of}");
    assert_eq!(
        checks
            .success_criteria
            .iter()
            .map(|c| c.id.as_str())
            .collect::<Vec<_>>(),
        ["refunded"],
        "{of}"
    );
}

/// A PUBLISHED version in S plans its own steps and checks in Draft and, once released, in Live; a CANARY version
/// in S (first by name) and a second PUBLISHED name (later by name) are not chosen. Mutation: `StaticProcedures`
/// plans the builtin `order_status` for S, so without `DesignProcedures` the plan is never refund_fix.
#[tokio::test]
async fn a_published_designed_procedure_plans_its_own_steps_and_checks_in_test_and_prod() {
    let x = setup().await;
    put(
        &x,
        REFUND,
        json!([version(1, "PUBLISHED", S, refund_steps(), true)]),
    )
    .await;
    put(
        &x,
        "refund_canary",
        json!([version(1, "CANARY", S, refund_steps(), false)]),
    )
    .await;
    put(
        &x,
        "zz_refund",
        json!([version(1, "PUBLISHED", S, refund_steps(), false)]),
    )
    .await;

    let draft = load(&x, DesignOf::Draft).await;
    assert!(
        !draft.procedures.iter().any(|p| p.name == "refund_canary"),
        "a CANARY version never reaches the runtime"
    );
    let p = draft.procedures.iter().find(|p| p.name == REFUND).unwrap();
    assert_eq!(
        p.context_space.as_deref(),
        Some(S),
        "the space reaches the runtime"
    );
    assert_designed_refund(&draft, "Draft");
    assert_ne!(
        StaticProcedures
            .plan(&draft, &request(S))
            .and_then(|(p, _)| p)
            .map(|p| p.name),
        Some(REFUND.to_owned()),
        "the control: the static source does not know it"
    );

    // Production reads the live design: nothing designed until the release, then the same plan
    let live = load(&x, DesignOf::Live).await;
    assert_ne!(shape_name(&live, S), Some(format!("{REFUND}@1")));
    crate::b5::support::publish(&x.e, &x.f).await;
    let live = load(&x, DesignOf::Live).await;
    assert_designed_refund(&live, "Live");
}

/// A tenant's PUBLISHED procedure with a `knowledge.answer` step in S, a space whose activity grants only `order.read`
/// and `delivery.read` (a tenant admin cannot edit an activity's grant): the read-only knowledge builtins are granted
/// implicitly, as the static knowledge plan's step is. The plan keeps the step (not refused `not_entitled`), binds the
/// request's message and the space's scope, and the runtime toolset (step 5b) lets the procedure role run it; a
/// write-effect operation outside the grant is still refused, and a tenant narrowing still removes the builtin.
#[tokio::test]
async fn an_authored_knowledge_step_needs_no_activity_grant() {
    use agent_api::job_design::toolset::DesignToolsets;
    use agent_api::runtime::assistant::tools::ToolsetIndex;
    use agent_api::runtime::spaces::SpaceCatalog;
    use agent_model::compile::FoldedKind;
    let x = setup().await;
    let mut know = step(1, 0, "ask", "knowledge.answer", "R");
    know["args"] = json!({});
    know["identity_level"] = json!("0");
    put(
        &x,
        "ask_knowledge",
        json!([version(1, "PUBLISHED", S, json!([know]), false)]),
    )
    .await;
    let mut d = load(&x, DesignOf::Draft).await;
    let granted = DesignToolsets.activity_grant(&d, &d.spaces.by_key(S).expect("S").activity);
    // The shared activity grants knowledge.search; this procedure uses knowledge.answer.
    assert!(
        !granted.iter().any(|g| g == "knowledge.answer"),
        "the control: S's activity does not grant the authored knowledge.answer step: {granted:?}"
    );
    // S routes knowledge kinds (the fixture's S lists none): its activity and department stay
    let mut spaces: Vec<_> = d.spaces.spaces().to_vec();
    spaces
        .iter_mut()
        .filter(|s| s.key == S)
        .for_each(|s| s.knowledge_kinds = vec!["faq.k".into()]);
    d.spaces = SpaceCatalog::new(spaces);
    d.knowledge.kinds = vec![FoldedKind {
        stage: "support".into(),
        kind: "faq.k".into(),
        label: "faq.k".into(),
        description: String::new(),
        store_key: None,
        required: false,
        tenant_data: false,
        layer: "test".into(),
        set_by: "test".into(),
        departments: Vec::new(),
        row_id: "row-faq.k".into(),
    }];
    let (p, phases) = DesignProcedures
        .plan(&d, &request(S))
        .expect("the knowledge procedure is planned, not refused");
    assert_eq!(p.map(|p| p.label()).as_deref(), Some("ask_knowledge@1"));
    let args = &phases[0].steps[0].args;
    assert_eq!(args["message_id"], json!("m-dp"));
    assert_eq!(
        args["scope"],
        json!([{"stage": "support", "kind": "faq.k"}])
    );
    let activity = d.spaces.by_key(S).unwrap().activity.clone();
    assert!(
        DesignToolsets.allows(&d, "procedure", Some(&activity), "knowledge.answer"),
        "step 5b grants the procedure role the read-only builtin"
    );
    assert!(
        !DesignToolsets.allows(&d, "procedure", Some(&activity), "payment.refund"),
        "a write outside the grant stays refused"
    );
}

fn shape_name(d: &RuntimeDesign, space: &str) -> Option<String> {
    DesignProcedures
        .plan(d, &request(space))
        .and_then(|(p, _)| p)
        .map(|p| p.label())
}

/// A space the tenant has no designed procedure for plans what it planned before: the template's procedure. Since
/// seed v9 RETURNS has a seeded, designed base procedure (`return_request@3`, step bodies and its own checks), so it
/// no longer equals `StaticProcedures` (the builtin) and this test asserts the intent with the seed as it stands:
/// publishing a tenant row for ANOTHER procedure (in S) changes nothing RETURNS plans, checks or refuses.
#[tokio::test]
async fn a_space_without_a_designed_procedure_falls_back_to_the_builtin() {
    let x = setup().await;
    let req = request(RETURNS);
    let before = load(&x, DesignOf::Draft).await;
    let plan_before = DesignProcedures
        .plan(&before, &req)
        .expect("returns has a plan");
    assert_eq!(
        plan_before.0.as_ref().map(|p| p.name.as_str()),
        Some("return_request"),
        "the template's procedure"
    );
    assert_eq!(plan_before.1.len(), 3, "its three phases");
    let checks_before = DesignProcedures.checks(&before, &req);
    put(
        &x,
        REFUND,
        json!([version(1, "PUBLISHED", S, refund_steps(), true)]),
    )
    .await;
    let d = load(&x, DesignOf::Draft).await;
    assert_eq!(
        shape(&d, S).0.as_deref(),
        Some("refund_fix@1"),
        "the tenant row serves its own space"
    );
    assert_eq!(
        DesignProcedures.plan(&d, &req).expect("returns has a plan"),
        plan_before,
        "RETURNS plans the template's procedure, unchanged by a row for another procedure"
    );
    assert_eq!(DesignProcedures.checks(&d, &req), checks_before);
    assert_eq!(DesignProcedures.refusal(&d, &req), None);
}

/// `name@version` of the template's `return_request` (the builtin RETURNS maps to), read before any tenant
/// version shadows it.
async fn builtin_return(x: &Fx) -> String {
    let d = load(x, DesignOf::Draft).await;
    let v = d
        .procedures
        .iter()
        .filter(|p| p.name == "return_request")
        .map(|p| p.version)
        .max()
        .expect("the template declares return_request");
    format!("return_request@{v}")
}

/// The version a tenant `return_request` takes in these tests: one above the template's, so it always shadows it.
fn tenant_version(builtin: &str) -> u32 {
    builtin
        .rsplit('@')
        .next()
        .and_then(|v| v.parse::<u32>().ok())
        .expect("name@version")
        + 1
}

/// What RETURNS plans when the builtin runs: the builtin's version and its three phases, as the static source.
#[track_caller]
fn assert_builtin_return(d: &RuntimeDesign, builtin: &str, why: &str) {
    let req = request(RETURNS);
    let (p, phases) = DesignProcedures
        .plan(d, &req)
        .unwrap_or_else(|| panic!("{why}: returns has a plan"));
    assert_eq!(
        p.map(|p| p.label()).as_deref(),
        Some(builtin),
        "{why}: the builtin's version is recorded, never the tenant version"
    );
    assert_eq!(phases.len(), 3, "{why}: the builtin's three phases");
    assert_eq!(
        StaticProcedures
            .plan(d, &req)
            .map(|(p, _)| p.map(|p| p.label())),
        Some(Some(builtin.to_owned())),
        "{why}: the static source records the builtin's version for the builtin's steps"
    );
    // a tenant row shadows the template's return_request, so a space it does not serve runs the builtin whole: the
    // builtin's steps and the builtin's checks, never the tenant version's
    assert_eq!(
        DesignProcedures.checks(d, &req),
        StaticProcedures.checks(d, &req),
        "{why}"
    );
    assert_eq!(DesignProcedures.refusal(d, &req), None, "{why}");
}

/// A tenant version named like a builtin (`return_request`, shadowing the base) runs its DESIGNED steps and records
/// its own version only when it was published under the request's space (rule 1). Published with no space (stored
/// ""), it never runs: the builtin's steps run under the builtin's version. Mutation: the old name-only rule 2 ran
/// the designed steps for the empty space too; the old static source recorded the tenant version for the builtin's
/// steps.
#[tokio::test]
async fn a_designed_procedure_named_like_a_builtin_runs_only_in_its_own_space() {
    let x = setup().await;
    let builtin = builtin_return(&x).await;
    let tv = tenant_version(&builtin);
    let tenant = format!("return_request@{tv}");
    assert_ne!(builtin, tenant, "the fixture's tenant version is new");

    // stored "": the builtin
    put(
        &x,
        "return_request",
        json!([version(tv, "PUBLISHED", "", return_steps(), false)]),
    )
    .await;
    let d = load(&x, DesignOf::Draft).await;
    assert_builtin_return(&d, &builtin, "no stored space");

    // stored RETURNS: its designed steps, its own version
    put(
        &x,
        "return_request",
        json!([version(tv, "PUBLISHED", RETURNS, return_steps(), false)]),
    )
    .await;
    let d = load(&x, DesignOf::Draft).await;
    assert_eq!(
        shape(&d, RETURNS),
        (
            Some(tenant),
            vec![
                vec!["order.read".to_owned()],
                vec!["case.create{return}".to_owned()]
            ]
        ),
        "the same space: the designed steps"
    );
    // no designed checks: the builtin's (none for return_request)
    assert_eq!(
        DesignProcedures.checks(&d, &request(RETURNS)),
        StaticProcedures.checks(&d, &request(RETURNS))
    );
}

/// Audit of agent#79: a tenant procedure with a builtin's name, published under space A (S, whose grant its steps
/// pass), is NOT used for a request in space B (RETURNS) whose last segment maps to that builtin — publish checked
/// its steps against A's grant only. B runs the builtin's steps and records the builtin's version; A still runs the
/// designed steps. Mutation: the old name-only rule 2 planned `delivery.read` for RETURNS under the tenant version.
#[tokio::test]
async fn a_builtin_named_procedure_published_in_another_space_does_not_run_in_this_one() {
    let x = setup().await;
    let builtin = builtin_return(&x).await;
    let tv = tenant_version(&builtin);
    put(
        &x,
        "return_request",
        json!([version(tv, "PUBLISHED", S, refund_steps(), false)]),
    )
    .await;
    let d = load(&x, DesignOf::Draft).await;
    assert_builtin_return(&d, &builtin, "published under another space");
    let (_, phases) = DesignProcedures.plan(&d, &request(RETURNS)).unwrap();
    assert!(
        phases
            .iter()
            .flat_map(|p| &p.steps)
            .all(|s| s.op_key() != TRACK),
        "no designed step of space A runs in B: {phases:?}"
    );
    assert_eq!(
        shape(&d, S),
        (
            Some(format!("return_request@{tv}")),
            vec![vec!["order.read".to_owned()], vec![TRACK.to_owned()]]
        ),
        "its own space still runs it"
    );
}

/// Defence in depth: a designed procedure published under RETURNS (stored straight through the design target)
/// whose step `payment.refund` is outside RETURNS' activity grant is refused at plan time — no plan (not the
/// builtin's either), and the refusal names `not_entitled` so the case hands over (never `out_of_scope`).
/// Mutation: without the plan-time grant check rule 1 plans the designed steps.
#[tokio::test]
async fn a_designed_step_outside_the_request_space_grant_is_refused_at_plan_time() {
    let x = setup().await;
    let mut refund = step(2, 1, "refund", "payment.refund", "X");
    refund["max_per_case"] = json!(1);
    put(
        &x,
        REFUND,
        json!([version(
            1,
            "PUBLISHED",
            RETURNS,
            json!([step(1, 0, "order", "order.read", "R"), refund]),
            true
        )]),
    )
    .await;
    let d = load(&x, DesignOf::Draft).await;
    assert!(
        d.procedures
            .iter()
            .any(|p| p.name == REFUND && p.context_space.as_deref() == Some(RETURNS)),
        "the version reaches the runtime"
    );
    let req = request(RETURNS);
    assert_eq!(DesignProcedures.plan(&d, &req), None, "refused, not run");
    assert_eq!(DesignProcedures.refusal(&d, &req), Some("not_entitled"));
    assert!(
        StaticProcedures.plan(&d, &req).is_some(),
        "the control: the builtin exists, and is not what a refusal runs"
    );
}

/// Fail closed: a designed procedure whose stored space equals the request space, but which the design's
/// SpaceCatalog does not know (so there is no activity and nothing is granted), is refused `not_entitled` —
/// no plan, no op call. Its only step is `order.read`, which the seeded spaces do grant, so the sole reason
/// for the refusal is the empty grant. Mutation: if `outside_grant`'s `Err` arm returned `Ok`/allowed, rule 1
/// would plan the designed steps and both assertions would fail.
#[tokio::test]
async fn a_designed_procedure_in_a_space_with_no_activity_grant_is_refused_at_plan_time() {
    const GHOST: &str = "retail:order-support:ghost_space";
    let x = setup().await;
    put(
        &x,
        REFUND,
        json!([version(
            1,
            "PUBLISHED",
            GHOST,
            json!([step(1, 0, "order", "order.read", "R")]),
            false
        )]),
    )
    .await;
    let d = load(&x, DesignOf::Draft).await;
    assert!(
        d.procedures
            .iter()
            .any(|p| p.name == REFUND && p.context_space.as_deref() == Some(GHOST)),
        "the version reaches the runtime"
    );
    assert!(
        d.spaces.by_key(GHOST).is_none(),
        "the space has no activity"
    );
    let req = request(GHOST);
    assert_eq!(DesignProcedures.plan(&d, &req), None, "refused, not run");
    assert_eq!(DesignProcedures.refusal(&d, &req), Some("not_entitled"));
}

/// A designed procedure whose step cap does not fit the runner (`max_per_case` 256, written past the API's cap of
/// 20) is refused at plan level: no plan — not the builtin's steps under its name — and a refusal
/// (`not_offered`), so the case hands over.
#[tokio::test]
async fn a_designed_procedure_whose_cap_overflows_is_refused_not_replaced_by_the_builtin() {
    let x = setup().await;
    let mut case = step(2, 1, "case", "case.create", "W");
    case["variant"] = json!("return");
    case["max_per_case"] = json!(256);
    put(
        &x,
        "return_fix",
        json!([version(
            1,
            "PUBLISHED",
            RETURNS,
            json!([step(1, 0, "order", "order.read", "R"), case]),
            false
        )]),
    )
    .await;
    let d = load(&x, DesignOf::Draft).await;
    let req = request(RETURNS);
    assert_eq!(DesignProcedures.plan(&d, &req), None);
    assert_eq!(DesignProcedures.refusal(&d, &req), Some("not_offered"));
    assert!(StaticProcedures.plan(&d, &req).is_some(), "the control");
}
