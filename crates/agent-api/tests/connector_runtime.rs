//! B7 T14: the connector executor B4's runner calls, on a real Postgres 18 against an axum mock on loopback (the
//! executor gets an `AppState` whose `Transport` policy has the test-only `allow_loopback`). Untrusted output
//! (spec 17 N-04): only declared outputs, capped, `untrusted: true`, adversarial text stays data. Production runs
//! the job's current release snapshot (B2, D62: its route, overrides and tenant templates) and fails closed
//! (`no_release`) for a job never released; the test env runs the working rows (overrides).
//!
//! Omitted: the "or logs" half of `credential_never_in_outcome_or_logs` (no log capture harness; the executor logs
//! only instance, op and code).
use crate::b5::support as b5_support;
use crate::common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_api::connectors::runtime::{
    ConnectorExecutor, Env, MAX_OUTCOME_BYTES, MAX_OUTCOME_STRING_BYTES, OpOutcome, OpRequest,
    OpState, ResolvedTarget,
};
use agent_api::connectors::transport::Transport;
use agent_api::netguard::Policy;
use agent_api::state::AppState;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get as route_get, post as route_post};
use axum::{Json, Router};
use base64::Engine as _;
use common::design_contract;
use common::platform::{Op, publish_template};
use common::{TestEnv, insert_tenant, insert_user, json, person, send, super_admin};
use serde_json::{Map, Value, json};
use uuid::Uuid;

const TOKEN: &str = "svc-token-for-runtime-7731";
const INJECTION: &str = include_str!("fixtures/untrusted/order_injection.json");

#[derive(Clone, Default)]
struct Mock {
    hits: Arc<AtomicUsize>,
    /// `METHOD /path` of every request, in order.
    seen: Arc<std::sync::Mutex<Vec<String>>>,
}

fn record(m: &Mock, what: String) {
    m.hits.fetch_add(1, Ordering::SeqCst);
    m.seen.lock().unwrap().push(what);
}

fn auth_of(h: &HeaderMap) -> String {
    h.get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

async fn order(
    State(m): State<Mock>,
    Path(id): Path<String>,
    h: HeaderMap,
) -> (StatusCode, Json<Value>) {
    record(&m, format!("GET /orders/{id}"));
    let auth = auth_of(&h);
    match id.as_str() {
        "O-1" => (
            StatusCode::OK,
            Json(
                json!({"status": "shipped", "state": "in_transit", "total": "19.90",
                        "lines": [{"sku": "S-1"}], "delivery_type": "home",
                        "internal_note": "undeclared, must be dropped"}),
            ),
        ),
        "O-BIG" => {
            let lines: Vec<Value> = (0..30).map(|_| json!({"sku": "y".repeat(1900)})).collect();
            (
                StatusCode::OK,
                Json(json!({"status": "s".repeat(5000), "lines": lines, "total": "1"})),
            )
        }
        "O-INJ" => (
            StatusCode::OK,
            Json(serde_json::from_str(INJECTION).unwrap()),
        ),
        // echoes the credential back in declared fields
        "O-ECHO" => (
            StatusCode::OK,
            Json(json!({"status": auth.clone(),
                        "total": auth.trim_start_matches("Bearer ").to_string()})),
        ),
        "O-500" => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("bad token {auth}\nPASSWORD:{TOKEN}")})),
        ),
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({"errors": [{"message": "Not Found"}]})),
        ),
    }
}

async fn cancel(State(m): State<Mock>, Path(id): Path<String>) -> Json<Value> {
    record(&m, format!("POST /orders/{id}/cancel"));
    Json(json!({"status": "cancelled", "refund_expected": true}))
}

