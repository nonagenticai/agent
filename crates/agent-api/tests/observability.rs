//! B1 T2: request ids, route-template metrics, the metrics listener split, upstream counters and what a log line
//! may carry. Real Postgres 18 (one container for this binary).
mod common;

use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use agent_api::observability;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{get, insert_tenant, insert_user, person, send, super_admin};
use http_body_util::BodyExt;
use tower::ServiceExt;
use uuid::Uuid;

async fn request_id_of(app: &axum::Router, req: Request<Body>) -> String {
    let resp = app.clone().oneshot(req).await.unwrap();
    resp.headers()
        .get("x-request-id")
        .expect("every response carries x-request-id")
        .to_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn request_id_is_echoed_or_generated() {
    let e = common::env().await;
    let with = Request::get("/health")
        .header("x-request-id", "abc-1")
        .body(Body::empty())
        .unwrap();
    assert_eq!(request_id_of(&e.app, with).await, "abc-1");
    // absent: a fresh uuid (v7)
    let id = request_id_of(&e.app, get("/health", None)).await;
    let u = Uuid::parse_str(&id).expect("a generated request id is a uuid");
    assert_eq!(u.get_version_num(), 7, "{id}");
    // malformed (too long, or outside [A-Za-z0-9-]): replaced, never echoed
    for bad in ["x".repeat(65), "a b".to_string(), "a\"b".to_string()] {
        let req = Request::get("/health")
            .header("x-request-id", bad.as_str())
            .body(Body::empty())
            .unwrap();
        let id = request_id_of(&e.app, req).await;
        assert_ne!(id, bad);
        assert!(Uuid::parse_str(&id).is_ok(), "{id}");
    }
    // errors carry it too
    let id = request_id_of(&e.app, get("/api/nope", None)).await;
    assert!(Uuid::parse_str(&id).is_ok());
}

#[tokio::test]
async fn metrics_label_is_the_route_template() {
    let h = observability::metrics_handle();
    let e = common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    let uri = format!("/api/tenants/{t}/users");
    let (s, b) = send(&e.app, get(&uri, Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let text = h.render();
    let line = text
        .lines()
        .find(|l| {
            l.starts_with("agent_http_requests_total{")
                && l.contains("route=\"/api/tenants/{tenant_id}/users\"")
                && l.contains("status=\"200\"")
        })
        .unwrap_or_else(|| panic!("no request counter for the template:\n{text}"));
    assert!(line.contains("method=\"GET\""), "{line}");
    assert!(
        !text.contains(&t.to_string()),
        "a raw path (tenant id) leaked into a label"
    );
    assert!(text.contains("agent_http_request_duration_seconds_bucket{"));
    assert!(text.contains("agent_build_info{version=\""), "{text}");
    let info = text
        .lines()
        .find(|l| l.starts_with("agent_build_info{"))
        .expect("agent_build_info line");
    assert!(info.contains("commit=\""), "{info}");
}

#[tokio::test]
async fn metrics_are_not_served_on_the_api_port() {
    let _ = observability::metrics_handle();
    let e = common::env().await;
    let (s, b) = send(&e.app, get("/metrics", None)).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["code"], "not_found");
    // control: the metrics router serves it
    let m = observability::metrics_router(observability::metrics_handle());
    let resp = m.oneshot(get("/metrics", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("agent_build_info"), "{text}");
}

#[tokio::test]
async fn upstream_wrapper_counts_outcomes() {
    let h = observability::metrics_handle();
    for _ in 0..2 {
        let r: Result<u8, &str> =
            observability::upstream("test_upstream", "op_counted", async { Ok(1) }).await;
        assert_eq!(r, Ok(1));
    }
    let r: Result<u8, &str> =
        observability::upstream("test_upstream", "op_counted", async { Err("no") }).await;
    assert_eq!(r, Err("no"), "the wrapper returns the call's own result");
    let text = h.render();
    let value = |outcome: &str| -> Option<f64> {
        text.lines()
            .find(|l| {
                l.starts_with("agent_upstream_requests_total{")
                    && l.contains("upstream=\"test_upstream\"")
                    && l.contains("op=\"op_counted\"")
                    && l.contains(&format!("outcome=\"{outcome}\""))
            })
            .and_then(|l| l.rsplit(' ').next())
            .and_then(|v| v.parse().ok())
    };
    assert_eq!(value("ok"), Some(2.0), "{text}");
    assert_eq!(value("error"), Some(1.0), "{text}");
    assert!(text.contains("agent_upstream_request_duration_seconds_bucket{"));
}

/// A writer the capture subscriber appends to.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The one log capture of this binary: installed as the process-wide GLOBAL default, once.
///
/// Why global (B1b T7): a `set_default` (thread-scoped) capture is the only registered dispatcher, so tracing-core
/// keeps `has_just_one` and computes a callsite's interest from the default of whichever thread registers it first.
/// Another test thread (no subscriber) that hits `http_request` first caches it as `never`, process-wide, and the
/// capture silently sees nothing (the old flake). With one global dispatcher every thread computes the same interest,
/// and the cache is rebuilt once after install for any callsite registered before it.
fn capture() -> Captured {
    static CAPTURE: OnceLock<Captured> = OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let buf = Captured::default();
            let w = buf.clone();
            let sub = tracing_subscriber::fmt()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false)
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || w.clone())
                .finish();
            tracing::subscriber::set_global_default(sub)
                .expect("the only global subscriber of this binary");
            buf
        })
        .clone()
}

