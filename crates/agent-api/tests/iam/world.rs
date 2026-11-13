//! The shared world of the IAM acceptance tests: a platform-owned tenant whose job runs on the real runtime roles.
//!
//! - the language model: [`Llm`], an OpenAI-compatible `POST /v1/chat/completions` on loopback (`INTERNAL_LLM_URL`).
//!   A request whose one system message carries `[task assistant]` is the assistant loop's and is answered by the
//!   test's [`Script`]; any other request is the qualifier's and is answered with [`SPACE`]. Every body is kept.
//! - the tenant's external system: [`System`], an HTTP server on loopback behind a real, non-stub connector instance
//!   (template `iam_desk@1`, bearer credential [`SERVICE_TOKEN`]). It serves a one-time-code check
//!   (`porting.read`), a CRM lookup (`party.list`), an order list (`order.list`) and a board comment
//!   (`case.comment`): catalogue operations standing in for the plan's `iam.verify`, `crm.lookup`, `account.read`
//!   and a kanban op. Their input names are the catalogue's (`msisdn` carries the code, `order_id` of `party.list`
//!   carries the token): a template may only render declared inputs, and the declarations map state keys to them.
//! - the roles: orchestrator (with the assistant), runner (real connector executor) and interaction, started with
//!   [`World::daemons`] exactly as `tests/kh/t7_answer.rs` does; the staff test chat (env `test`) and the API-key
//!   interactions (env `prod`, [`World::live`]) are the two ways in.
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_api::connectors::transport::Transport;
use agent_api::interaction::webhook::{self, Webhooks};
use agent_api::interaction::{BackendError, BackendEvent, InteractionBackend, InteractionRequest};
use agent_api::job_design::role_switch::JobRoleSwitch;
use agent_api::netguard::Policy;
use agent_api::runtime::assistant::tools::ToolsetIndex;
use agent_api::runtime::assistant::{Assistant, RoleSwitch};
use agent_api::runtime::design::{DesignCache, RuntimeDesign};
use agent_api::runtime::heartbeat::Serving;
use agent_api::runtime::interaction::Interaction;
use agent_api::runtime::orchestrator::{Orchestrator, StaticProcedures};
use agent_api::runtime::runner::Runner;
use agent_api::runtime::{Env, Scope};
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::routing::post;
use base64::Engine as _;
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::b5::support as b5_support;
use crate::common::design_contract::{self, ContractJob};
use crate::common::{
    self, TestEnv, conv, insert_user, json as req, person, platform, send, super_admin,
};
use crate::redis_env as r;

// ---- the story's constants --------------------------------------------------------------------------------------------

/// The staff test chat's customer (a typed test customer; the chat writes it as state `email`).
pub const CUSTOMER_EMAIL: &str = "ann@shop.example";
/// The one-time code the user types, the token the verifier issues, the id the CRM answers: none of them may ever reach
/// a model request or the transcript.
pub const CODE: &str = "482913";
pub const TOKEN: &str = "tok-7f3a9c21-secret";
pub const CUSTOMER: &str = "CUST-4711";
/// The platform's own credential of the connector instance (never the user's token).
pub const SERVICE_TOKEN: &str = "svc-token-iam-0001";
/// The question the declaration asks, the user's first message, the model's final words.
pub const PROMPT: &str = "Please enter the one-time code we just sent you";
pub const ASK: &str = "Show me my orders";
pub const ANSWER: &str = "Here are your orders.";
/// The context space the qualifier answers: a space of the b5 template without a procedure, so the assistant runs.
pub const SPACE: &str = "retail:order-support:anything_else";

/// The catalogue operations the world authors and routes.
pub const VERIFY: &str = "porting.read";
pub const LOOKUP: &str = "party.list";
pub const READ: &str = "order.list";
pub const BOARD: &str = "case.comment";
/// An irreversible (X) operation: never a producer.
pub const CANCEL: &str = "order.cancel";
pub const OPS: [&str; 5] = [VERIFY, LOOKUP, READ, BOARD, CANCEL];

const TEMPLATE: &str = "iam_desk";
const OPDEFS: &str = include_str!("../../contracts/opdefs.json");

// ---- the fake model ---------------------------------------------------------------------------------------------------

/// The answer of the assistant loop to one request body: `(delay in ms before answering, the assistant message)`.
pub type Script = Arc<dyn Fn(&Value) -> (u64, Value) + Send + Sync>;

/// An assistant message calling `calls` (`(function name, arguments)`), ids `c0`, `c1`, ….
pub fn calls_msg(calls: &[(&str, Value)]) -> Value {
    let tc: Vec<Value> = calls
        .iter()
        .enumerate()
        .map(|(i, (name, args))| {
            json!({"id": format!("c{i}"), "type": "function",
                   "function": {"name": name, "arguments": args.to_string()}})
        })
        .collect();
    json!({"role": "assistant", "content": null, "tool_calls": tc})
}

pub fn final_msg(text: &str) -> Value {
    json!({"role": "assistant", "content": text})
}

/// Does the history of `body` hold a tool answer (a round after the first)?
pub fn has_tool_result(body: &Value) -> bool {
    body["messages"]
        .as_array()
        .is_some_and(|m| m.iter().any(|x| x["role"] == "tool"))
}

/// The assistant calls `tool` with `args` in its first round and says `text` once it has the tool's answer.
pub fn tool_then_say(tool: &'static str, args: Value, text: &'static str) -> Script {
    Arc::new(move |b: &Value| {
        if has_tool_result(b) {
            (0, final_msg(text))
        } else {
            (0, calls_msg(&[(tool, args.clone())]))
        }
    })
}