async fn mock() -> (String, Mock) {
    let m = Mock::default();
    let app = Router::new()
        .route("/orders/{id}", route_get(order))
        .route("/orders/{id}/cancel", route_post(cancel))
        .with_state(m.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), m)
}

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    vars.insert(
        "TENANT_DATA_KEYS".to_string(),
        format!(r#"{{"k1":"{key}"}}"#),
    );
    vars.insert("TENANT_DATA_KEY_CURRENT".to_string(), "k1".to_string());
}

struct World {
    e: TestEnv,
    st: AppState,
    app: Router,
    im: String,
    t: Uuid,
    x: ConnectorExecutor,
}

async fn world() -> World {
    let e = common::env_with(with_keys).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Runtime").await;
    insert_user(&e.owner, "u-im", t, "integrations_manager").await;
    let mut st = e.state.clone();
    st.connectors = Transport::new(Policy {
        allow_loopback: true,
        private_targets: vec![],
    });
    publish_template(&e, DEMO.0, DEMO.1, &shop_ops(false))
        .await
        .unwrap();
    let app = agent_api::router(st.clone());
    let x = ConnectorExecutor::new(st.clone());
    World {
        e,
        st,
        app,
        im: person("u-im", &[]),
        t,
        x,
    }
}

impl World {
    /// A system on `template@version`; the credential is set unless it is a stub.
    async fn system(&self, id: &str, tpl: (&str, &str), env: &str, base: &str, max_rps: Value) {
        let stub = base.is_empty();
        let (s, b) = send(
            &self.app,
            json(
                "POST",
                "/api/connections/systems",
                Some(&self.im),
                json!({"id": id, "label": id, "template_key": tpl.0, "template_version": tpl.1,
                       "env": env, "base_url": base, "env_ids": {}, "max_rps": max_rps, "stub": stub}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        if !stub {
            let (s, b) = send(
                &self.app,
                json(
                    "PUT",
                    &format!("/api/connections/systems/{id}/credential"),
                    Some(&self.im),
                    json!({"service_token": TOKEN}),
                ),
            )
            .await;
            assert_eq!(s, StatusCode::OK, "{b}");
        }
    }

    fn req(&self, op: &str, env: Env, inputs: Value, outputs: &[&str]) -> OpRequest {
        OpRequest {
            tenant_id: self.t,
            job_id: Uuid::new_v4(),
            operation: op.to_string(),
            env,
            inputs: inputs.as_object().cloned().unwrap_or_default(),
            outputs: outputs.iter().map(|s| s.to_string()).collect(),
            idempotency_key: "idem-runtime-1".to_string(),
            actor: None,
            case_id: None,
        }
    }

    async fn run(&self, req: &OpRequest, instance: &str) -> OpOutcome {
        self.x
            .execute(
                req,
                &ResolvedTarget::Instance {
                    instance_id: instance.to_string(),
                },
            )
            .await
    }
}

const DEMO: (&str, &str) = ("test_shop", "3");

/// `test_shop@3`: an order read keyed by order id and a cancel (X) the mock serves; the stub fixture answers `{id, status}`.
fn shop_ops(verify: bool) -> Vec<Op> {
    let ok = || json!({"id": "redacted", "status": "ok"});
    let cancel = Op::star("order", "cancel")
        .path("/orders/{{canonical.order_id}}/cancel")
        .request("{\"reason\":\"{{canonical.reason}}\"}")
        .returns(ok());
    vec![
        Op::star("order", "read")
            .path("/orders/{{canonical.order_id}}")
            .returns(ok()),
        if verify {
            cancel.verify_read("order.read")
        } else {
            cancel
        },
    ]
}

fn keys(m: &Map<String, Value>) -> Vec<&str> {
    let mut k: Vec<&str> = m.keys().map(String::as_str).collect();
    k.sort_unstable();
    k
}

#[tokio::test]
async fn undeclared_fields_dropped() {
    let w = world().await;
    let (base, _m) = mock().await;
    w.system("shop-test", DEMO, "test", &base, Value::Null)
        .await;
    let r = w.req(
        "order.read",
        Env::Test,
        json!({"order_id": "O-1"}),
        &["status", "total"],
    );
    let o = w.run(&r, "shop-test").await;
    assert_eq!(o.state, OpState::Ok, "{o:?}");
    assert_eq!(keys(&o.output), vec!["status", "total"]);
    assert_eq!(o.output["status"], "shipped");
    assert!(o.untrusted);
    assert_eq!(o.instance_id.as_deref(), Some("shop-test"));
    assert_eq!(o.template.as_deref(), Some("test_shop@3"));
    assert_eq!(o.verified, None);
}

#[tokio::test]
async fn oversize_output_capped() {
    let w = world().await;
    let (base, _m) = mock().await;
    w.system("shop-test", DEMO, "test", &base, Value::Null)
        .await;
    let r = w.req(
        "order.read",
        Env::Test,
        json!({"order_id": "O-BIG"}),
        &["status", "lines", "total"],
    );
    let o = w.run(&r, "shop-test").await;
    assert_eq!(o.state, OpState::Ok, "{o:?}");
    let size = serde_json::to_vec(&o.output).unwrap().len();
    assert!(size <= MAX_OUTCOME_BYTES, "{size}");
    assert!(o.output["status"].as_str().unwrap().len() <= MAX_OUTCOME_STRING_BYTES);
    assert_eq!(o.output["truncated"], true);
    assert_eq!(o.output["total"], "1");
    assert!(o.untrusted);
}

#[tokio::test]
async fn injection_fixture_stays_data() {
    let w = world().await;
    let (base, m) = mock().await;
    w.system("shop-test", DEMO, "test", &base, Value::Null)
        .await;
    let fixture: Value = serde_json::from_str(INJECTION).unwrap();
    let r = w.req(
        "order.read",
        Env::Test,
        json!({"order_id": "O-INJ"}),
        &["status", "total", "lines"],
    );
    let o = w.run(&r, "shop-test").await;
    assert_eq!(o.state, OpState::Ok, "{o:?}");
    assert!(o.untrusted);
    // verbatim data: no field interpreted, no placeholder rendered, the undeclared one dropped
    assert_eq!(o.output["status"], fixture["status"]);
    assert_eq!(o.output["total"], fixture["total"]);
    assert_eq!(o.output["lines"], fixture["lines"]);
    assert!(o.output.get("instructions").is_none());
    let text = serde_json::to_string(&o).unwrap();
    assert!(!text.contains(TOKEN), "{text}");
    // one read only: nothing in the body triggered another call
    assert_eq!(m.hits.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn prod_call_fails_closed_without_release() {
    let w = world().await;
    let (base, m) = mock().await;
    w.system("shop-prod", DEMO, "prod", &base, Value::Null)
        .await;
    let r = w.req(
        "order.read",
        Env::Prod,
        json!({"order_id": "O-1"}),
        &["status"],
    );
    let o = w.run(&r, "shop-prod").await;
    assert_eq!(o.state, OpState::Error, "{o:?}");
    assert_eq!(o.outcome_code.as_deref(), Some("no_release"));
    assert!(o.output.is_empty());
    assert_eq!(m.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn prod_call_ignores_unpublished_route_change() {
    let w = world().await;
    let (base, _m) = mock().await;
    w.system("shop-a", DEMO, "prod", &base, Value::Null).await;
    w.system("shop-b", DEMO, "prod", &base, Value::Null).await;
    // the template whose activities name order.read (the test offering has none of its own)
    let (layer, version) = b5_support::template(&w.e).await;
    let job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, template_layer, template_version, created_by) \
         SELECT $1, 'cca', 'Customer care', pins, layer, version, 'test' FROM job_template_tag \
          WHERE layer = $2 AND version = $3 AND status = 'tagged' \
         RETURNING id",
    )
    .bind(w.t)
    .bind(layer)
    .bind(version)
    .fetch_one(&w.e.owner)
    .await
    .expect("a tagged customer-care template");
    let route = |id: &str| {
        json(
            "PUT",
            "/api/jobs/cca/operations/order.read/routes/prod",
            Some(&w.im),
            json!({"target": {"kind": "instance", "instance_id": id}}),
        )
    };
    // the draft selection folds the activities' operations: without one nothing is an expected operation to route
    let f = design_contract::contract_job(w.t, job, "cca");
    design_contract::write(
        &w.e,
        &f,
        "selection",
        &agent_api::design::DraftRef::new(),
        Some(design_contract::cuttable_selection()),
    )
    .await;
    // every other operation the template's activities need is routed on prod; order.read gets the instance below
    b5_support::route_activity_ops(&w.e, &f).await;
    // shop-a passed its contract test for order.read on prod (a route counts as ready only when tested)
    sqlx::query(
        "INSERT INTO connector_contract_result (tenant_id, instance_id, op, env, side_effect, outcome) \
         VALUES ($1, 'shop-a', 'order.read', 'prod', 'R', 'green')",
    )
    .bind(w.t)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let (s, b) = send(&w.app, route("shop-a")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    design_contract::publishable(&w.e, &f).await;
    design_contract::publish(&w.e, &f).await;

    // unpublished draft changes: order.read moves to shop-b, and shop-a gets a status override
    let (s, b) = send(&w.app, route("shop-b")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    sqlx::query(
        "INSERT INTO field_mapping_override (tenant_id, instance_id, op, field, default_path, path, updated_by) \
         VALUES ($1, 'shop-a', 'order.read', 'status', '$.status', '$.state', 'u-im')",
    )
    .bind(w.t)
    .execute(&w.e.owner)
    .await
    .unwrap();

    let mut r = w.req(
        "order.read",
        Env::Prod,
        json!({"order_id": "O-1"}),
        &["status"],
    );
    r.job_id = job;
    // the caller hands over the draft's target; Production runs the release's
    let o = w.run(&r, "shop-b").await;
    assert_eq!(o.state, OpState::Ok, "{o:?}");
    assert_eq!(o.instance_id.as_deref(), Some("shop-a"), "{o:?}");
    assert_eq!(
        o.output["status"], "shipped",
        "the unpublished override does not apply in prod: {o:?}"
    );
}

#[tokio::test]
async fn test_call_uses_draft_override() {
    let w = world().await;
    let (base, _m) = mock().await;
    w.system("shop-test", DEMO, "test", &base, Value::Null)
        .await;
    // a working (draft) override row: status comes from $.state instead of $.status
    sqlx::query(
        "INSERT INTO field_mapping_override (tenant_id, instance_id, op, field, default_path, path, updated_by) \
         VALUES ($1, 'shop-test', 'order.read', 'status', '$.status', '$.state', 'u-im')",
    )
    .bind(w.t)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let r = w.req(
        "order.read",
        Env::Test,
        json!({"order_id": "O-1"}),
        &["status"],
    );
    let o = w.run(&r, "shop-test").await;
    assert_eq!(o.state, OpState::Ok, "{o:?}");
    assert_eq!(o.output["status"], "in_transit");
}

#[tokio::test]
async fn stub_never_serves_prod() {
    let w = world().await;
    w.system("shop-stub", DEMO, "test", "", Value::Null).await;
    let read = |env| w.req("order.read", env, json!({"order_id": "O-1"}), &["status"]);
    let o = w.run(&read(Env::Prod), "shop-stub").await;
    assert_eq!(o.state, OpState::Error, "{o:?}");
    assert!(o.output.is_empty());
    assert_ne!(o.outcome_code.as_deref(), None);
    // the same stub serves the test env from its fixture
    let o = w.run(&read(Env::Test), "shop-stub").await;
    assert_eq!(o.state, OpState::Ok, "{o:?}");
    assert_eq!(o.output["status"], "ok");
}

/// `test_shop_vr@1`: `test_shop@3` whose `order.cancel` (X) declares `verify_read order.read`.
async fn verify_template(e: &TestEnv) {
    publish_template(e, "test_shop_vr", "1", &shop_ops(true))
        .await
        .unwrap();
}

#[tokio::test]
async fn x_op_runs_verify_read() {
    let w = world().await;
    verify_template(&w.e).await;
    let (base, m) = mock().await;
    w.system("shop-vr", ("test_shop_vr", "1"), "test", &base, Value::Null)
        .await;
    let cancel = |id: &str| {
        w.req(
            "order.cancel",
            Env::Test,
            json!({"order_id": id, "reason": "changed mind"}),
            &["status", "refund_expected"],
        )
    };
    let o = w.run(&cancel("O-1"), "shop-vr").await;
    assert_eq!(o.state, OpState::Ok, "{o:?}");
    assert_eq!(o.verified, Some(true));
    assert_eq!(o.output["status"], "cancelled");
    assert_eq!(
        *m.seen.lock().unwrap(),
        vec![
            "POST /orders/O-1/cancel".to_string(),
            "GET /orders/O-1".to_string()
        ]
    );
    // the write went through but the verify read finds nothing: unknown outcome, never ok
    let o = w.run(&cancel("O-GONE"), "shop-vr").await;
    assert_eq!(o.state, OpState::UnknownOutcome, "{o:?}");
    assert_eq!(o.outcome_code.as_deref(), Some("unknown_outcome"));
    assert!(o.output.is_empty());
}

#[tokio::test]
async fn rate_limit_per_instance() {
    let w = world().await;
    w.system("stub-a", DEMO, "test", "", json!(1)).await;
    w.system("stub-b", DEMO, "test", "", json!(1)).await;
    let r = w.req(
        "order.read",
        Env::Test,
        json!({"order_id": "O-1"}),
        &["status"],
    );
    assert_eq!(w.run(&r, "stub-a").await.state, OpState::Ok);
    let o = w.run(&r, "stub-a").await;
    assert_eq!(o.state, OpState::Error, "{o:?}");
    assert_eq!(o.outcome_code.as_deref(), Some("rate_limited"));
    // another instance has its own bucket
    assert_eq!(w.run(&r, "stub-b").await.state, OpState::Ok);
    // and so does another executor process (in-process buckets)
    let other = ConnectorExecutor::new(w.st.clone());
    let o = other
        .execute(
            &r,
            &ResolvedTarget::Instance {
                instance_id: "stub-a".into(),
            },
        )
        .await;
    assert_eq!(o.state, OpState::Ok, "{o:?}");
}

#[tokio::test]
async fn credential_never_in_outcome_or_logs() {
    let w = world().await;
    let (base, _m) = mock().await;
    w.system("shop-test", DEMO, "test", &base, Value::Null)
        .await;
    // the system echoes the credential in declared fields
    let r = w.req(
        "order.read",
        Env::Test,
        json!({"order_id": "O-ECHO"}),
        &["status", "total"],
    );
    let o = w.run(&r, "shop-test").await;
    assert_eq!(o.state, OpState::Ok, "{o:?}");
    let text = serde_json::to_string(&o).unwrap();
    assert!(!text.contains(TOKEN), "{text}");
    // and in an error body
    let r = w.req(
        "order.read",
        Env::Test,
        json!({"order_id": "O-500"}),
        &["status"],
    );
    let o = w.run(&r, "shop-test").await;
    assert_eq!(o.state, OpState::Error, "{o:?}");
    let text = serde_json::to_string(&o).unwrap();
    assert!(!text.contains(TOKEN), "{text}");
    assert!(!format!("{o:?}").contains(TOKEN));
}

/// A template that declares `auth_kind: none` needs no stored credential. Transport `mcp` + kind `none` is the
/// PLATFORM MCP server (its URL is `MCP_URL`, its identity the platform's own), and a none-auth REST/GraphQL system
/// sends no Authorization header either. The executor must reach that stage and answer `MCP_URL is not set` in this
/// test state, never the missing-credential refusal (found live 2026-11-05: the `fr_mcp` system could not start a
/// project).
#[tokio::test]
async fn a_none_auth_template_needs_no_credential() {
    let w = world().await;
    let ops = [Op::star("case", "read")];
    let (s, b) = send(
        &w.app,
        json(
            "POST",
            "/api/platform/connector-templates",
            Some(&super_admin()),
            json!({"key": "platform_mcp_test", "version": "1", "label": "Platform MCP", "vendor": "Test",
                   "system_class": "ticketing", "wave": "v1", "transport": "mcp", "auth_kind": "none",
                   "vendor_api_pin": "test pin",
                   "ops": ops.iter().map(Op::template_op).collect::<Vec<_>>()}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = send(
        &w.app,
        json(
            "POST",
            "/api/platform/connector-templates/platform_mcp_test/1/publish",
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // the system is created WITHOUT a credential: kind `none` has none to store
    let (s, b) = send(
        &w.app,
        json(
            "POST",
            "/api/connections/systems",
            Some(&w.im),
            json!({"id": "plat-mcp", "label": "plat", "template_key": "platform_mcp_test",
                   "template_version": "1", "env": "test", "base_url": "https://platform.example/mcp",
                   "env_ids": {}, "stub": false, "max_rps": null}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let r = w.req(
        "case.read",
        Env::Test,
        json!({"case_id": "C-1"}),
        &["title"],
    );
    let o = w.run(&r, "plat-mcp").await;
    assert_eq!(o.state, OpState::Error, "{o:?}");
    assert_eq!(o.outcome_code.as_deref(), Some("connector_misconfigured"));
    let detail = o.detail.as_deref().unwrap_or("");
    assert!(detail.contains("MCP_URL"), "{o:?}");
    assert!(!detail.contains("credential"), "{o:?}");
    // the same rule on a contract test: it must not refuse the run for a credential that cannot exist
    let (s, b) = send(
        &w.app,
        json(
            "POST",
            "/api/connections/systems/plat-mcp/contract-tests",
            Some(&w.im),
            json!({"op": "case.read", "prod_write_ok": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_ne!(b["code"].as_str(), Some("connector_auth_failed"), "{b}");
    assert!(
        !b["note"]
            .as_str()
            .unwrap_or("")
            .contains("No credential set"),
        "{b}"
    );
}

#[tokio::test]
async fn auth_failed_instance_is_connector_misconfigured() {
    let w = world().await;
    let (base, m) = mock().await;
    w.system("shop-test", DEMO, "test", &base, Value::Null)
        .await;
    for status in ["auth_failed", "invalid_target"] {
        sqlx::query("UPDATE connector_instance SET status = $1 WHERE id = 'shop-test'")
            .bind(status)
            .execute(&w.e.owner)
            .await
            .unwrap();
        let r = w.req(
            "order.read",
            Env::Test,
            json!({"order_id": "O-1"}),
            &["status"],
        );
        let o = w.run(&r, "shop-test").await;
        assert_eq!(o.state, OpState::Error, "{o:?}");
        assert_eq!(o.outcome_code.as_deref(), Some("connector_misconfigured"));
    }
    assert_eq!(m.hits.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn non_instance_targets_are_not_handled() {
    let w = world().await;
    let r = w.req("case.create{handover}", Env::Test, json!({}), &[]);
    for target in [
        ResolvedTarget::Builtin { detail: None },
        ResolvedTarget::AgentCases,
        ResolvedTarget::DeepLink {
            url: "https://example.com/x".into(),
        },
        ResolvedTarget::NotOffered,
    ] {
        let o = w.x.execute(&r, &target).await;
        assert_eq!(o.state, OpState::NotHandled, "{target:?}");
    }
}
