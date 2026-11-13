//! The web chat customer stream on agent-api (crate::conversations::webchat_stream): its frames never reach the
//! staff job and conversation feeds, a stream holds no database connection, and the route is capped per process
//! ([`STREAM_SLOTS`]), and a staff reply's display name rides the customer's event. Its own test binary: the process-wide cap is filled here and must not starve other tests.
//! Real Postgres 18.
use crate::common;

use std::time::Duration;

use agent_api::channels::core::DeliverKind;
use agent_api::conversations::hub::Frame;
use agent_api::conversations::webchat_stream::{EVENT, STREAM_SLOTS, published};
use agent_api::interaction::{ConversationRef, Scope};
use axum::body::Body;
use axum::http::StatusCode;
use common::conv::{self, EDGE_KEY};
use common::{get, person};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

const WAIT: Duration = Duration::from_secs(5);

/// Read `body` until `want` arrived (or the stream ended / [`WAIT`] passed); everything read.
async fn read_until(body: &mut Body, want: &str) -> String {
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + WAIT;
    while !seen.contains(want) {
        let Ok(Some(frame)) = tokio::time::timeout_at(deadline, body.frame()).await else {
            break;
        };
        if let Ok(d) = frame.unwrap().into_data() {
            seen.push_str(&String::from_utf8_lossy(&d));
        }
    }
    seen
}

#[tokio::test]
async fn staff_feeds_never_carry_web_chat_customer_frames() {
    let c = conv::setup().await;
    let (id, _) = c.customer_case("wc-1", "m1", "Hello").await;
    let token = person("u-admin", &[]);
    for path in [
        "/api/jobs/j1/events".to_string(),
        format!("/api/jobs/j1/conversations/{id}/events"),
    ] {
        let r =
            c.e.app
                .clone()
                .oneshot(get(&path, Some(&token)))
                .await
                .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{path}");
        let mut body = r.into_body();
        let frame = |event: &'static str, text: &str| Frame {
            tenant: c.t,
            job_id: c.job,
            conversation_id: Some(id.clone()),
            event,
            data: json!({"conversation_id": id, "text": text}),
        };
        // the customer-facing copy first, then a staff frame: the feed shows the second only
        c.e.state
            .events
            .publish(frame(EVENT, "customer-facing copy"));
        c.e.state.events.publish(frame("owner", "staff sentinel"));
        let seen = read_until(&mut body, "staff sentinel").await;
        assert!(seen.contains("event: owner"), "{path}: {seen}");
        assert!(!seen.contains("customer-facing copy"), "{path}: {seen}");
        assert!(!seen.contains("event: webchat"), "{path}: {seen}");
    }
}

#[tokio::test]
async fn the_stream_holds_no_database_connection_and_is_capped_per_process() {
    let c = conv::setup().await;
    let (id, _) = c.customer_case("wc-1", "m1", "Hello").await;
    let q = format!("tenant={}&job_id={}&binding_id={}", c.t, c.job, c.wc);
    let url = format!("/api/internal/channel/webchat/conversations/{id}/events?{q}");
    let open = || tokio::time::timeout(WAIT, c.e.app.clone().oneshot(get(&url, Some(EDGE_KEY))));

    // more open streams than the pool has connections (10): each open still gets one, and the database still
    // answers while they all stay open
    let mut held = Vec::new();
    for n in 0..12 {
        let r = open()
            .await
            .unwrap_or_else(|_| panic!("stream {n} waited on the pool"))
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "stream {n}");
        held.push(r.into_body());
    }
    let (s, b) = tokio::time::timeout(
        WAIT,
        c.internal_get(&format!(
            "/api/internal/channel/conversations/{id}/messages?{q}"
        )),
    )
    .await
    .expect("a pooled connection is free while the streams are open");
    assert_eq!(s, StatusCode::OK, "{b}");
    drop(held);

    // a full process refuses at once, before touching the database
    let all = STREAM_SLOTS
        .try_acquire_many(STREAM_SLOTS.available_permits() as u32)
        .unwrap();
    let r = open().await.unwrap().unwrap();
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        r.headers().get("retry-after").and_then(|v| v.to_str().ok()),
        Some("5")
    );
    let b = r.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(conv::code(&v), "unavailable");
    drop(all);
    let r = open().await.unwrap().unwrap();
    assert_eq!(r.status(), StatusCode::OK, "a freed slot is usable again");
}

#[tokio::test]
async fn a_staff_reply_carries_its_display_name_to_the_customer_stream() {
    let c = conv::setup().await;
    let (id, _) = c.customer_case("wc-1", "m1", "Hello").await;
    let q = format!("tenant={}&job_id={}&binding_id={}", c.t, c.job, c.wc);
    let url = format!("/api/internal/channel/webchat/conversations/{id}/events?{q}");
    let r = tokio::time::timeout(WAIT, c.e.app.clone().oneshot(get(&url, Some(EDGE_KEY))))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let mut body = r.into_body();
    let s = Scope {
        tenant: c.t,
        job_id: c.job,
        job_key: "j1".to_string(),
    };
    let cref = ConversationRef {
        channel: "web_chat".into(),
        id: "wc-1".into(),
        route: Default::default(),
    };
    let st = &c.e.state;
    published(
        st,
        &s,
        &id,
        &cref,
        DeliverKind::Text,
        "From staff",
        Some(" Mara "),
        "m_1",
    )
    .await;
    published(
        st,
        &s,
        &id,
        &cref,
        DeliverKind::Text,
        "Blank name",
        Some("  "),
        "m_2",
    )
    .await;
    published(
        st,
        &s,
        &id,
        &cref,
        DeliverKind::HandoverNotice,
        "",
        None,
        "m_3",
    )
    .await;
    let seen = read_until(&mut body, "m_3").await;
    let events: Vec<serde_json::Value> = seen
        .lines()
        .filter_map(|l| l.strip_prefix("data:"))
        .filter_map(|d| serde_json::from_str(d.trim()).ok())
        .collect();
    let by_id = |i: &str| {
        events
            .iter()
            .find(|v| v["id"] == i)
            .cloned()
            .unwrap_or_else(|| panic!("no event {i} in {seen}"))
    };
    assert_eq!(by_id("m_1")["name"], "Mara", "{seen}");
    assert!(by_id("m_2").get("name").is_none(), "{seen}");
    assert_eq!(by_id("m_3")["kind"], "handover_notice", "{seen}");
    assert!(by_id("m_3").get("name").is_none(), "{seen}");
}