/// The assistant only talks.
pub fn say_only(text: &'static str) -> Script {
    Arc::new(move |_: &Value| (0, final_msg(text)))
}

pub fn system_of(body: &Value) -> &str {
    body["messages"][0]["content"].as_str().unwrap_or_default()
}

pub struct Llm {
    pub url: String,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl Llm {
    pub async fn start(script: Script) -> Llm {
        let bodies: Arc<Mutex<Vec<Value>>> = Arc::default();
        let seen = bodies.clone();
        let app = axum::Router::new().route(
            "/v1/chat/completions",
            post(move |axum::Json(v): axum::Json<Value>| {
                let (seen, script) = (seen.clone(), script.clone());
                async move {
                    seen.lock().unwrap().push(v.clone());
                    let system = system_of(&v);
                    // the script answers the assistant loop and the MCP task answer (crate::runtime::task_answer)
                    let scripted = system.contains("[task assistant]")
                        || system.contains("[task task_answer]");
                    let msg = if scripted {
                        let (delay_ms, msg) = script(&v);
                        if delay_ms > 0 {
                            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        }
                        msg
                    } else {
                        json!({"role": "assistant",
                               "content": json!({"context_space": SPACE, "slots": {}}).to_string()})
                    };
                    axum::Json(json!({
                        "choices": [{"message": msg}],
                        "usage": {"prompt_tokens": 9, "completion_tokens": 5},
                    }))
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(l, app).await.unwrap();
        });
        Llm {
            url: format!("http://{addr}/v1"),
            bodies,
        }
    }

    /// Every request body so far (qualifier and assistant).
    pub fn bodies(&self) -> Vec<Value> {
        self.bodies.lock().unwrap().clone()
    }

    /// The assistant loop's request bodies.
    pub fn assistant_bodies(&self) -> Vec<Value> {
        self.bodies()
            .into_iter()
            .filter(|b| system_of(b).contains("[task assistant]"))
            .collect()
    }

    /// The first round of each assistant turn: the bodies with no tool answer in their history.
    pub fn first_rounds(&self) -> usize {
        self.assistant_bodies()
            .iter()
            .filter(|b| !has_tool_result(b))
            .count()
    }

    /// Every body as one string: what a leak check searches.
    pub fn everything(&self) -> String {
        serde_json::to_string(&self.bodies()).unwrap()
    }
}

// ---- the fake external system ------------------------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Hit {
    pub method: String,
    pub path: String,
    pub query: String,
    pub auth: String,
    pub body: String,
}

type Hits = Arc<Mutex<Vec<Hit>>>;

pub struct System {
    pub url: String,
    hits: Hits,
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

async fn serve_system(
    axum::extract::State(hits): axum::extract::State<Hits>,
    request: Request<Body>,
) -> (StatusCode, axum::Json<Value>) {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 1 << 20)
        .await
        .unwrap_or_default();
    let query = parts.uri.query().unwrap_or("").to_owned();
    let hit = Hit {
        method: parts.method.to_string(),
        path: parts.uri.path().to_owned(),
        query: query.clone(),
        auth: parts
            .headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_owned(),
        body: String::from_utf8_lossy(&bytes).into_owned(),
    };
    hits.lock().unwrap().push(hit.clone());
    let q = parse_query(&query);
    let arg = |k: &str| q.get(k).map(String::as_str);
    let ok = StatusCode::OK;
    match (hit.method.as_str(), hit.path.as_str()) {
        // the IAM app: the one-time code is the credential, the token is the answer
        ("GET", "/porting/read") if arg("msisdn") == Some(CODE) => (
            ok,
            axum::Json(
                json!({"porting_id": TOKEN, "direction": "in", "status": "verified",
                              "porting_date": "2026-11-01", "rejection_reason": ""}),
            ),
        ),
        ("GET", "/porting/read") => (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": "the code is wrong"})),
        ),
        // the CRM: the email AND the token identify the customer
        ("GET", "/party/list")
            if arg("order_id") == Some(TOKEN) && arg("email") == Some(CUSTOMER_EMAIL) =>
        {
            (
                ok,
                axum::Json(json!({"party_id": CUSTOMER, "roles": "customer"})),
            )
        }
        ("GET", "/party/list") => (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "unknown token"})),
        ),
        // the account: only the CRM's customer id opens it
        ("GET", "/order/list") if arg("party_id") == Some(CUSTOMER) => (
            ok,
            axum::Json(
                json!({"order_id": "ORD-1", "status": "shipped", "placed_at": "2026-10-01",
                              "total": "19.90", "items": "kettle"}),
            ),
        ),
        ("GET", "/order/list") => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"errors": [{"message": "Not Found"}]})),
        ),
        ("POST", "/case/comment") => (ok, axum::Json(json!({"status": "commented"}))),
        ("POST", "/order/cancel") => (ok, axum::Json(json!({"status": "cancelled"}))),
        _ => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"errors": [{"message": "Not Found"}]})),
        ),
    }
}

impl System {
    pub async fn start() -> System {
        let hits: Hits = Arc::default();
        let app = axum::Router::new()
            .fallback(serve_system)
            .with_state(hits.clone());
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(l, app).await.unwrap();
        });
        System {
            url: format!("http://{addr}"),
            hits,
        }
    }

    pub fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }

    pub fn paths(&self) -> Vec<String> {
        self.hits().into_iter().map(|h| h.path).collect()
    }
}

