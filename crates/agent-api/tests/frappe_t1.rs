//! B7 T16: the platform template `frappe_helpdesk@1.31` against responses recorded from the live site
//! (Frappe 15.123.1 + Helpdesk 1.31.0 at `http://frappe.example.com:8000`, 2026-10-21; redacted:
//! every email is `@example.com`, no key material) in `tests/fixtures/frappe/`.
//!
//! Per op: the template renders exactly the request that was sent live (method, path, body), the recorded response
//! is replayed through the stub transport (selected by name with the stub fixture header) and the mapping engine
//! maps it to the canonical outputs / errors. R10: `case.create` raises the ticket for `customer_email` when given,
//! else for the configured service user (`env.requester_id`).
use std::collections::BTreeMap;
use std::path::PathBuf;

use agent_api::connectors::mapping::{RenderCtx, extract, reduce, render};
use agent_api::connectors::model::{ConnectorTemplate, ConnectorTemplateOp, TemplateStatus};
use agent_api::connectors::opkey::op_key;
use agent_api::connectors::tenant_templates::{def_of, op_defs};
use agent_api::connectors::transport::stub::{FIXTURE_HEADER, replay};
use agent_api::connectors::validate::validate_template;
use agent_api::connectors::wire::{OpError, RenderedRequest};
use serde_json::{Map, Value, json};

const KEY: &str = "frappe_helpdesk";
const VERSION: &str = "1.31";
const SERVICE_USER: &str = "agent-connector@example.com";

fn manifest_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn read_json(rel: &str) -> Value {
    let path = manifest_path(rel);
    let text =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
}

fn recorded(name: &str) -> Value {
    read_json(&format!("tests/fixtures/frappe/{name}.json"))
}

fn template() -> ConnectorTemplate {
    serde_json::from_value(read_json("tests/fixtures/frappe/template.json"))
        .expect("frappe template deserializes")
}

fn op<'t>(t: &'t ConnectorTemplate, key: &str) -> &'t ConnectorTemplateOp {
    t.ops
        .iter()
        .find(|o| op_key(&o.op_ref()) == key)
        .unwrap_or_else(|| panic!("{KEY}@{VERSION} has {key}"))
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        other => panic!("not an object: {other}"),
    }
}

/// The instance env ids of the live test instance.
fn env() -> Map<String, Value> {
    obj(json!({
        "requester_id": SERVICE_USER,
        "default_ticket_type": "Question",
        "complaint_ticket_type": "Question",
    }))
}

fn rendered(
    op: &ConnectorTemplateOp,
    canonical: Value,
    env: &Map<String, Value>,
    key: &str,
) -> RenderedRequest {
    let canonical = obj(canonical);
    let empty = Map::new();
    let ctx = RenderCtx {
        canonical: &canonical,
        config: &empty,
        env,
        secret: &empty,
        idempotency_key: key,
    };
    render(op, &ctx).expect("the op renders")
}

/// The rendered request is the one recorded live: method, path (`%20` is how the transport sends the space the
/// recording shows) and JSON body.
fn assert_request(req: &RenderedRequest, rec: &Value) {
    let r = &rec["request"];
    assert_eq!(json!(req.method), r["method"], "method");
    let full = if req.query.is_empty() {
        req.path.clone()
    } else {
        let q: Vec<String> = req.query.iter().map(|(k, v)| format!("{k}={v}")).collect();
        format!("{}?{}", req.path, q.join("&"))
    };
    assert_eq!(json!(full.replace("%20", " ")), r["path"], "path");
    let body = req.body.clone().unwrap_or(Value::Null);
    assert_eq!(body, r["body"], "body");
}

/// Replays the recorded response through the stub transport and maps it (extract, then reduce to the op's B6
/// outputs).
fn replayed(
    op: &ConnectorTemplateOp,
    req: &RenderedRequest,
    name: &str,
) -> Result<Map<String, Value>, OpError> {
    let rec = recorded(name);
    let fixture = json!({"name": name, "status": rec["status"], "body": rec["body"]});
    let mut req = req.clone();
    req.headers
        .push((FIXTURE_HEADER.to_owned(), name.to_owned()));
    let raw = replay(&req, &[fixture]).expect("the stub replays the recording");
    let outputs = def_of(op_defs(), &op.op_ref())
        .map(|d| d.output_fields.clone())
        .unwrap_or_default();
    extract(op, &BTreeMap::new(), &raw).map(|m| reduce(m, &outputs))
}

#[test]
fn the_template_is_active_and_validates_clean() {
    let t = template();
    assert_eq!(t.status, TemplateStatus::Active, "verified live → active");
    let issues = validate_template(&t, op_defs());
    assert!(issues.is_empty(), "{issues:?}");
    for key in [
        "case.create",
        "case.create{complaint}",
        "case.read",
        "case.comment",
    ] {
        op(&t, key);
    }
}

#[test]
fn case_create_for_the_customer() {
    let t = template();
    let o = op(&t, "case.create");
    let rec = recorded("create");
    let req = rendered(
        o,
        json!({
            "subject": "[b7-t16] customer ticket",
            "description": "Recorded by B7 T16 live verification.",
            "customer_email": "b7t16-customer@example.com",
        }),
        &env(),
        "b7t16-1792573982-a",
    );
    assert_request(&req, &rec);
    let out = replayed(o, &req, "create").expect("created");
    assert_eq!(out.get("case_id"), Some(&json!("0003")));
    assert_eq!(out.get("status"), Some(&json!("open")));
}

