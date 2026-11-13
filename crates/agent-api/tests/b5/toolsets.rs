//! B5 T11 — J8 toolsets (plan "Route table": `toolsetMeta`, `narrowToolsetGrant`, `restoreToolsetGrant`,
//! `toolsetOverview`) and T11a — the `assistant` role per job (`setAgentRoleEnabled`, kind `role_setting`, F-40),
//! plus the one toolset source (B4 owner ruling 5): `job_design::toolset::DesignToolsets` is what the assistant's
//! offer and the runner's `ToolGate` (T12) consult. Real Postgres 18, a Demo Retail job created through
//! `POST /api/jobs`, through the HTTP router as `agent_app`. The draft keeps the template's default selection (every
//! area), so `delivery:where_is_my_order` (whose operations seed v8 carries) is in it.
//!
//! The pinned platform row of `assistant` is `enabled: false` (seed v1, K50), so a job cannot switch it on here
//! (409 `role_disabled_on_platform`); the enabled path (grants − narrowings, the switch, the budget) is covered by
//! the unit tests of `job_design::toolset` over hand-built bases.
use std::collections::BTreeSet;

use agent_api::design::DesignOf;
use agent_api::job_design::role_switch::JobRoleSwitch;
use agent_api::job_design::runtime::JobDesign;
use agent_api::job_design::toolset::{
    DesignToolsets, REASON_DISABLED_ON_PLATFORM, REASON_NARROWED,
};
use agent_api::runtime::assistant::tools::{ToolsetIndex, offered};
use agent_api::runtime::assistant::{MAX_TOOL_CALLS, RoleSwitch};
use agent_api::runtime::catalog::OperationCatalog;
use agent_api::runtime::design::RuntimeDesign;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::common::design_contract::{self, ContractJob, DESIGNER};
use crate::common::{self, TestEnv, get, insert_user, person, send};

/// An activity of the Demo Retail bundle the seed knows the operations of.
const ACTIVITY: &str = "delivery:where_is_my_order";
/// One of its operations (side effect R: inside every role's max).
const OP: &str = "order.read";

struct Fx {
    e: TestEnv,
    f: ContractJob,
    /// `job_designer` only.
    designer: String,
}

async fn setup() -> Fx {
    let e = common::env().await;
    let f = crate::b5::support::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    Fx {
        e,
        f,
        designer: person(DESIGNER, &[]),
    }
}

fn uri(x: &Fx, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", x.f.job.job_key)
}

async fn read(x: &Fx, tail: &str) -> (StatusCode, Value) {
    send(&x.e.app, get(&uri(x, tail), Some(&x.designer))).await
}

async fn write(x: &Fx, method: &str, tail: &str, body: Value) -> (StatusCode, Value) {
    send(
        &x.e.app,
        common::json(method, &uri(x, tail), Some(&x.designer), body),
    )
    .await
}

#[track_caller]
fn ok(got: (StatusCode, Value)) -> Value {
    assert_eq!(got.0, StatusCode::OK, "{}", got.1);
    got.1
}

#[track_caller]
fn refused(got: (StatusCode, Value), status: StatusCode, code: &str) -> Value {
    assert_eq!(got.0, status, "{}", got.1);
    assert_eq!(got.1["error"]["code"], json!(code), "{}", got.1);
    got.1
}

/// `(key, kind, page, label)` of every ledger row of the job.
async fn ledger(x: &Fx) -> Vec<(String, String, String, String)> {
    sqlx::query_as("SELECT key, kind, page, label FROM draft_ledger WHERE job_id = $1 ORDER BY key")
        .bind(x.f.job.job_id)
        .fetch_all(&x.e.owner)
        .await
        .unwrap()
}

async fn narrow(x: &Fx, role: &str, activity: &str, op: &str) -> (StatusCode, Value) {
    write(
        x,
        "POST",
        &format!("toolsets/{role}/narrowings"),
        json!({"activity": activity, "op": op}),
    )
    .await
}

async fn restore(x: &Fx, role: &str, activity: &str, op: &str) -> (StatusCode, Value) {
    write(
        x,
        "DELETE",
        &format!("toolsets/{role}/narrowings"),
        json!({"activity": activity, "op": op}),
    )
    .await
}