// ---- the staff playground and the assistant's policy objects --------------------------------------------------------------

pub const PLAYGROUND: &str = "playground answer";

/// The staff draft playground (`AppState.interaction`): counts its calls.
#[derive(Default)]
pub struct Playground {
    pub calls: AtomicUsize,
}

impl Playground {
    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl InteractionBackend for Playground {
    fn respond(
        &self,
        _req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        stream::iter(vec![
            Ok(BackendEvent::Delta(PLAYGROUND.into())),
            Ok(BackendEvent::Done { usage: None }),
        ])
        .boxed()
    }
}

/// The assistant is on for every activity.
struct On;

impl RoleSwitch for On {
    fn assistant_enabled(&self, _d: &RuntimeDesign, _activity: &str) -> bool {
        true
    }
}

/// A toolset granting the `assistant` role exactly these operation keys (the b5 template's activity for the space
/// grants none of the world's operations).
struct Grants(Vec<String>);

impl ToolsetIndex for Grants {
    fn allows(&self, _d: &RuntimeDesign, role: &str, _activity: Option<&str>, op: &str) -> bool {
        role == "assistant" && self.0.iter().any(|g| g.as_str() == op)
    }
}

// ---- authoring the catalogue operations ---------------------------------------------------------------------------------------

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, req(method, uri, Some(&super_admin()), body)).await
}

/// The `*` row of catalogue operation `key` (`object.verb`) as `contracts/opdefs.json` defines it.
fn op_row(key: &str) -> Value {
    let (object, verb) = key.split_once('.').expect("object.verb");
    let defs: Vec<Value> = serde_json::from_str(OPDEFS).expect("contracts/opdefs.json");
    let d = defs
        .into_iter()
        .find(|d| d["object"] == object && d["verb"] == verb && d["variant"] == "*")
        .unwrap_or_else(|| panic!("{key} is not in contracts/opdefs.json"));
    let side = d["side_effect"].as_str().unwrap().to_owned();
    let (modality, idem, confirm) = match side.as_str() {
        "R" => ("observe", "nat", false),
        "W" => ("request", "key+dup", true),
        "X" => ("do", "key+dup", true),
        other => panic!("{key}: side effect {other} is not one the IAM tests author"),
    };
    json!({
        "attrs": {
            "modality": modality, "is_derive": false, "side_effect": side, "idempotency": idem,
            "confirm_back": confirm, "verify_read": null, "builtin": false, "op_kind": "standard",
            "guard_rules": [], "identity_level": d["identity_level"],
            "input_fields": d["input_fields"], "output_fields": d["output_fields"],
        },
        "description": format!("{side} operation of the IAM test catalogue."),
    })
}

/// Author catalogue operations `keys` on the base layer and tag it when any was new: a job template tagged AFTER this
/// call pins them (so call it before the first `b5_support::template`). Idempotent.
async fn author_ops(e: &TestEnv, keys: &[&str]) {
    platform::offering(e).await;
    let base: String = sqlx::query_scalar("SELECT id::text FROM layer WHERE key = 'base'")
        .fetch_one(&e.owner)
        .await
        .expect("the base layer");
    let mut any = false;
    for key in keys {
        let (s, b) = sa(
            e,
            "POST",
            &format!("/api/layers/{base}/rows"),
            json!({"kind": "operation", "key": key, "op": "add", "fields": op_row(key)}),
        )
        .await;
        if s == StatusCode::CONFLICT && b["error"]["code"] == "duplicate_add" {
            continue;
        }
        assert_eq!(s, StatusCode::CREATED, "operation {key}: {b}");
        any = true;
    }
    if any {
        let (s, b) = sa(e, "POST", "/api/platform/templates/base/tags", json!({})).await;
        let nothing = s == StatusCode::CONFLICT && b["error"]["code"] == "nothing_to_tag";
        assert!(s == StatusCode::CREATED || nothing, "tag base: {s} {b}");
    }
}

/// Enable the platform `assistant` role and add a context space with no procedure (`SPACE`), then re-tag the template
/// stack. Returns the newest job template `(layer, version)`: jobs must be created on it.
async fn extend_template(e: &TestEnv) -> (String, i32) {
    let layer_id = |key: &'static str| async move {
        let id: String = sqlx::query_scalar("SELECT id::text FROM layer WHERE key = $1")
            .bind(key)
            .fetch_one(&e.owner)
            .await
            .unwrap_or_else(|x| panic!("layer {key}: {x}"));
        id
    };
    let row = |layer: String,
               kind: &'static str,
               key: &'static str,
               op: &'static str,
               fields: Value| async move {
        let mut body = json!({"kind": kind, "key": key, "op": op, "fields": fields});
        if op == "patch" {
            body["expected_rev"] = json!(1);
        }
        let mut res = sa(
            e,
            "POST",
            &format!("/api/layers/{layer}/rows"),
            body.clone(),
        )
        .await;
        // a patch names the row's current revision: retry once with the one the conflict reports
        if res.0 == StatusCode::CONFLICT {
            let err = &res.1["error"];
            if let Some(rev) = err["current_rev"]
                .as_i64()
                .or(err["details"]["current_rev"].as_i64())
            {
                body["expected_rev"] = json!(rev);
                res = sa(e, "POST", &format!("/api/layers/{layer}/rows"), body).await;
            }
        }
        let (s, b) = res;
        assert_eq!(s, StatusCode::CREATED, "{kind} {key}: {b}");
    };
    // the shared B5 template adds the platform `assistant` role switched off (seed v1, K50): the super admin switches
    // it on for the job template (P15)
    let job_template = platform::JOB_LAYER.trim_start_matches("job/");
    let (s, b) = sa(
        e,
        "PUT",
        &format!("/api/platform/agent-roles/{job_template}/assistant/enabled"),
        json!({"enabled": true}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "enable assistant: {b}");
    row(
        layer_id(platform::DEPARTMENT_LAYER).await,
        "context_space",
        SPACE,
        "add",
        json!({"label": "Anything else", "area": "order-support", "slots": []}),
    )
    .await;
    row(
        layer_id(platform::JOB_LAYER).await,
        "job_setting",
        "iam_content",
        "add",
        json!({"label": "IAM test content"}),
    )
    .await;
    for family in ["base", platform::DEPARTMENT_LAYER, platform::JOB_LAYER] {
        let (s, b) = sa(
            e,
            "POST",
            &format!("/api/platform/templates/{}/tags", family.replace('/', ".")),
            json!({}),
        )
        .await;
        let nothing = s == StatusCode::CONFLICT && b["error"]["code"] == "nothing_to_tag";
        assert!(s == StatusCode::CREATED || nothing, "tag {family}: {s} {b}");
    }
    let version: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM job_template_tag WHERE layer = $1 AND status = 'tagged'",
    )
    .bind(platform::JOB_LAYER)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    (platform::JOB_LAYER.to_owned(), version)
}