/// The captured lines of request `rid`. The global capture also holds other tests' lines, so the leak check is
/// scoped to this request's own lines (every line inside its `http` span carries its `request_id`); an event logged
/// outside that span is not covered here.
fn lines_of(c: &Captured, rid: &str) -> Vec<String> {
    let out = String::from_utf8(c.0.lock().unwrap().clone()).unwrap();
    out.lines()
        .filter(|l| l.contains(rid))
        .map(str::to_string)
        .collect()
}

/// Which of `secrets` appear in `lines`.
fn leaks<'a>(lines: &[String], secrets: &[&'a str]) -> Vec<&'a str> {
    secrets
        .iter()
        .copied()
        .filter(|s| lines.iter().any(|l| l.contains(s)))
        .collect()
}

#[tokio::test]
async fn log_line_has_no_query_string_or_sub() {
    let cap = capture();
    let e = common::env().await;
    // any callsite another thread registered before the global install is re-evaluated against it
    tracing::callsite::rebuild_interest_cache();
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-secret-subject", t, "tenant_user").await;
    let rid = format!("b1b-t7-{}", Uuid::new_v4().simple());
    let uri = format!("/api/tenants/{t}/users?email=ann%40globex.com&token=tok-secret-123");
    let mut req = get(&uri, Some(&person("u-secret-subject", &[])));
    req.headers_mut()
        .insert("x-request-id", rid.parse().unwrap());
    let (s, b) = send(&e.app, req).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let lines = lines_of(&cap, &rid);
    let line = lines
        .iter()
        .find(|l| l.contains("http_request"))
        .unwrap_or_else(|| panic!("no http_request line for {rid}:\n{lines:#?}"));
    // control: the line is the request's, with the template and status
    assert!(line.contains("/api/tenants/{tenant_id}/users"), "{line}");
    assert!(line.contains("\"status\":200"), "{line}");
    let tid = t.to_string();
    let secrets = [
        "u-secret-subject",
        "globex",
        "tok-secret-123",
        "email=",
        tid.as_str(),
    ];
    assert_eq!(leaks(&lines, &secrets), Vec::<&str>::new(), "{lines:#?}");
    // mutation check of the assertion itself: had the layer logged the raw URI (query string included) or the
    // subject on this request's line, the same check would name each leak
    let mutated = vec![format!(
        "{line} {{\"uri\":\"{uri}\",\"sub\":\"u-secret-subject\"}}"
    )];
    assert_eq!(leaks(&mutated, &secrets), secrets.to_vec());
}