fn row<'a>(toolset: &'a Value, label: &str) -> &'a Value {
    toolset["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|r| {
            let op = r["op"].as_str().unwrap();
            match r["variant"].as_str() {
                Some(v) => format!("{op}{{{v}}}") == label,
                None => op == label,
            }
        })
        .unwrap_or_else(|| panic!("no row {label} in {toolset}"))
}

/// The toolset of `role` × `activity` in an overview.
fn toolset_in<'a>(overview: &'a Value, role: &str, activity: &str) -> &'a Value {
    overview["toolsets"]
        .as_array()
        .expect("toolsets")
        .iter()
        .find(|t| t["role"] == json!(role) && t["activity"] == json!(activity))
        .unwrap_or_else(|| panic!("no toolset {role} × {activity}"))
}

/// The job's runtime design `of` with its job design read (what T12's `RuntimeDesign::read` will fill).
async fn runtime_design(x: &Fx, of: DesignOf) -> RuntimeDesign {
    let jr = x.f.job_ref();
    let mut tx = x.f.job.begin(&x.e.state.db).await.unwrap();
    let mut d = RuntimeDesign::load(&mut tx, &x.e.state.design, &jr, &of)
        .await
        .unwrap_or_else(|e| panic!("load: {e:?}"));
    let jd = JobDesign::load(
        &mut tx,
        &x.e.state.design,
        x.e.state.compiler.as_ref(),
        &jr,
        &of,
    )
    .await
    .unwrap_or_else(|e| panic!("job design: {e:?}"));
    tx.commit().await.unwrap();
    d.job_design = Arc::new(jd);
    d
}