/// The connector template: REST over the fake system. The catalogue's own input names are rendered (`msisdn` = the
/// code, `order_id` of `party.list` = the token, `party_id` = the customer id).
fn template_ops() -> Vec<platform::Op> {
    vec![
        platform::Op::star("porting", "read"),
        platform::Op::star("party", "list")
            .path("/party/list?email={{canonical.email}}&order_id={{canonical.order_id}}"),
        platform::Op::star("order", "list"),
        platform::Op::star("case", "comment"),
        platform::Op::star("order", "cancel"),
    ]
}

// ---- declarations --------------------------------------------------------------------------------------------------------------

/// `porting.read` (the IAM app's verify): asks the user for the code, produces the sealed `auth_token`.
pub fn verify_policy() -> Value {
    json!({
        "inputs": {"msisdn": {"source": "state", "key": "one_time_code",
                              "from": {"ask_user": PROMPT, "secret": true, "retries": 2}}},
        "outputs": {"porting_id": {"key": "auth_token", "kind": "secret", "ttl_s": 3600}},
    })
}

/// `party.list` (the CRM lookup): the channel's email and the token, produces `customer_id`.
pub fn lookup_policy() -> Value {
    json!({
        "inputs": {
            "email": {"source": "state", "key": "email", "from": "state"},
            "order_id": {"source": "state", "key": "auth_token",
                         "from": {"tool": VERIFY, "output": "porting_id"}},
        },
        "outputs": {"party_id": {"key": "customer_id"}},
    })
}

/// `order.list` (the account read): `party_id` is the customer id of state; `from_tool` names the CRM op as its
/// source, else the producer is found in the job's index (an attached procedure).
pub fn read_policy(from_tool: bool) -> Value {
    let from = if from_tool {
        json!({"tool": LOOKUP, "output": "party_id"})
    } else {
        json!("state")
    };
    json!({"inputs": {"party_id": {"source": "state", "key": "customer_id", "from": from}}})
}

/// The same chain as tenant procedure steps ("Acme sign-in" produces `auth_token` and `customer_id`).
pub fn signin_steps() -> Value {
    let v = verify_policy();
    let l = lookup_policy();
    json!([
        {"op": VERIFY, "inputs": v["inputs"], "outputs": v["outputs"]},
        {"op": LOOKUP, "inputs": l["inputs"], "outputs": l["outputs"]},
    ])
}

// ---- the world -------------------------------------------------------------------------------------------------------------------

pub struct ApiKey {
    pub id: String,
    pub secret: String,
}

pub struct World {
    pub e: TestEnv,
    /// The first job (its tenant is the world's).
    pub f: ContractJob,
    pub llm: Llm,
    pub sys: System,
    pub play: Arc<Playground>,
    /// An integrations manager of the tenant.
    pub im: String,
    /// The job template every job of the world is created on.
    pub template: (String, i32),
}

fn provider_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    vars.insert(
        "PROVIDER_SECRET_KEYS".into(),
        format!(r#"{{"p1":"{key}"}}"#),
    );
    vars.insert("PROVIDER_SECRET_KEY_CURRENT".into(), "p1".into());
}

fn webhook_key() -> String {
    "k".repeat(48)
}

/// Poll `f` until it answers `Some` (30 s at most).
pub async fn until<T, F, Fut>(what: &str, f: F) -> T
where
    F: Fn() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

