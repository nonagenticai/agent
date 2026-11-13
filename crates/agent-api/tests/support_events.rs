//! Staff Workspace T7: the tenant-wide live stream (`GET /api/support/events`, crate::support::events) and presence
//! (`POST /api/jobs/{job}/conversations/{id}/presence`, crate::support::presence). The stream rides the per-job Redis
//! bridge: a customer message reaches it as `message` and `inbox_row` frames naming the job; without Redis it is 503
//! `stream_unavailable`. Presence reaches colleagues on the stream, and `viewing` marks the conversation read for the
//! viewer only. Real Postgres 18 and the shared test Redis.
use crate::common;
use crate::redis_env;

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::StatusCode;
use common::conv::{self, Ctx, code};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

const WAIT: Duration = Duration::from_secs(5);

/// Open the tenant stream as `sub`.
async fn open(c: &Ctx, sub: &str) -> Body {
    let token = common::person(sub, &[]);
    let r = tokio::time::timeout(
        WAIT,
        c.e.app
            .clone()
            .oneshot(common::get("/api/support/events", Some(&token))),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    r.into_body()
}

/// What the stream sent until `want` appears (or [`WAIT`] passes).
async fn read_until(body: &mut Body, want: &str) -> String {
    read_until_matching(body, |seen| seen.contains(want)).await
}

/// Read complete frames until their contents satisfy `want` (or [`WAIT`] passes).
async fn read_until_matching(body: &mut Body, want: impl Fn(&str) -> bool) -> String {
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + WAIT;
    while !want(&seen) {
        let Ok(Some(frame)) = tokio::time::timeout_at(deadline, body.frame()).await else {
            break;
        };
        if let Ok(d) = frame.unwrap().into_data() {
            seen.push_str(&String::from_utf8_lossy(&d));
        }
    }
    seen
}

/// The `data:` of every `event` frame in `seen`, parsed.
fn frames(seen: &str, event: &str) -> Vec<Value> {
    seen.split("\n\n")
        .filter(|block| block.lines().any(|l| l == format!("event: {event}")))
        .filter_map(|block| {
            let data: String = block
                .lines()
                .filter_map(|l| l.strip_prefix("data: "))
                .collect();
            serde_json::from_str(&data).ok()
        })
        .collect()
}

#[tokio::test]
async fn events_deliver_message_within_2s() {
    let vars = redis_env::api_vars("sw-events").await;
    let c = conv::setup_with(|v| v.extend(vars)).await;
    // routing off: the open stream makes u-agent online and a tenant with the platform flag on (SW-7:
    // `tenants.support_auto_assign`, OFF by default, so nothing assigns here as it stands) would round-robin the
    // handover to them before the Take below; kept explicit so the test does not depend on that default
    let (s, b) = c
        .call(
            "PUT",
            "/api/support/routing",
            "u-mgr",
            json!({"mode": "off"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut body = open(&c, "u-agent").await;
    let t0 = Instant::now();
    let (s, b) = c
        .inbound("wc-live", "wc-live-1", "Can I speak to a real person?")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let conv_id = b["conversation_id"].as_str().unwrap().to_string();
    let seen = read_until(&mut body, "event: inbox_row").await;
    let took = t0.elapsed();
    assert!(seen.contains("event: inbox_row"), "{seen}");
    assert!(
        took < Duration::from_millis(2000),
        "message + inbox_row took {took:?}"
    );
    let msg = frames(&seen, "message");
    let m = msg
        .iter()
        .find(|m| m["conversation_id"] == conv_id.as_str())
        .unwrap_or_else(|| panic!("no message frame: {seen}"));
    assert_eq!(m["job"], "j1", "{m}");
    assert_eq!(m["kind"], "customer", "the MessageView fields: {m}");
    assert!(
        m["at"].is_string() && m.get("client_msg_id").is_some(),
        "{m}"
    );
    let row = frames(&seen, "inbox_row")
        .into_iter()
        .find(|r| r["conversation_id"] == conv_id.as_str())
        .unwrap_or_else(|| panic!("no inbox_row frame: {seen}"));
    assert_eq!(row["job"], "j1", "{row}");
    // The handover stores its notice in the same transaction as the customer's message. The frame carries
    // that customer message; the inbox row carries the latest transcript line, which is the agent's notice.
    assert_eq!(row["last_message"]["kind"], "agent", "{row}");
    assert_eq!(
        row["last_message"]["text"],
        "I've passed this conversation to a colleague, who will reply here.",
        "{row}"
    );
    assert!(row["display_name"].is_string(), "{row}");
    // A follow-up on the queued conversation has no new notice: both its frame and the inbox preview must
    // carry the new customer text. A pending handover frame may arrive first, so wait for both projections.
    let text = "One more detail.";
    let t1 = Instant::now();
    let (s, b) = c.inbound("wc-live", "wc-live-2", text).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["notice"], false, "{b}");
    let updated = |seen: &str| {
        frames(seen, "message").iter().any(|m| {
            m["conversation_id"] == conv_id && m["kind"] == "customer" && m["text"] == text
        }) && frames(seen, "inbox_row").iter().any(|r| {
            r["conversation_id"] == conv_id
                && r["last_message"]["kind"] == "customer"
                && r["last_message"]["text"] == text
        })
    };
    let seen = read_until_matching(&mut body, updated).await;
    assert!(updated(&seen), "{seen}");
    assert!(
        t1.elapsed() < Duration::from_millis(2000),
        "follow-up message + inbox_row took {:?}",
        t1.elapsed()
    );
    // the owner change of a Take arrives as `owner` (with the job) and a fresh row
    let (s, t) = c.take(&conv_id, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{t}");
    let seen = read_until(&mut body, r#""owner":"user:u-agent""#).await;
    let owner = frames(&seen, "owner");
    assert!(
        owner
            .iter()
            .any(|o| o["job"] == "j1" && o["owner"] == "user:u-agent"),
        "{seen}"
    );
}

#[tokio::test]
async fn events_503_without_redis() {
    let c = conv::setup().await;
    let (s, b) = c.get("/api/support/events", "u-agent").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(code(&b), "stream_unavailable", "{b}");
    let (s, b) = c.get("/api/support/events", "u-obs").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
}

#[tokio::test]
async fn presence_reaches_colleagues_and_viewing_marks_read() {
    let vars = redis_env::api_vars("sw-presence").await;
    let c = conv::setup_with(|v| v.extend(vars)).await;
    let (s, b) = c
        .inbound("wc-p", "wc-p-1", "Can I speak to a real person?")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let conv_id = b["conversation_id"].as_str().unwrap().to_string();
    let presence = format!("/api/jobs/j1/conversations/{conv_id}/presence");
    // a customer message nobody has viewed is unread for everyone
    for sub in ["u-agent", "u-agent2"] {
        let (_, inbox) = c.get("/api/support/inbox?tab=unassigned", sub).await;
        assert_eq!(inbox["rows"][0]["unread"], true, "{sub}: {inbox}");
    }
    let mut body = open(&c, "u-agent2").await;
    let (s, b) = c
        .call("POST", &presence, "u-agent", json!({"state": "viewing"}))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let seen = read_until(&mut body, "event: presence").await;
    let p = frames(&seen, "presence");
    let p = p
        .first()
        .unwrap_or_else(|| panic!("no presence frame: {seen}"));
    assert_eq!(
        (&p["conversation_id"], &p["job"], &p["sub"], &p["state"]),
        (
            &json!(conv_id),
            &json!("j1"),
            &json!("u-agent"),
            &json!("viewing")
        ),
        "{p}"
    );
    assert_eq!(p["name"], "u-agent", "{p}");
    // read for the viewer only
    let (_, inbox) = c.get("/api/support/inbox?tab=unassigned", "u-agent").await;
    assert_eq!(inbox["rows"][0]["unread"], false, "{inbox}");
    let (_, inbox) = c.get("/api/support/inbox?tab=unassigned", "u-agent2").await;
    assert_eq!(inbox["rows"][0]["unread"], true, "{inbox}");
    // typing is published too; anything else is refused; so is a member without a staff role
    let (s, b) = c
        .call("POST", &presence, "u-agent", json!({"state": "typing"}))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let seen = read_until(&mut body, r#""state":"typing""#).await;
    let typing = frames(&seen, "presence")
        .into_iter()
        .find(|p| p["state"] == "typing")
        .unwrap_or_else(|| panic!("no typing frame: {seen}"));
    // published without a name; the stream names it from the earlier viewing frame
    assert_eq!(typing["name"], "u-agent", "{typing}");
    assert_eq!(typing["job"], "j1", "{typing}");
    let (s, b) = c
        .call("POST", &presence, "u-agent", json!({"state": "dancing"}))
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_presence", "{b}");
    let (s, b) = c
        .call("POST", &presence, "u-obs", json!({"state": "viewing"}))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "support_agent", "{b}");
    let (s, b) = c
        .call(
            "POST",
            "/api/jobs/j1/conversations/nope/presence",
            "u-agent",
            json!({"state": "viewing"}),
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}