#[test]
fn case_create_without_a_customer_is_raised_by_the_service_user() {
    let t = template();
    let o = op(&t, "case.create");
    let rec = recorded("create_service_user");
    let mut env = env();
    env.insert("default_ticket_type".into(), json!("Unspecified"));
    // customer_email absent, and present but null
    for null_email in [false, true] {
        let mut canonical = json!({
            "subject": "[b7-t16] service-user ticket",
            "description": "Recorded by B7 T16 live verification (no customer email).",
        });
        if null_email {
            canonical["customer_email"] = Value::Null;
        }
        let req = rendered(o, canonical, &env, "b7t16-1792573982-b");
        assert_request(&req, &rec);
        assert_eq!(
            req.body.as_ref().and_then(|b| b.get("raised_by")),
            Some(&json!(SERVICE_USER))
        );
        let out = replayed(o, &req, "create_service_user").expect("created");
        assert_eq!(out.get("case_id"), Some(&json!("0004")));
        assert_eq!(out.get("status"), Some(&json!("open")));
    }
}

#[test]
fn a_duplicate_agent_key_is_417_invalid_input() {
    // custom_agent_key is unique: Frappe answers 417 UniqueValidationError (not 409); the dup_search before the
    // insert is what keeps a retry from getting here.
    let t = template();
    let o = op(&t, "case.create");
    let req = rendered(
        o,
        json!({"subject": "[b7-t16] dup key", "description": "x", "customer_email": "b7t16-customer@example.com"}),
        &env(),
        "b7t16-1792573982-a",
    );
    assert_eq!(recorded("create_duplicate_key")["status"], json!(417));
    let e = replayed(o, &req, "create_duplicate_key").expect_err("417");
    assert_eq!(e.code, "invalid_input");
}

#[test]
fn case_read_and_the_status_map() {
    let t = template();
    let o = op(&t, "case.read");
    let req = rendered(o, json!({"case_id": "0003"}), &env(), "");
    assert_request(&req, &recorded("read"));
    let out = replayed(o, &req, "read").expect("read");
    assert_eq!(out.get("status"), Some(&json!("open")));

    // Resolved / Closed as the live PUTs answered them, through the read op's field map.
    for (name, want) in [
        ("update_status", "resolved"),
        ("update_status_closed", "closed"),
    ] {
        let out = replayed(o, &req, name).expect(name);
        assert_eq!(out.get("status"), Some(&json!(want)), "{name}");
    }
    // Every system status Helpdesk 1.31 ships (ticket_statuses.json), plus Paused for sites that add it.
    let shipped: Vec<String> = recorded("ticket_statuses")["body"]["data"]
        .as_array()
        .expect("statuses")
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_owned))
        .collect();
    assert_eq!(shipped, ["Open", "Replied", "Resolved", "Closed"]);
    let base = recorded("read");
    for (system, want) in [
        ("Open", "open"),
        ("Replied", "pending_customer"),
        ("Paused", "pending_provider"),
        ("Resolved", "resolved"),
        ("Closed", "closed"),
    ] {
        let mut body = base["body"].clone();
        body["data"]["status"] = json!(system);
        let raw = agent_api::connectors::wire::RawResponse {
            status: 200,
            body,
            retry_after_secs: None,
        };
        let out = extract(o, &BTreeMap::new(), &raw).expect(system);
        assert_eq!(out.get("status"), Some(&json!(want)), "{system}");
    }
}

#[test]
fn case_comment_is_an_hd_ticket_comment() {
    let t = template();
    let o = op(&t, "case.comment");
    let req = rendered(
        o,
        json!({"case_id": "0003", "comment": "[b7-t16] agent comment"}),
        &env(),
        "b7t16-1792573982-cm",
    );
    assert_request(&req, &recorded("comment"));
    replayed(o, &req, "comment").expect("commented");
}

#[test]
fn not_found_and_auth_failures() {
    let t = template();
    let o = op(&t, "case.read");
    let missing = rendered(o, json!({"case_id": "999999"}), &env(), "");
    assert_request(&missing, &recorded("not_found"));
    assert_eq!(
        replayed(o, &missing, "not_found").expect_err("404").code,
        "not_found"
    );
    let req = rendered(o, json!({"case_id": "0003"}), &env(), "");
    assert_request(&req, &recorded("auth_401"));
    assert_eq!(
        replayed(o, &req, "auth_401").expect_err("401").code,
        "connector_auth_failed"
    );
}

#[test]
fn fixtures_are_redacted() {
    let dir = manifest_path("tests/fixtures/frappe");
    for entry in std::fs::read_dir(&dir).expect("fixture dir") {
        let path = entry.expect("entry").path();
        // the template definition names its credential fields (`api_secret`); only recorded responses are scanned
        if path.file_name().is_some_and(|n| n == "template.json") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read fixture");
        for word in text.split(|c: char| c.is_whitespace() || "\"'<>,;()[]{}".contains(c)) {
            if let Some((_, domain)) = word.split_once('@') {
                assert!(
                    domain.trim_end_matches(|c: char| !c.is_ascii_alphanumeric()) == "example.com"
                        || !domain.contains('.'),
                    "{}: unredacted email {word}",
                    path.display()
                );
            }
        }
        let lower = text.to_ascii_lowercase();
        for banned in ["api_secret", "authorization", "\"sid\"", "set-cookie"] {
            assert!(!lower.contains(banned), "{}: {banned}", path.display());
        }
    }
}