impl World {
    /// A platform-owned tenant with one job (b5 template + the world's operations), the connector template
    /// published, an integrations manager, the fake model answering `script`. `webhooks`: async interactions with
    /// signed callbacks on (a local receiver allowed). No operation is routed yet: [`World::wire`].
    pub async fn build(name: &str, script: Script, webhooks: bool) -> World {
        let llm = Llm::start(script).await;
        let sys = System::start().await;
        let vars = r::api_vars(name).await;
        let url = llm.url.clone();
        let mut e = common::env_with(|v| {
            v.extend(vars);
            v.insert("INTERNAL_LLM_URL".into(), url);
            conv::with_keys(v);
            provider_keys(v);
            if webhooks {
                v.insert("INTERACTION_WEBHOOK_KEY".into(), webhook_key());
            }
        })
        .await;
        e.state.connectors = Transport::new(Policy {
            allow_loopback: true,
            private_targets: vec![],
        });
        let play = Arc::new(Playground::default());
        e.state.interaction = play.clone();
        if webhooks {
            let mut w = Webhooks::new(&webhook_key());
            w.allow_private = true;
            w.retry = vec![Duration::ZERO, Duration::from_millis(50)];
            e.state.webhooks = Some(Arc::new(w));
        }
        e.app = agent_api::router(e.state.clone());
        if webhooks {
            webhook::spawn_worker(e.state.clone()).expect("webhooks configured");
        }
        // `order.cancel` (CANCEL) is the B5 template's own operation: author only the ones it lacks
        author_ops(&e, &[VERIFY, LOOKUP, READ, BOARD]).await;
        b5_support::template(&e).await;
        let template = extend_template(&e).await;
        let tenant = Uuid::new_v4();
        let f = design_contract::new_job_for_on(&e, tenant, template.clone()).await;
        b5_support::route_activity_ops(&e, &f).await;
        sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
            .bind(f.tenant)
            .execute(&e.owner)
            .await
            .unwrap();
        platform::publish_template(&e, TEMPLATE, "1", &template_ops())
            .await
            .unwrap_or_else(|m| panic!("connector template: {m}"));
        let im_sub = format!("u-im-{}", f.tenant.simple());
        insert_user(&e.owner, &im_sub, f.tenant, "integrations_manager").await;
        let w = World {
            e,
            f,
            llm,
            sys,
            play,
            im: person(&im_sub, &[]),
            template,
        };
        w.prepare(&w.f).await;
        w
    }

    // -- requests ----------------------------------------------------------------------------------------------------------------

    fn tenant_header(&self, mut q: Request<Body>, tenant: Uuid) -> Request<Body> {
        q.headers_mut()
            .insert("x-active-tenant-id", tenant.to_string().parse().unwrap());
        q
    }

    /// `method /api/jobs/{job}/{tail}` as a super_admin acting in the job's tenant.
    pub async fn call(
        &self,
        f: &ContractJob,
        method: &str,
        tail: &str,
        body: Value,
    ) -> (StatusCode, Value) {
        let uri = format!("/api/jobs/{}/{tail}", f.job.job_key);
        let q = if method == "GET" {
            common::get(&uri, Some(&super_admin()))
        } else {
            req(method, &uri, Some(&super_admin()), body)
        };
        send(&self.e.app, self.tenant_header(q, f.tenant)).await
    }

    /// `method uri` (a tenant route) as a super_admin acting in the world's tenant.
    pub async fn tenant_call(&self, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        let q = if method == "GET" {
            common::get(uri, Some(&super_admin()))
        } else {
            req(method, uri, Some(&super_admin()), body)
        };
        send(&self.e.app, self.tenant_header(q, self.f.tenant)).await
    }

    // -- the job's design ----------------------------------------------------------------------------------------------------