// ---- T11 ------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn toolset_meta_lists_roles_activities_and_narrowings() {
    let x = setup().await;
    let m = ok(read(&x, "toolsets").await);
    let roles: Vec<&str> = m["roles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["role"].as_str().unwrap())
        .collect();
    for r in ["procedure", "qualifier", "post_gate", "assistant"] {
        assert!(roles.contains(&r), "{r} in {roles:?}");
    }
    let assistant = m["roles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["enabled"], false);
    assert_eq!(assistant["enabled_platform"], false);
    assert_eq!(assistant["enabled_job"], false);
    assert_eq!(assistant["configurable"], true);
    assert_eq!(assistant["max_tool_calls"], Value::Null);
    let procedure = m["roles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "procedure")
        .unwrap();
    assert_eq!(procedure["enabled"], true);
    assert_eq!(procedure["configurable"], false);
    assert!(
        m["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == ACTIVITY && a["origin"] == "base"),
        "{}",
        m["activities"]
    );
    assert_eq!(m["narrowings"], json!([]));
}

#[tokio::test]
async fn procedure_role_grant_is_activity_ops() {
    let x = setup().await;
    let o = ok(read(&x, "toolsets/overview?env=test").await);
    let t = toolset_in(&o, "procedure", ACTIVITY);
    assert_eq!(t["role_enabled"], true);
    let labels: BTreeSet<String> = t["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            assert!(
                r["expected"].is_string(),
                "derived rows are all expected: {r}"
            );
            assert_eq!(r["granted"], true, "{r}");
            match r["variant"].as_str() {
                Some(v) => format!("{}{{{v}}}", r["op"].as_str().unwrap()),
                None => r["op"].as_str().unwrap().to_owned(),
            }
        })
        .collect();
    let want: BTreeSet<String> = [
        "delivery.read{by_tracking_ref}",
        "order.read",
        "knowledge.search",
        "delivery.read",
        // seed v9: the optional grant of the parcels-by-party read
        "delivery.list",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert_eq!(labels, want);
    // the assistant is off on the platform (seed v1): every row says so
    let a = toolset_in(&o, "assistant", ACTIVITY);
    assert_eq!(a["role_enabled"], false);
    assert!(
        a["rows"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["reason"] == REASON_DISABLED_ON_PLATFORM && r["effective"] == false),
        "{a}"
    );
}

#[tokio::test]
async fn unrouted_until_b7_reason_no_route() {
    let x = setup().await;
    let o = ok(read(&x, "toolsets/overview?env=prod").await);
    let t = toolset_in(&o, "procedure", ACTIVITY);
    // no route row and no prod default (the demo stub is test-only): unrouted
    let d = row(t, "delivery.read{by_tracking_ref}");
    assert_eq!(d["route"], json!({"kind": "unrouted"}), "{d}");
    assert_eq!(d["effective"], false);
    assert_eq!(d["reason"], "No route on prod (N4)");
    // a built-in runs inside agent
    let k = row(t, "knowledge.search");
    assert_eq!(k["route"], json!({"kind": "builtin"}), "{k}");
    assert_eq!(k["effective"], true);
    assert_eq!(k["reason"], Value::Null);
}

#[tokio::test]
async fn narrow_then_row_narrowed_and_not_effective() {
    let x = setup().await;
    let t = ok(narrow(&x, "procedure", ACTIVITY, OP).await);
    assert_eq!(t["role"], "procedure");
    assert_eq!(t["env"], "prod");
    let r = row(&t, OP);
    assert_eq!(r["narrowed"], true);
    assert_eq!(r["granted"], true, "granted is before the narrowing");
    assert_eq!(r["effective"], false);
    assert_eq!(r["reason"], REASON_NARROWED);
    let key = format!("narrow:procedure:{ACTIVITY}:{OP}");
    assert_eq!(
        ledger(&x).await,
        [(
            key,
            "narrowing".to_owned(),
            "J8".to_owned(),
            format!("{OP} removed from procedure for {ACTIVITY}")
        )]
    );
    let m = ok(read(&x, "toolsets").await);
    assert_eq!(
        m["narrowings"],
        json!([format!("procedure|{ACTIVITY}|{OP}")])
    );

    // restoring it: the ledger row goes (draft = live)
    let t = ok(restore(&x, "procedure", ACTIVITY, OP).await);
    assert_eq!(row(&t, OP)["narrowed"], false);
    assert!(ledger(&x).await.is_empty());
}

#[tokio::test]
async fn narrow_not_granted_409() {
    let x = setup().await;
    // the qualifier is not granted `knowledge.search` on this activity (N3): nothing to narrow
    refused(
        narrow(&x, "qualifier", ACTIVITY, "knowledge.search").await,
        StatusCode::CONFLICT,
        "not_granted",
    );
    // an operation the activity does not have
    refused(
        narrow(&x, "procedure", ACTIVITY, "payment.refund").await,
        StatusCode::CONFLICT,
        "not_granted",
    );
    assert!(ledger(&x).await.is_empty());
}

#[tokio::test]
async fn narrow_twice_409_already_narrowed() {
    let x = setup().await;
    ok(narrow(&x, "procedure", ACTIVITY, OP).await);
    refused(
        narrow(&x, "procedure", ACTIVITY, OP).await,
        StatusCode::CONFLICT,
        "already_narrowed",
    );
    assert_eq!(ledger(&x).await.len(), 1);
}

#[tokio::test]
async fn restore_unknown_422_widening_rejected() {
    let x = setup().await;
    refused(
        restore(&x, "procedure", ACTIVITY, OP).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "widening_rejected",
    );
    assert!(ledger(&x).await.is_empty());
}

#[tokio::test]
async fn unknown_role_activity_and_body_are_refused() {
    let x = setup().await;
    refused(
        narrow(&x, "nobody", ACTIVITY, OP).await,
        StatusCode::NOT_FOUND,
        "role_not_found",
    );
    refused(
        narrow(&x, "procedure", "nope:nope", OP).await,
        StatusCode::NOT_FOUND,
        "activity_not_found",
    );
    refused(
        restore(&x, "procedure", "nope:nope", OP).await,
        StatusCode::NOT_FOUND,
        "activity_not_found",
    );
    refused(
        write(
            &x,
            "POST",
            "toolsets/procedure/narrowings",
            json!({"activity": ACTIVITY, "op": OP, "extra": 1}),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_body",
    );
}

#[tokio::test]
async fn overview_prod_reads_live_test_reads_draft() {
    let x = setup().await;
    ok(narrow(&x, "procedure", ACTIVITY, OP).await);
    let test = ok(read(&x, "toolsets/overview?env=test").await);
    let prod = ok(read(&x, "toolsets/overview?env=prod").await);
    assert_eq!(test["env"], "test");
    assert_eq!(prod["env"], "prod");
    assert_eq!(
        row(toolset_in(&test, "procedure", ACTIVITY), OP)["narrowed"],
        true
    );
    assert_eq!(
        row(toolset_in(&prod, "procedure", ACTIVITY), OP)["narrowed"],
        false,
        "a draft narrowing is not live"
    );
    let n = test["narrowings"].as_array().unwrap();
    assert_eq!(n.len(), 1);
    assert_eq!(n[0]["role"], "procedure");
    assert_eq!(n[0]["op"], OP);
    assert_eq!(n[0]["by"], DESIGNER);
    assert_eq!(prod["narrowings"], json!([]));
    // without tenant versions both envs run the template's procedures (by activity, no step bodies)
    assert_eq!(test["procedures"], prod["procedures"]);
    for p in test["procedures"].as_array().unwrap() {
        assert_eq!(p["origin"], "base", "{p}");
        assert!(!p["activity"].as_str().unwrap().is_empty(), "{p}");
    }
}

#[tokio::test]
async fn invalid_env_422() {
    let x = setup().await;
    for q in ["toolsets/overview", "toolsets/overview?env=staging"] {
        refused(
            read(&x, q).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_env",
        );
    }
}

/// The `ToolsetIndex` the runtime installs is `decide(..).granted` where the design governs the role × activity (T12:
/// a role without toolsets defers to B4, allowed), and the assistant's offer is exactly what it allows (the runner's `ToolGate`
/// asks the same `DesignToolsets`): one source.
#[tokio::test]
async fn design_toolset_index_allows_equals_decide_granted() {
    let x = setup().await;
    ok(narrow(&x, "procedure", ACTIVITY, OP).await);
    let d = runtime_design(&x, DesignOf::Draft).await;
    let t = &d.job_design.toolsets;
    let keys: Vec<String> = d.catalog.keys().into_iter().map(str::to_owned).collect();
    assert!(!keys.is_empty());
    let mut granted = 0;
    for role in t.roles() {
        for activity in t.activities().map(Some).chain([None]) {
            for k in &keys {
                let decision = t.decide(&role.key, activity, k);
                // T12: a role the design has toolsets for is governed for every activity (none/unknown: refused)
                let want = decision.granted || !t.governs_role(&role.key);
                assert_eq!(
                    DesignToolsets.allows(&d, &role.key, activity, k),
                    want,
                    "{} × {activity:?} × {k}",
                    role.key
                );
                assert_eq!(decision.granted, decision.reason.is_none());
                granted += usize::from(decision.granted);
            }
        }
    }
    assert!(granted > 0, "some operation is granted somewhere");
    let narrowed = t.decide("procedure", Some(ACTIVITY), OP);
    assert_eq!(narrowed.reason.as_deref(), Some(REASON_NARROWED));
}

#[tokio::test]
async fn assistant_offer_and_runner_gate_agree() {
    let x = setup().await;
    ok(narrow(&x, "procedure", ACTIVITY, OP).await);
    for of in [DesignOf::Draft, DesignOf::Live] {
        let d = runtime_design(&x, of.clone()).await;
        let t = &d.job_design.toolsets;
        for role in t.roles() {
            for activity in t.activities().map(Some).chain([None]) {
                let offer: BTreeSet<String> =
                    offered(&d.catalog, &d, &DesignToolsets, &role.key, activity)
                        .into_iter()
                        .map(|tool| tool.op_key)
                        .collect();
                let gate: BTreeSet<String> = d
                    .catalog
                    .keys()
                    .into_iter()
                    .filter(|k| DesignToolsets.allows(&d, &role.key, activity, k))
                    .map(str::to_owned)
                    .collect();
                assert_eq!(offer, gate, "{of:?} {} × {activity:?}", role.key);
            }
        }
        // prod reads the live design: the draft narrowing is refused in Test only
        let allowed = DesignToolsets.allows(&d, "procedure", Some(ACTIVITY), OP);
        assert_eq!(allowed, of == DesignOf::Live, "{of:?}");
    }
}

// ---- T11a -----------------------------------------------------------------------------------------------------------

async fn set_role(x: &Fx, role: &str, body: Value) -> (StatusCode, Value) {
    write(x, "PUT", &format!("toolsets/{role}/enabled"), body).await
}

#[tokio::test]
async fn enable_assistant_needs_platform_role_enabled() {
    let x = setup().await;
    refused(
        set_role(&x, "assistant", json!({"enabled": true})).await,
        StatusCode::CONFLICT,
        "role_disabled_on_platform",
    );
    assert!(ledger(&x).await.is_empty());
}

#[tokio::test]
async fn only_assistant_is_configurable() {
    let x = setup().await;
    for role in ["procedure", "qualifier", "post_gate"] {
        refused(
            set_role(&x, role, json!({"enabled": false})).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "role_not_configurable",
        );
    }
    refused(
        set_role(&x, "nobody", json!({"enabled": false})).await,
        StatusCode::NOT_FOUND,
        "role_not_found",
    );
    assert!(ledger(&x).await.is_empty());
}

#[tokio::test]
async fn max_tool_calls_bounds_422() {
    let x = setup().await;
    for n in [json!(0), json!(7), json!(-1)] {
        refused(
            set_role(
                &x,
                "assistant",
                json!({"enabled": false, "max_tool_calls": n}),
            )
            .await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_max_tool_calls",
        );
    }
    refused(
        set_role(
            &x,
            "assistant",
            json!({"enabled": false, "max_tool_calls": "3"}),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_body",
    );
    refused(
        set_role(&x, "assistant", json!({"enabled": false, "budget": 3})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_body",
    );
    assert!(ledger(&x).await.is_empty());
}

/// A setting is a D54 draft of kind `role_setting`; back to the default (off, no budget) its row and ledger row go.
#[tokio::test]
async fn role_setting_is_a_draft_and_returns_to_default() {
    let x = setup().await;
    let m = ok(set_role(
        &x,
        "assistant",
        json!({"enabled": false, "max_tool_calls": 3}),
    )
    .await);
    let a = m["roles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["role"] == "assistant")
        .unwrap();
    assert_eq!(a["max_tool_calls"], 3);
    assert_eq!(a["enabled_job"], false);
    let rows = ledger(&x).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "role-setting:assistant");
    assert_eq!(rows[0].1, "role_setting");
    assert_eq!(rows[0].2, "J8");
    // absent max = keep it
    let m = ok(set_role(&x, "assistant", json!({"enabled": false})).await);
    assert!(
        m["roles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["role"] == "assistant" && r["max_tool_calls"] == 3)
    );
    // null = the default: the row goes, and with it the ledger row
    ok(set_role(
        &x,
        "assistant",
        json!({"enabled": false, "max_tool_calls": null}),
    )
    .await);
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM job_agent_role_setting WHERE job_id = $1")
            .bind(x.f.job.job_id)
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(n, 0);
    assert!(ledger(&x).await.is_empty());
}

#[tokio::test]
async fn role_switch_reads_live_in_prod_draft_in_test() {
    let x = setup().await;
    ok(set_role(
        &x,
        "assistant",
        json!({"enabled": false, "max_tool_calls": 2}),
    )
    .await);
    let draft = runtime_design(&x, DesignOf::Draft).await;
    let live = runtime_design(&x, DesignOf::Live).await;
    assert_eq!(draft.job_design.roles.max_tool_calls("assistant"), 2);
    assert_eq!(draft.job_design.roles.assistant_budget().max_tool_calls, 2);
    assert_eq!(
        live.job_design.roles.max_tool_calls("assistant"),
        MAX_TOOL_CALLS,
        "the draft setting is not live"
    );
    // the platform row is off at the v1 pins: the switch is off in both
    for d in [&draft, &live] {
        assert!(!JobRoleSwitch.assistant_enabled(d, ACTIVITY));
        assert!(!JobRoleSwitch.assistant_enabled(d, ""));
    }
    // the procedure role has no job switch and its platform row is on
    assert!(draft.job_design.roles.enabled("procedure"));
}

/// The whole design contract (items 1–7) for `role_setting` (T11a; `tests/b5/targets.rs` covers T5's ten kinds).
#[tokio::test]
async fn role_setting_full_contract() {
    let e = common::env().await;
    let at = json!({"role": "assistant"}).as_object().cloned().unwrap();
    design_contract::run_all(
        &e,
        "role_setting",
        &[(at, json!({"enabled": false, "max_tool_calls": 3}))],
        Some("platform default"),
    )
    .await;
}
