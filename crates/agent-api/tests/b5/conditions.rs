//! B5 T7a — procedure step conditions, per-case cap, success criteria (F-15, F-20; migration 0147). Through the
//! HTTP router as `agent_app` on a Demo Retail job (as `procedures.rs`): `createProcedureVersion` validates and
//! stores the step-level `preconditions` / `postconditions` / `performed_by` / `max_per_case` (inside `steps`) and
//! the version-level `success_criteria` / `tool_requirements` / `pddl_domain`; conditions are checked with B4's
//! `runtime::conditions::validate`. `job_design::procedures` is what the runtime (T12) and readiness read.
use agent_api::design::DesignOf;
use agent_api::job_design::procedures as jd_procedures;
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::common::design_contract::{self, ContractJob, DESIGNER};
use crate::common::{self, TestEnv, get, insert_user, person, send};

/// The space of `procedures.rs`: its activity grant (R15 seed) covers `order.read`.
const SPACE: &str = "retail:logistics:track_parcel";
const PROC: &str = "b5_cancel_fixture";

struct Fx {
    e: TestEnv,
    f: ContractJob,
    designer: String,
    admin: String,
}

async fn setup() -> Fx {
    let e = common::env().await;
    let f = crate::b5::support::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    let admin = design_contract::release_manager(&e, &f).await;
    Fx {
        e,
        f,
        designer: person(DESIGNER, &[]),
        admin,
    }
}

fn uri(x: &Fx, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", x.f.job.job_key)
}

async fn post(x: &Fx, tail: &str, tok: &str, body: Value) -> (StatusCode, Value) {
    send(
        &x.e.app,
        common::json("POST", &uri(x, tail), Some(tok), body),
    )
    .await
}

#[track_caller]
fn ok(got: (StatusCode, Value)) -> Value {
    assert_eq!(got.0, StatusCode::OK, "{}", got.1);
    got.1
}