    /// The draft's selection (what folds the catalogue and the spaces).
    pub async fn prepare(&self, f: &ContractJob) {
        let (s, b) = self
            .call(
                f,
                "PUT",
                "target",
                json!({"selection": design_contract::cuttable_selection(), "expected_updated_at": null}),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }

    /// Another job of the world's tenant (created through the API from the same template), prepared and wired to the
    /// test environment.
    pub async fn new_job(&self) -> ContractJob {
        let (layer, version) = self.template.clone();
        let q = req(
            "POST",
            "/api/jobs",
            Some(&super_admin()),
            json!({"template": format!("{layer}@v{version}")}),
        );
        let (s, b) = send(&self.e.app, self.tenant_header(q, self.f.tenant)).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        let key = b["key"].as_str().expect("JobSummary.key").to_owned();
        let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
            .bind(self.f.tenant)
            .bind(&key)
            .fetch_one(&self.e.owner)
            .await
            .unwrap();
        let f = design_contract::contract_job(self.f.tenant, id, &key);
        self.prepare(&f).await;
        self.wire(&f, "test").await;
        f
    }

    pub async fn put_policy(
        &self,
        f: &ContractJob,
        op: &str,
        policy: Value,
    ) -> (StatusCode, Value) {
        self.call(f, "PUT", &format!("tools/{op}/policy"), policy)
            .await
    }

    pub async fn policy_ok(&self, f: &ContractJob, op: &str, policy: Value) {
        let (s, b) = self.put_policy(f, op, policy).await;
        assert_eq!(s, StatusCode::OK, "policy of {op}: {b}");
    }

    /// The chain of the plan's A2 "Done when": `order.list` needs `customer_id` (from the CRM op), the CRM op needs the
    /// email and `auth_token` (from the IAM op), the IAM op asks the user for the code.
    pub async fn chain_policies(&self, f: &ContractJob) {
        self.policy_ok(f, VERIFY, verify_policy()).await;
        self.policy_ok(f, LOOKUP, lookup_policy()).await;
        self.policy_ok(f, READ, read_policy(true)).await;
    }

    /// `PUT toolsets/assistant/enabled` with `setting` (+ `enabled: true`).
    pub async fn role_setting(&self, f: &ContractJob, mut setting: Value) {
        setting["enabled"] = json!(true);
        let (s, b) = self
            .call(f, "PUT", "toolsets/assistant/enabled", setting)
            .await;
        assert_eq!(s, StatusCode::OK, "role setting: {b}");
    }

    // -- the connector ---------------------------------------------------------------------------------------------------------

    /// The tenant's non-stub instance `iam-<env>` on the template, pointing at the fake system, with its credential.
    pub async fn instance(&self, env: &str) {
        let id = format!("iam-{env}");
        let (s, b) = send(
            &self.e.app,
            req(
                "POST",
                "/api/connections/systems",
                Some(&self.im),
                json!({"id": id, "label": id, "template_key": TEMPLATE, "template_version": "1",
                       "env": env, "base_url": self.sys.url, "env_ids": {}, "max_rps": null, "stub": false}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        let (s, b) = send(
            &self.e.app,
            req(
                "PUT",
                &format!("/api/connections/systems/{id}/credential"),
                Some(&self.im),
                json!({"service_token": SERVICE_TOKEN}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }

    /// Route the world's operations of `f` in `env` to the tenant's instance `iam-<env>` (the instance is created on
    /// first use per environment); prod routes also get their green contract result (a route counts as ready only
    /// when tested).
    pub async fn wire(&self, f: &ContractJob, env: &str) {
        let id = format!("iam-{env}");
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM connector_instance WHERE tenant_id = $1 AND id = $2)",
        )
        .bind(f.tenant)
        .bind(&id)
        .fetch_one(&self.e.owner)
        .await
        .unwrap();
        if !exists {
            self.instance(env).await;
        }
        for op in OPS {
            let (object, verb) = op.split_once('.').unwrap();
            sqlx::query(
                "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, \
                                              instance_id, updated_by) \
                 VALUES ($1, $2, $3, $4, '*', $5, 'instance', $6, 'test') \
                 ON CONFLICT (tenant_id, job_id, object, verb, variant, env) \
                 DO UPDATE SET target_kind = 'instance', instance_id = EXCLUDED.instance_id",
            )
            .bind(f.tenant)
            .bind(f.job.job_id)
            .bind(object)
            .bind(verb)
            .bind(env)
            .bind(&id)
            .execute(&self.e.owner)
            .await
            .unwrap();
            if env == "prod" {
                sqlx::query(
                    "INSERT INTO connector_contract_result (tenant_id, instance_id, op, env, side_effect, outcome) \
                     VALUES ($1, $2, $3, 'prod', $4, 'green')",
                )
                .bind(f.tenant)
                .bind(&id)
                .bind(op)
                .bind(match op {
                    BOARD => "W",
                    CANCEL => "X",
                    _ => "R",
                })
                .execute(&self.e.owner)
                .await
                .unwrap();
            }
        }
    }

    // -- the roles ---------------------------------------------------------------------------------------------------------------

    /// The orchestrator (assistant on, granting `grants`), runner (real connector executor) and interaction roles of
    /// `f`'s `env` scope as `agent_runtime`. `real_switch`: the job's own role setting switches the assistant (so it
    /// must have been PUT before), else the assistant is on for the job.
    pub async fn daemons(&self, f: &ContractJob, env: Env, grants: &[&str], real_switch: bool) {
        let st = &self.e.state;
        let root = st.redis.clone().expect("REDIS_URL set");
        let scope = Scope::new(f.tenant, f.job.job_key.clone(), env).unwrap();
        let bus = root.scoped(&scope);
        let rt = common::runtime_pool(&self.e.owner).await;
        let cache = Arc::new(DesignCache::new(Duration::from_secs(600)));
        let mut runtime_state = st.clone();
        runtime_state.db = rt.clone();
        let switch: Arc<dyn RoleSwitch> = if real_switch {
            Arc::new(JobRoleSwitch)
        } else {
            Arc::new(On)
        };
        let assistant = Assistant::new(runtime_state.clone())
            .with_switch(switch)
            .with_toolset(Arc::new(Grants(
                grants.iter().map(|g| (*g).to_owned()).collect(),
            )));
        let orchestrator = Orchestrator::new(
            bus.clone(),
            rt.clone(),
            Arc::clone(&st.design),
            Arc::clone(&cache),
            StaticProcedures,
        )
        .unwrap()
        .with_consumer(format!("iam-orchestrator-{}", f.job.job_key))
        .with_consume(r::fast())
        .with_assistant(Arc::new(assistant));
        tokio::spawn(Arc::new(orchestrator).run());
        let runner = Runner::new(bus.clone(), runtime_state, Arc::clone(&cache))
            .unwrap()
            .with_consumer(format!("iam-runner-{}", f.job.job_key))
            .with_consume(r::fast());
        tokio::spawn(Arc::new(runner).run());
        let interaction = Arc::new(Interaction::new(rt, Arc::clone(&st.design), bus).unwrap());
        let consumer = format!("iam-interaction-{}", f.job.job_key);
        tokio::spawn(async move {
            interaction
                .run(&consumer, r::fast(), cache, Serving::default())
                .await
        });
    }

    // -- the staff test chat (env test) ---------------------------------------------------------------------------------------

    /// `startChat` as the test customer [`CUSTOMER_EMAIL`]: the conversation id.
    pub async fn start_chat(&self, f: &ContractJob) -> String {
        let (s, b) = self
            .call(
                f,
                "POST",
                "conversations",
                json!({"env": "test", "customer_email": CUSTOMER_EMAIL}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        b["conversation_id"].as_str().unwrap().to_owned()
    }

    /// `sendChatMessage`: 202 once stored and queued.
    pub async fn say(&self, f: &ContractJob, conv: &str, text: &str) {
        let (s, b) = self
            .call(
                f,
                "POST",
                &format!("conversations/{conv}/messages"),
                json!({"text": text}),
            )
            .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    }

    /// The agent lines of `conv` (`(case_id, text)`, in order), once there are at least `n` (30 s at most).
    pub async fn agent_lines(&self, conv: &str, n: usize) -> Vec<(Option<String>, String)> {
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            let rows: Vec<(Option<String>, String)> = sqlx::query_as(
                "SELECT case_id, text FROM conversation_message \
                  WHERE conversation_id = $1 AND role = 'agent' ORDER BY at_ms, message_id",
            )
            .bind(conv)
            .fetch_all(&self.e.owner)
            .await
            .unwrap();
            if rows.len() >= n || Instant::now() > until {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The A2 story on `f` (daemons running, the chain declared or attached): the user asks, the reply is the
    /// question, nothing reached the system yet, the user sends the code, the chain runs and the answer follows.
    /// The conversation id.
    pub async fn signin_flow(&self, f: &ContractJob) -> String {
        let before = self.sys.hits().len();
        let conv = self.start_chat(f).await;
        self.say(f, &conv, ASK).await;
        let lines = self.agent_lines(&conv, 1).await;
        assert_eq!(
            lines.first().map(|l| l.1.as_str()),
            Some(PROMPT),
            "the reply is the declared question: {lines:?}"
        );
        assert_eq!(
            self.sys.hits().len(),
            before,
            "nothing reached the system before the user answered"
        );
        self.say(f, &conv, CODE).await;
        let lines = self.agent_lines(&conv, 2).await;
        assert!(
            lines.get(1).is_some_and(|l| l.1.contains(ANSWER)),
            "the original tool ran and the model answered: {lines:?}"
        );
        conv
    }

    // -- database views -----------------------------------------------------------------------------------------------------------

    /// `(key, kind, value, sealed secret)` of the conversation's state, by key.
    pub async fn state(
        &self,
        f: &ContractJob,
        conv: &str,
    ) -> Vec<(String, String, Option<String>, Option<String>)> {
        sqlx::query_as(
            "SELECT key, kind, value, secret FROM conversation_state \
              WHERE job_id = $1 AND conversation_id = $2 ORDER BY key",
        )
        .bind(f.job.job_id)
        .bind(conv)
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
    }

    /// `(operation, state, outcome_code, phase)` of the job's `operation_call` rows, oldest first.
    pub async fn calls(&self, f: &ContractJob) -> Vec<(String, String, Option<String>, String)> {
        sqlx::query_as(
            "SELECT operation, state, outcome_code, COALESCE(step->>'phase', '') FROM operation_call \
              WHERE job_id = $1 ORDER BY started_at, call_id",
        )
        .bind(f.job.job_id)
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
    }

    /// `(role, text)` of the conversation's messages, in order.
    pub async fn messages(&self, f: &ContractJob, conv: &str) -> Vec<(String, String)> {
        sqlx::query_as(
            "SELECT role, text FROM conversation_message WHERE job_id = $1 AND conversation_id = $2 \
              ORDER BY at_ms, message_id",
        )
        .bind(f.job.job_id)
        .bind(conv)
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
    }

    /// `(reason, trigger)` of the job's handover items, once there is one (30 s at most).
    pub async fn handover(&self, f: &ContractJob) -> (String, String) {
        until("a handover item", || async {
            let rows: Vec<(String, String)> = sqlx::query_as(
                "SELECT reason, trigger FROM agent_handover_items WHERE tenant_id = $1 AND case_id IN \
                   (SELECT case_id FROM agent_case WHERE job_id = $2) ORDER BY queued_at",
            )
            .bind(f.tenant)
            .bind(f.job.job_id)
            .fetch_all(&self.e.owner)
            .await
            .unwrap();
            rows.into_iter().next()
        })
        .await
    }

    pub async fn count(&self, sql: &'static str, f: &ContractJob) -> i64 {
        sqlx::query_scalar(sql)
            .bind(f.job.job_id)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }

    // -- the live job (env prod) and its API key -------------------------------------------------------------------------------

    /// A LIVE job: `case.comment` declared as the board's op (`requires` nothing, `confirmation: auto`), the
    /// assistant switched on with `case_per_thread` and `role` merged in, the world's operations routed on prod, the
    /// draft released, the roles of the prod scope running (the job's own role setting switches the assistant), a
    /// live `interaction` heartbeat for the API, and an API key labelled "Kanban board". Async interactions with
    /// signed callbacks are on.
    pub async fn live(name: &str, script: Script, role: Value) -> (World, ApiKey) {
        let w = World::build(name, script, true).await;
        let f = &w.f;
        w.wire(f, "prod").await;
        w.policy_ok(f, BOARD, json!({"requires": [], "confirmation": "auto"}))
            .await;
        let mut setting = json!({"case_per_thread": true});
        if let (Some(dst), Some(src)) = (setting.as_object_mut(), role.as_object()) {
            dst.extend(src.clone());
        }
        w.role_setting(f, setting).await;
        b5_support::publish(&w.e, f).await;
        conv::serve(&w.e.state, f.tenant, &f.job.job_key, Env::Prod).await;
        w.daemons(f, Env::Prod, &[BOARD], true).await;
        let (s, b) = send(
            &w.e.app,
            req(
                "POST",
                &format!("/api/jobs/{}/api-keys", f.job.job_key),
                Some(&w.im),
                json!({"label": "Kanban board"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        let key = ApiKey {
            id: b["key"]["id"].as_str().unwrap().to_owned(),
            secret: b["secret"].as_str().unwrap().to_owned(),
        };
        (w, key)
    }

    /// `POST /api/jobs/{job}/interactions` with the API key.
    pub async fn keyed(&self, f: &ContractJob, key: &ApiKey, body: Value) -> (StatusCode, Value) {
        let r = Request::post(format!("/api/jobs/{}/interactions", f.job.job_key))
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {}", key.secret))
            .body(Body::from(body.to_string()))
            .unwrap();
        send(&self.e.app, r).await
    }

    /// The job's webhook secret (raw key bytes), as an integrations manager reads it.
    pub async fn webhook_secret(&self, f: &ContractJob) -> Vec<u8> {
        let (s, b) = send(
            &self.e.app,
            common::get(
                &format!("/api/jobs/{}/interactions/webhook-secret", f.job.job_key),
                Some(&self.im),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let shown = b["secret"].as_str().unwrap();
        base64::engine::general_purpose::STANDARD
            .decode(shown.strip_prefix("whsec_").expect("whsec_ prefix"))
            .unwrap()
    }
}

// ---- the callback receiver and the Standard Webhooks check ------------------------------------------------------------------

/// A local receiver: records `(headers, body)` of every callback and answers 200.
pub struct Receiver {
    pub url: String,
    rx: mpsc::UnboundedReceiver<(HeaderMap, String)>,
}

impl Receiver {
    pub async fn start() -> Receiver {
        let (tx, rx) = mpsc::unbounded_channel();
        let app = axum::Router::new().route(
            "/hook",
            post(move |h: HeaderMap, body: String| {
                let tx = tx.clone();
                async move {
                    let _ = tx.send((h, body));
                    StatusCode::OK
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        Receiver {
            url: format!("http://{addr}/hook"),
            rx,
        }
    }

    /// The next callback (90 s at most: a whole tool-using turn runs before it).
    pub async fn next(&mut self) -> (HeaderMap, String) {
        tokio::time::timeout(Duration::from_secs(90), self.rx.recv())
            .await
            .expect("a callback within 90 s")
            .expect("receiver open")
    }

    /// Nothing more arrives within `ms`.
    pub async fn quiet(&mut self, ms: u64) {
        let more = tokio::time::timeout(Duration::from_millis(ms), self.rx.recv()).await;
        assert!(more.is_err(), "no further callback");
    }
}

fn header<'a>(h: &'a HeaderMap, k: &str) -> &'a str {
    h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// The Standard Webhooks check a receiver runs: `v1,` + base64(HMAC-SHA256(secret, "{id}.{ts}.{body}")) and a fresh
/// timestamp.
pub fn verify_signature(secret: &[u8], h: &HeaderMap, body: &str) {
    let id = header(h, "webhook-id");
    let ts = header(h, "webhook-timestamp");
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    let tag = ring::hmac::sign(&key, format!("{id}.{ts}.{body}").as_bytes());
    let want = format!(
        "v1,{}",
        base64::engine::general_purpose::STANDARD.encode(tag)
    );
    assert_eq!(header(h, "webhook-signature"), want, "a valid signature");
    let ts: i64 = ts.parse().unwrap();
    assert!(
        (chrono::Utc::now().timestamp() - ts).abs() < 60,
        "fresh timestamp"
    );
}

// ---- external MCP systems (caller identity Part B) ----------------------------------------------------------------------------

impl World {
    /// The tenant's non-stub instance `id` on the published platform template `template_key@1`, pointing at
    /// `base_url`, with `credential` (the template's credential fields). Idempotent per tenant and id.
    pub async fn external_instance(
        &self,
        id: &str,
        template_key: &str,
        env: &str,
        base_url: &str,
        credential: Value,
    ) {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM connector_instance WHERE tenant_id = $1 AND id = $2)",
        )
        .bind(self.f.tenant)
        .bind(id)
        .fetch_one(&self.e.owner)
        .await
        .unwrap();
        if exists {
            return;
        }
        let (s, b) = send(
            &self.e.app,
            req(
                "POST",
                "/api/connections/systems",
                Some(&self.im),
                json!({"id": id, "label": id, "template_key": template_key, "template_version": "1",
                       "env": env, "base_url": base_url, "env_ids": {}, "max_rps": null, "stub": false}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        let (s, b) = send(
            &self.e.app,
            req(
                "PUT",
                &format!("/api/connections/systems/{id}/credential"),
                Some(&self.im),
                credential,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }

    /// Route the world's operations of `f` in `env` to the tenant's instance `instance_id` (any template), as
    /// [`World::wire`] routes them to `iam-<env>`.
    pub async fn route_ops_to(&self, f: &ContractJob, env: &str, instance_id: &str) {
        for op in OPS {
            let (object, verb) = op.split_once('.').unwrap();
            sqlx::query(
                "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, \
                                              instance_id, updated_by) \
                 VALUES ($1, $2, $3, $4, '*', $5, 'instance', $6, 'test') \
                 ON CONFLICT (tenant_id, job_id, object, verb, variant, env) \
                 DO UPDATE SET target_kind = 'instance', instance_id = EXCLUDED.instance_id",
            )
            .bind(f.tenant)
            .bind(f.job.job_id)
            .bind(object)
            .bind(verb)
            .bind(env)
            .bind(instance_id)
            .execute(&self.e.owner)
            .await
            .unwrap();
        }
    }
}