#[track_caller]
fn refused(got: (StatusCode, Value), code: &str) -> Value {
    assert_eq!(got.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", got.1);
    assert_eq!(got.1["error"]["code"], json!(code), "{}", got.1);
    got.1
}

fn step(name: &str, phase: u32, op: &str) -> Value {
    json!({
        "step_number": 1,
        "phase": phase,
        "name": name,
        "operation": op,
        "variant": null,
        "modality": "observe",
        "side_effect": "R",
        "identity_level": "0",
        "confirm_back": false,
        "gate": false,
        "args": {"order_id": "$order_id"},
    })
}

fn cond(id: &str, fact: &str, op: &str, value: Value) -> Value {
    if value.is_null() {
        json!({"id": id, "fact": fact, "op": op})
    } else {
        json!({"id": id, "fact": fact, "op": op, "value": value})
    }
}

/// A two-step version: `order` (order.read, phase 0) then `cancel` (order.cancel, X, phase 1) with conditions.
fn full_input(version: u32) -> Value {
    let mut cancel = step("cancel", 1, "order.cancel");
    cancel["preconditions"] = json!([
        cond("order_found", "steps.order.outcome", "eq", json!("ok")),
        cond("verified", "identity.level", "gte", json!(2)),
    ]);
    cancel["postconditions"] = json!([cond(
        "cancelled",
        "steps.cancel.out.status",
        "eq",
        json!("cancelled")
    )]);
    cancel["performed_by"] = json!("procedure");
    cancel["max_per_case"] = json!(3);
    json!({
        "name": PROC,
        "version": version,
        "context_space": SPACE,
        "description": "Cancel an order",
        "steps": [step("order", 0, "order.read"), cancel],
        "success_criteria": [cond("done", "steps.cancel.outcome", "eq", json!("ok"))],
        "tool_requirements": ["order.cancel", "order.read"],
        "pddl_domain": "(define (domain cancel))",
    })
}

async fn create(x: &Fx, body: Value) -> (StatusCode, Value) {
    post(x, "procedures", &x.designer, body).await
}

#[tokio::test]
async fn create_version_stores_conditions_and_success() {
    let x = setup().await;
    let v = ok(create(&x, full_input(1)).await);
    let steps = v["steps"].as_array().unwrap();
    assert_eq!(steps[0]["preconditions"], json!([]));
    assert_eq!(steps[0]["performed_by"], Value::Null);
    assert_eq!(steps[0]["max_per_case"], Value::Null);
    assert_eq!(steps[1]["side_effect"], json!("X"));
    assert_eq!(steps[1]["preconditions"][0]["id"], json!("order_found"));
    assert_eq!(steps[1]["preconditions"][1]["value"], json!(2));
    assert_eq!(steps[1]["postconditions"][0]["id"], json!("cancelled"));
    assert_eq!(steps[1]["performed_by"], json!("procedure"));
    assert_eq!(steps[1]["max_per_case"], json!(3));
    assert_eq!(
        v["success_criteria"],
        json!([{"id": "done", "fact": "steps.cancel.outcome", "op": "eq", "value": "ok"}])
    );
    assert_eq!(
        v["tool_requirements"],
        json!(["order.cancel", "order.read"])
    );
    assert_eq!(v["pddl_domain"], json!("(define (domain cancel))"));

    // stored: version-level columns (0147) and step-level fields inside `steps`
    let (success, tools, pddl, steps): (Value, Vec<String>, Option<String>, Value) =
        sqlx::query_as(
            "SELECT success_criteria, tool_requirements, pddl_domain, steps FROM procedure_version \
          WHERE job_id = $1 AND name = $2 AND version = 1",
        )
        .bind(x.f.job.job_id)
        .bind(PROC)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(success[0]["id"], json!("done"));
    assert_eq!(tools, ["order.cancel", "order.read"]);
    assert_eq!(pddl.as_deref(), Some("(define (domain cancel))"));
    assert_eq!(steps[1]["max_per_case"], json!(3));
    assert_eq!(
        steps[1]["preconditions"][0]["fact"],
        json!("steps.order.outcome")
    );

    // the design place carries them too (snapshot value)
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let rows = agent_api::design::values_of_kind(
        &mut tx,
        &x.e.state.design,
        &x.f.job_ref(),
        &DesignOf::Draft,
        "procedure",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let stored = jd_procedures::stored(&rows).unwrap();
    let v1 = &stored[PROC][0];
    assert_eq!(v1.success_criteria[0].id, "done");
    assert_eq!(v1.tool_requirements, ["order.cancel", "order.read"]);
    assert_eq!(v1.steps[1].max_per_case, Some(3));
}

#[tokio::test]
async fn condition_validated_with_b4_validator() {
    let x = setup().await;
    // unknown fact path on step 2's preconditions
    let mut i = full_input(1);
    i["steps"][1]["preconditions"] =
        json!([cond("weather", "weather.today", "exists", Value::Null)]);
    let b = refused(create(&x, i).await, "invalid_condition");
    assert_eq!(b["error"]["step"], json!(2), "{b}");
    assert_eq!(b["error"]["condition"], json!("weather"), "{b}");
    assert_eq!(b["error"]["list"], json!("preconditions"), "{b}");

    // a precondition reading its own step (it has not run yet): forward reference
    let mut i = full_input(1);
    i["steps"][1]["preconditions"] =
        json!([cond("self_ref", "steps.cancel.outcome", "eq", json!("ok"))]);
    let b = refused(create(&x, i).await, "invalid_condition");
    assert_eq!(b["error"]["condition"], json!("self_ref"), "{b}");

    // a step of the first phase reading a later step
    let mut i = full_input(1);
    i["steps"][0]["postconditions"] =
        json!([cond("later", "steps.cancel.outcome", "exists", Value::Null)]);
    let b = refused(create(&x, i).await, "invalid_condition");
    assert_eq!(
        (b["error"]["step"].clone(), b["error"]["list"].clone()),
        (json!(1), json!("postconditions"))
    );

    // operator / value mismatch in the success criteria: step null
    let mut i = full_input(1);
    i["success_criteria"] = json!([cond("bad", "customer.tier", "in", json!("gold"))]);
    let b = refused(create(&x, i).await, "invalid_condition");
    assert_eq!(b["error"]["step"], Value::Null, "{b}");
    assert_eq!(b["error"]["list"], json!("success_criteria"), "{b}");
    assert_eq!(b["error"]["condition"], json!("bad"), "{b}");

    // nothing was written
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM procedure_version WHERE job_id = $1")
        .bind(x.f.job.job_id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn max_per_case_only_on_x_and_p_steps() {
    let x = setup().await;
    // an R step
    let mut i = full_input(1);
    i["steps"][0]["max_per_case"] = json!(1);
    let b = refused(create(&x, i).await, "invalid_cap");
    assert_eq!(b["error"]["step"], json!(1), "{b}");
    // out of 1..=20 on the X step
    for cap in [0, 21] {
        let mut i = full_input(1);
        i["steps"][1]["max_per_case"] = json!(cap);
        let b = refused(create(&x, i).await, "invalid_cap");
        assert_eq!(b["error"]["step"], json!(2), "{b}");
    }
    let mut i = full_input(1);
    i["steps"][1]["max_per_case"] = json!(20);
    ok(create(&x, i).await);
}

#[tokio::test]
async fn performed_by_must_be_a_job_role() {
    let x = setup().await;
    let mut i = full_input(1);
    i["steps"][1]["performed_by"] = json!("pilot");
    let b = refused(create(&x, i).await, "unknown_role");
    assert_eq!(b["error"]["step"], json!(2), "{b}");
    let mut i = full_input(1);
    i["steps"][1]["performed_by"] = json!("assistant");
    ok(create(&x, i).await);
}

#[tokio::test]
async fn tool_requirements_must_be_catalogue_ops() {
    let x = setup().await;
    for bad in ["order.teleport", "order", "Order.read", "order.read{", ""] {
        let mut i = full_input(1);
        i["tool_requirements"] = json!([bad]);
        let b = refused(create(&x, i).await, "unknown_operation");
        assert_eq!(b["error"]["tool"], json!(bad), "{b}");
    }
    // duplicates collapse, order kept
    let mut i = full_input(1);
    i["tool_requirements"] = json!(["order.read", "order.cancel", "order.read"]);
    let v = ok(create(&x, i).await);
    assert_eq!(
        v["tool_requirements"],
        json!(["order.read", "order.cancel"])
    );
}

#[tokio::test]
async fn pddl_domain_stored_verbatim_not_parsed() {
    let x = setup().await;
    // not balanced, not PDDL at all: stored as given, whitespace included
    let text = "  (define (domain cancel)\n  ;; unbalanced (( on purpose\n";
    let mut i = full_input(1);
    i["pddl_domain"] = json!(text);
    let v = ok(create(&x, i).await);
    assert_eq!(v["pddl_domain"], json!(text));
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT pddl_domain FROM procedure_version WHERE job_id = $1 AND name = $2",
    )
    .bind(x.f.job.job_id)
    .bind(PROC)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(stored.as_deref(), Some(text));
    // over 64 KiB
    let mut i = full_input(2);
    i["pddl_domain"] = json!("x".repeat(64 * 1024 + 1));
    refused(create(&x, i).await, "invalid_pddl");
    // absent = null
    let mut i = full_input(2);
    i.as_object_mut().unwrap().remove("pddl_domain");
    let v = ok(create(&x, i).await);
    assert_eq!(v["pddl_domain"], Value::Null);
}

/// A version whose steps stay inside the space's grant (`order.read`), so it can be published.
fn publishable(version: u32) -> Value {
    let mut order = step("order", 0, "order.read");
    order["postconditions"] = json!([cond(
        "found",
        "steps.order.out.status",
        "exists",
        Value::Null
    )]);
    order["performed_by"] = json!("procedure");
    json!({
        "name": PROC,
        "version": version,
        "context_space": SPACE,
        "description": "Look an order up",
        "steps": [order],
        "success_criteria": [cond("done", "steps.order.outcome", "eq", json!("ok"))],
        "tool_requirements": ["order.read"],
    })
}

async fn publish(x: &Fx, v: u32) {
    for to in ["CANARY", "PUBLISHED"] {
        ok(post(
            x,
            &format!("procedures/{PROC}/versions/{v}/status"),
            &x.admin,
            json!({ "to": to }),
        )
        .await);
    }
}

#[tokio::test]
async fn published_version_fields_reach_runtime_design() {
    let x = setup().await;
    ok(create(&x, publishable(1)).await);
    ok(create(&x, full_input(2)).await); // a DRAFT: never reaches the runtime
    publish(&x, 1).await;
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let procs =
        jd_procedures::published(&mut tx, &x.e.state.design, &x.f.job_ref(), &DesignOf::Draft)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(procs.len(), 1, "{procs:?}");
    let p = &procs[0];
    assert_eq!((p.name.as_str(), p.version, p.steps), (PROC, 1, 1));
    assert_eq!(p.label, "Look an order up");
    assert_eq!(p.success_criteria[0].id, "done");
    assert_eq!(p.tool_requirements, ["order.read"]);
    let s = &p.step_defs[0];
    assert_eq!(s.postconditions[0].id, "found");
    assert_eq!(s.performed_by.as_deref(), Some("procedure"));
}

#[tokio::test]
async fn readiness_warns_tool_requirement_unrouted() {
    let x = setup().await;
    ok(create(&x, publishable(1)).await);
    publish(&x, 1).await;
    // `support::new_job` routes every operation the activities name on Production (the R5 publish gate), and the
    // fixture space's activity names `order.read`: take that route away so the requirement really is unrouted.
    sqlx::query(
        "DELETE FROM operation_route WHERE tenant_id = $1 AND job_id = $2 AND object = 'order' AND verb = 'read' \
            AND env = 'prod'",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .execute(&x.e.owner)
    .await
    .unwrap();
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let gaps =
        jd_procedures::tool_unrouted(&mut tx, &x.e.state.design, &x.f.job_ref(), &DesignOf::Draft)
            .await
            .unwrap();
    tx.commit().await.unwrap();
    // a fresh job routes nothing in Production: order.read would be handed over before the first step
    assert_eq!(gaps.len(), 1, "{gaps:?}");
    let g = &gaps[0];
    assert_eq!(
        g.key,
        format!("procedure_tool_unrouted:{PROC}@1:order.read")
    );
    assert_eq!(g.cause.as_deref(), Some(jd_procedures::TOOL_UNROUTED));
    assert_eq!(
        serde_json::to_value(g).unwrap()["blocks"],
        json!("go_live"),
        "a warning, never an apply blocker"
    );
}

#[tokio::test]
async fn old_versions_without_fields_load_with_defaults() {
    let x = setup().await;
    // a row as T7 wrote it: no 0147 columns named, steps without the T7a fields
    sqlx::query(
        "INSERT INTO procedure_version (tenant_id, job_id, name, version, status, context_space, description, \
                                        steps, created_by) \
         VALUES ($1, $2, $3, 1, 'DRAFT', $4, 'Old', $5, 'u-designer')",
    )
    .bind(x.f.tenant)
    .bind(x.f.job.job_id)
    .bind(PROC)
    .bind(SPACE)
    .bind(json!([{
        "step_number": 1, "phase": 0, "name": "order", "operation": "order.read", "variant": null,
        "modality": "observe", "side_effect": "R", "identity_level": "2", "confirm_back": false, "gate": false,
        "args": {}
    }]))
    .execute(&x.e.owner)
    .await
    .unwrap();
    let b = ok(send(
        &x.e.app,
        get(
            &uri(&x, &format!("procedures/{PROC}")),
            Some(x.designer.as_str()),
        ),
    )
    .await);
    let v = &b[0];
    assert_eq!(v["success_criteria"], json!([]));
    assert_eq!(v["tool_requirements"], json!([]));
    assert_eq!(v["pddl_domain"], Value::Null);
    assert_eq!(v["steps"][0]["preconditions"], json!([]));
    assert_eq!(v["steps"][0]["postconditions"], json!([]));
    assert_eq!(v["steps"][0]["max_per_case"], Value::Null);

    // a snapshot value written before T7a restores with the defaults (target `set` → `get`)
    let t = x.e.state.design.target("procedure").unwrap();
    let mut r = agent_api::design::DraftRef::new();
    r.insert("name".into(), json!(PROC));
    let jr = x.f.job_ref();
    let at = agent_api::design::Place {
        tenant: x.f.tenant,
        job: Some(&jr),
    };
    let old = Some(
        json!([{"version": 2, "status": "DRAFT", "context_space": SPACE, "description": "Old",
                           "reviewers": [], "steps": [], "released_in": null}]),
    );
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    t.set(&mut tx, at, &r, &old).await.unwrap();
    let got = t.get(&mut tx, at, &r).await.unwrap().unwrap();
    tx.commit().await.unwrap();
    assert_eq!(got[0]["success_criteria"], json!([]));
    assert_eq!(got[0]["tool_requirements"], json!([]));
    assert_eq!(got[0]["pddl_domain"], Value::Null);
    // the projection treats "absent" and the defaults as the same value (no phantom change)
    let mut with_defaults = old.clone().unwrap();
    with_defaults[0]["success_criteria"] = json!([]);
    with_defaults[0]["tool_requirements"] = json!([]);
    with_defaults[0]["pddl_domain"] = Value::Null;
    assert_eq!(t.proj(&old), t.proj(&Some(with_defaults)));
}
