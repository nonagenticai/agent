//! B4 T10 — the Redis bridge into B12's event hub (crate::sse_hub): one Pub/Sub connection per process with
//! ref-counted SUBSCRIBE, `bus` / `ack` frames from another process on the job feed, conversation filtering, a
//! lagging client closed, `Last-Event-ID` replay and the JSON `busBacklog` from `console:recent`, tenant isolation,
//! and the feeds without Redis (local hub only).
use std::time::{Duration, Instant};

use agent_api::conversations::hub::{EventHub, Frame, LocalHub};
use agent_api::runtime::bus::Envelope;
use agent_api::runtime::{Bus, Env, Scope, Streams};
use agent_api::sse_hub::{self, Upstream};
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::StreamExt;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

use crate::common::{self, TestEnv};
use crate::redis_env as r;

const WAIT: Duration = Duration::from_secs(5);

struct Job {
    e: TestEnv,
    t: Uuid,
    job: Uuid,
    /// The env's `AGENT_PREFIX`.
    prefix: String,
}

async fn job_in(e: &TestEnv, t: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, 'J', '[]', 'test') RETURNING id",
    )
    .bind(t)
    .bind(key)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

/// An env on the shared Redis (its own prefix): tenant with observer `u-ev` and job `j1`.
async fn setup(test: &str) -> Job {
    let vars = r::api_vars(test).await;
    let prefix = vars[1].1.clone();
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, "Events Co").await;
    common::insert_user(&e.owner, "u-ev", t, "tenant_user").await;
    let job = job_in(&e, t, "j1").await;
    Job { e, t, job, prefix }
}

/// A bus of another process (its own connection) on `tenant`'s `j1` in `env`.
async fn other_process(prefix: &str, tenant: Uuid, env: Env) -> Bus {
    Bus::connect_with_response_timeout(&r::url().await, prefix, Duration::from_secs(5))
        .await
        .unwrap()
        .scoped(&Scope::new(tenant, "j1", env).unwrap())
}

fn op_call(cid: &str, at_ms: u64) -> Envelope {
    let mut env = Envelope::new(
        "op.call",
        "orchestrator",
        cid,
        json!({"call_id": cid, "operation": "order.read", "inputs": {"email": "femke@example.com"}}),
    )
    .unwrap();
    env.published_at_ms = at_ms;
    env
}

/// XADD an `op.call` on `bus:op` (mirrored on the console); its entry id.
async fn publish_op(bus: &Bus, cid: &str, at_ms: u64) -> String {
    bus.publish(&bus.streams.op(), &op_call(cid, at_ms))
        .await
        .unwrap()
}

/// The runner acks `entry` (mirrored on the console as an `ack` frame).
async fn runner_acks(bus: &Bus, entry: &str, cid: &str) {
    let stream = bus.streams.op();
    bus.ensure_group(&stream, "runner").await.unwrap();
    bus.ack(&stream, "runner", entry, Some(&op_call(cid, 0)))
        .await
        .unwrap();
}

async fn numsub(bus: &Bus, channel: &str) -> i64 {
    let (_, n): (String, i64) = redis::cmd("PUBSUB")
        .arg("NUMSUB")
        .arg(channel)
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    n
}

/// One SSE event (also read by `e2e_m1`).
#[derive(Debug)]
pub struct Ev {
    pub event: String,
    pub id: Option<String>,
    pub data: Value,
}

/// An open SSE response body.
pub struct Feed {
    body: Body,
    buf: String,
}

impl Feed {
    pub fn new(body: Body) -> Feed {
        Feed {
            body,
            buf: String::new(),
        }
    }

    /// The first body chunk as text (comments included); `None` when nothing arrives within `wait`.
    pub async fn first_chunk(&mut self, wait: Duration) -> Option<String> {
        let f = tokio::time::timeout(wait, self.body.frame())
            .await
            .ok()??
            .ok()?;
        let bytes = f.into_data().ok()?;
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// The next event (keep-alive comments skipped); `None` when the stream ends or `wait` passes.
    pub async fn next(&mut self, wait: Duration) -> Option<Ev> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(i) = self.buf.find("\n\n") {
                let block: String = self.buf.drain(..i + 2).collect();
                let mut ev = Ev {
                    event: "message".into(),
                    id: None,
                    data: Value::Null,
                };
                let mut data = String::new();
                let mut has_data = false;
                for line in block.lines() {
                    if let Some(v) = line.strip_prefix("event:") {
                        ev.event = v.trim().to_string();
                    } else if let Some(v) = line.strip_prefix("id:") {
                        ev.id = Some(v.trim().to_string());
                    } else if let Some(v) = line.strip_prefix("data:") {
                        data.push_str(v.strip_prefix(' ').unwrap_or(v));
                        has_data = true;
                    }
                }
                if !has_data {
                    continue;
                }
                ev.data = serde_json::from_str(&data).unwrap_or(Value::String(data));
                return Some(ev);
            }
            let f = tokio::time::timeout_at(deadline, self.body.frame())
                .await
                .ok()??
                .ok()?;
            if let Ok(bytes) = f.into_data() {
                self.buf.push_str(&String::from_utf8_lossy(&bytes));
            }
        }
    }
}

/// `GET path` as SSE for member `sub` (with `Last-Event-ID` when given): the status and, on 200, the body.
async fn open(
    app: &Router,
    path: &str,
    sub: &str,
    last: Option<&str>,
) -> (StatusCode, Option<Feed>) {
    let mut b = Request::get(path)
        .header(
            "authorization",
            format!("Bearer {}", common::person(sub, &[])),
        )
        .header("accept", "text/event-stream");
    if let Some(l) = last {
        b = b.header("last-event-id", l);
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let feed = (status == StatusCode::OK).then(|| Feed::new(resp.into_body()));
    (status, feed)
}

fn message(t: Uuid, j: Uuid, conv: &str, n: u64) -> Frame {
    Frame {
        tenant: t,
        job_id: j,
        conversation_id: Some(conv.into()),
        event: "message",
        data: json!({ "n": n }),
    }
}

#[tokio::test]
async fn hundred_clients_one_pubsub_connection() {
    let j = setup("hundred").await;
    let bridge = j.e.state.sse.clone().expect("REDIS_URL set: the bridge");
    let mut rx = j.e.state.events.subscribe();
    let mut subs = Vec::new();
    for _ in 0..100 {
        subs.push(bridge.acquire(j.t, j.job, "j1").await.unwrap());
    }
    assert_eq!(bridge.connections_opened(), 1, "one Pub/Sub connection");
    let probe = r::root("hundred-probe").await;
    for env in Env::ALL {
        let s = Streams::scoped(&j.prefix, &Scope::new(j.t, "j1", env).unwrap());
        for ch in [s.console(), s.chat()] {
            assert_eq!(
                numsub(&probe, &ch).await,
                1,
                "{ch}: one SUBSCRIBE for 100 feeds"
            );
        }
    }
    // a frame reaches the hub once, not once per feed
    let other = other_process(&j.prefix, j.t, Env::Test).await;
    publish_op(&other, "CS-1", 1).await;
    let f = tokio::time::timeout(WAIT, rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.event, "bus");
    assert!(
        tokio::time::timeout(Duration::from_millis(300), rx.recv())
            .await
            .is_err(),
        "the frame was republished more than once"
    );
    // the last feed to close unsubscribes
    let console = Streams::scoped(&j.prefix, &Scope::new(j.t, "j1", Env::Test).unwrap()).console();
    let half = subs.split_off(50);
    drop(half);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        numsub(&probe, &console).await,
        1,
        "50 feeds still hold the channel"
    );
    drop(subs);
    let deadline = Instant::now() + WAIT;
    while numsub(&probe, &console).await != 0 {
        assert!(
            Instant::now() < deadline,
            "no UNSUBSCRIBE after the last feed closed"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // a later feed re-subscribes on the same connection
    let _again = bridge.acquire(j.t, j.job, "j1").await.unwrap();
    assert_eq!(numsub(&probe, &console).await, 1);
    assert_eq!(bridge.connections_opened(), 1);
}

#[tokio::test]
async fn lagging_client_closed_others_continue() {
    let hub = LocalHub::default();
    let (t, j) = (Uuid::new_v4(), Uuid::new_v4());
    let mut slow = Box::pin(sse_hub::frames(
        hub.subscribe(),
        Upstream::Local,
        t,
        j,
        None,
        Vec::new(),
    ));
    let mut fast = Box::pin(sse_hub::frames(
        hub.subscribe(),
        Upstream::Local,
        t,
        j,
        None,
        Vec::new(),
    ));
    let mut n = 0u64;
    // 1800 frames: the fast client reads every 600, the slow one never reads (buffer 1024)
    for _ in 0..3 {
        for _ in 0..600 {
            hub.publish(message(t, j, "c1", n));
            n += 1;
        }
        for _ in 0..600 {
            fast.next().await.expect("the fast client keeps up");
        }
    }
    let slow_next = tokio::time::timeout(WAIT, slow.next()).await.unwrap();
    assert!(
        slow_next.is_none(),
        "the lagging client's stream is closed, not skipped forward: {slow_next:?}"
    );
    hub.publish(message(t, j, "c1", n));
    let f = tokio::time::timeout(WAIT, fast.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.data["n"], json!(n), "the other client continues");
}

#[tokio::test]
async fn conversation_stream_never_yields_another_conversation() {
    let j = setup("conv-filter").await;
    let bridge = j.e.state.sse.clone().expect("REDIS_URL set: the bridge");
    let sub = bridge.acquire(j.t, j.job, "j1").await.unwrap();
    let mut conv_a = Box::pin(sse_hub::frames(
        j.e.state.events.subscribe(),
        Upstream::Bridged(sub),
        j.t,
        j.job,
        Some("conv-a".into()),
        Vec::new(),
    ));
    // the job feed sees everything: proves the other frames were bridged (their absence below is not vacuous)
    let mut job_feed = Box::pin(sse_hub::frames(
        j.e.state.events.subscribe(),
        Upstream::Local,
        j.t,
        j.job,
        None,
        Vec::new(),
    ));
    let other = other_process(&j.prefix, j.t, Env::Test).await;
    sse_hub::publish_chat(&other, "conv-b", "message", &json!({"text": "for b"}))
        .await
        .unwrap();
    publish_op(&other, "CS-9", 1).await;
    sse_hub::publish_chat(&other, "conv-a", "message", &json!({"text": "for a"}))
        .await
        .unwrap();
    sse_hub::publish_chat(&other, "conv-b", "csat", &json!({}))
        .await
        .unwrap();
    let mut seen = Vec::new();
    for _ in 0..4 {
        let f = tokio::time::timeout(WAIT, job_feed.next())
            .await
            .unwrap()
            .unwrap();
        seen.push((f.event, f.conversation_id));
    }
    assert_eq!(
        seen,
        vec![
            ("message", Some("conv-b".to_string())),
            ("bus", None),
            ("message", Some("conv-a".to_string())),
            ("csat", Some("conv-b".to_string())),
        ]
    );
    let f = tokio::time::timeout(WAIT, conv_a.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (f.event, f.conversation_id.as_deref()),
        ("message", Some("conv-a"))
    );
    assert_eq!(f.data["text"], json!("for a"));
    assert!(
        tokio::time::timeout(Duration::from_millis(500), conv_a.next())
            .await
            .is_err(),
        "another conversation's frame reached conv-a's stream"
    );
}

#[tokio::test]
async fn json_backlog_after_after_ms_oldest_first() {
    let j = setup("backlog").await;
    let prod = other_process(&j.prefix, j.t, Env::Prod).await;
    let test = other_process(&j.prefix, j.t, Env::Test).await;
    // published out of time order, across both envs
    let e3 = publish_op(&prod, "CS-3", 3000).await;
    publish_op(&prod, "CS-1", 1000).await;
    publish_op(&test, "CS-2", 2000).await;
    runner_acks(&prod, &e3, "CS-3").await;
    let (s, b) = common::send(
        &j.e.app,
        common::get(
            "/api/jobs/j1/events?after_ms=1500",
            Some(&common::person("u-ev", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let items = b["items"].as_array().unwrap();
    let at: Vec<u64> = items
        .iter()
        .map(|i| i["envelope"]["published_at_ms"].as_u64().unwrap())
        .collect();
    assert_eq!(at, vec![2000, 3000], "after after_ms, oldest first: {b}");
    assert_eq!(items[0]["envelope"]["env"], json!("test"));
    assert_eq!(items[0]["acked"], json!(false));
    assert_eq!(
        items[1]["acked"],
        json!(true),
        "the runner's ack is folded in"
    );
    assert_eq!(items[1]["entry_id"], json!(e3));
    assert_eq!(items[1]["stream"], json!("bus:op"), "no scope prefix");
    assert_eq!(items[1]["consumer_group"], json!("runner"));
    assert!(
        items[1]["envelope"]["payload"].get("inputs").is_none(),
        "op payloads are ids and codes only (K50): {b}"
    );
    assert_eq!(b["agents"], json!([]));
    // Accept: application/json without after_ms is the whole backlog
    let mut req = common::get("/api/jobs/j1/events", Some(&common::person("u-ev", &[])));
    req.headers_mut()
        .insert("accept", "application/json".parse().unwrap());
    let (s, b) = common::send(&j.e.app, req).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"].as_array().unwrap().len(), 3);
}

/// The feed writes a comment at once: production's edge holds the response headers until the first body byte, so a
/// feed that stays silent until its first keep-alive looks "connecting…" for 15 s.
#[tokio::test]
async fn stream_opens_with_a_comment_at_once() {
    let j = setup("opens").await;
    let (s, feed) = open(&j.e.app, "/api/jobs/j1/events", "u-ev", None).await;
    assert_eq!(s, StatusCode::OK);
    let first = feed
        .unwrap()
        .first_chunk(Duration::from_secs(2))
        .await
        .expect("a first byte well before the 15 s keep-alive");
    assert!(first.starts_with(": open"), "{first:?}");
}

#[tokio::test]
async fn other_tenant_cannot_open_the_stream() {
    let j = setup("tenant").await;
    let b = Uuid::new_v4();
    common::insert_tenant(&j.e.owner, b, "Other Co").await;
    common::insert_user(&j.e.owner, "u-other", b, "tenant_user").await;
    // B has no `j1`: neither form opens
    let (s, _) = open(&j.e.app, "/api/jobs/j1/events", "u-other", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = common::send(
        &j.e.app,
        common::get(
            "/api/jobs/j1/events?after_ms=0",
            Some(&common::person("u-other", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // B's own `j1` never carries A's frames
    job_in(&j.e, b, "j1").await;
    let (s, a_feed) = open(&j.e.app, "/api/jobs/j1/events", "u-ev", None).await;
    assert_eq!(s, StatusCode::OK);
    let (s, b_feed) = open(&j.e.app, "/api/jobs/j1/events", "u-other", None).await;
    assert_eq!(s, StatusCode::OK);
    let (mut a_feed, mut b_feed) = (a_feed.unwrap(), b_feed.unwrap());
    publish_op(&other_process(&j.prefix, j.t, Env::Test).await, "A-1", 1).await;
    publish_op(&other_process(&j.prefix, b, Env::Test).await, "B-1", 2).await;
    let a = a_feed.next(WAIT).await.expect("A's frame on A's feed");
    assert_eq!(a.data["envelope"]["correlation_id"], json!("A-1"));
    let got = b_feed.next(WAIT).await.expect("B's frame on B's feed");
    assert_eq!(
        got.data["envelope"]["correlation_id"],
        json!("B-1"),
        "A's frame reached B's feed: {got:?}"
    );
    assert_eq!(got.data["envelope"]["tenant_id"], json!(b));
}

#[tokio::test]
async fn frame_published_by_another_process_reaches_sse() {
    let j = setup("other-process").await;
    let (s, feed) = open(&j.e.app, "/api/jobs/j1/events", "u-ev", None).await;
    assert_eq!(s, StatusCode::OK);
    let mut feed = feed.unwrap();
    let other = other_process(&j.prefix, j.t, Env::Test).await;
    let id = publish_op(&other, "CS-7", 7).await;
    let ev = feed.next(WAIT).await.expect("the bus frame");
    assert_eq!(ev.event, "bus");
    assert_eq!(ev.id, Some(format!("test/bus:op/{id}")));
    assert_eq!(ev.data["entry_id"], json!(id));
    assert_eq!(ev.data["stream"], json!("bus:op"));
    assert_eq!(ev.data["consumer_group"], json!("runner"));
    assert_eq!(ev.data["acked"], json!(false));
    assert_eq!(ev.data["envelope"]["correlation_id"], json!("CS-7"));
    assert!(ev.data["envelope"]["payload"].get("inputs").is_none());
    // the recipient's XACK follows as an `ack` frame, without an SSE id
    runner_acks(&other, &id, "CS-7").await;
    let ack = feed.next(WAIT).await.expect("the ack frame");
    assert_eq!(ack.event, "ack");
    assert_eq!(ack.id, None);
    assert_eq!(ack.data["entry_id"], json!(id));
    assert_eq!(ack.data["env"], json!("test"));
    // a conversation frame from another process reaches the job feed too
    sse_hub::publish_chat(&other, "conv-1", "message", &json!({"text": "hi"}))
        .await
        .unwrap();
    let m = feed.next(WAIT).await.expect("the chat frame");
    assert_eq!(
        (m.event.as_str(), &m.data["text"]),
        ("message", &json!("hi"))
    );
}

#[tokio::test]
async fn last_event_id_replays_from_console_recent() {
    let j = setup("replay").await;
    let other = other_process(&j.prefix, j.t, Env::Test).await;
    let e1 = publish_op(&other, "CS-1", 1).await;
    let e2 = publish_op(&other, "CS-2", 2).await;
    let e3 = publish_op(&other, "CS-3", 3).await;
    let last = format!("test/bus:op/{e1}");
    let (s, feed) = open(&j.e.app, "/api/jobs/j1/events", "u-ev", Some(&last)).await;
    assert_eq!(s, StatusCode::OK);
    let mut feed = feed.unwrap();
    let a = feed.next(WAIT).await.expect("replayed e2");
    let b = feed.next(WAIT).await.expect("replayed e3");
    assert_eq!(
        (a.data["entry_id"].clone(), b.data["entry_id"].clone()),
        (json!(e2), json!(e3))
    );
    // live frames follow the replay
    let e4 = publish_op(&other, "CS-4", 4).await;
    let c = feed.next(WAIT).await.expect("live e4");
    assert_eq!(c.data["entry_id"], json!(e4));
}

#[tokio::test]
async fn without_redis_the_local_hub_still_feeds_sse() {
    let e = common::env().await;
    assert!(e.state.redis.is_none() && e.state.sse.is_none());
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, "Local Co").await;
    common::insert_user(&e.owner, "u-local", t, "tenant_user").await;
    let job = job_in(&e, t, "j1").await;
    let (s, feed) = open(&e.app, "/api/jobs/j1/events", "u-local", None).await;
    assert_eq!(s, StatusCode::OK);
    let mut feed = feed.unwrap();
    e.state.events.publish(Frame {
        tenant: t,
        job_id: job,
        conversation_id: Some("c1".into()),
        event: "message",
        data: json!({"text": "local"}),
    });
    let m = feed.next(WAIT).await.expect("the local frame");
    assert_eq!(
        (m.event.as_str(), &m.data["text"]),
        ("message", &json!("local"))
    );
    // the JSON backlog needs the bus
    let (s, b) = common::send(
        &e.app,
        common::get(
            "/api/jobs/j1/events?after_ms=0",
            Some(&common::person("u-local", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["code"], json!("unavailable"));
}

#[tokio::test]
async fn unreachable_redis_fails_feed_opens_fast_and_concurrently_then_reconnects() {
    let addr = r::dead_addr().await;
    let bus = Bus::connect_lazy(&r::url_at(addr).await, &r::prefix("sse-down")).unwrap();
    let hub: std::sync::Arc<dyn EventHub> = std::sync::Arc::new(LocalHub::default());
    let bridge = sse_hub::Bridge::new(&bus, hub.clone());
    let (t, job) = (Uuid::new_v4(), Uuid::new_v4());
    // 20 feeds open at once while Redis is down: none waits on a connect, none queues behind another
    let t0 = Instant::now();
    let opens = futures_util::future::join_all((0..20).map(|_| bridge.acquire(t, job, "j1"))).await;
    let took = t0.elapsed();
    assert!(opens.iter().all(Result::is_err), "Redis is down");
    assert!(
        took < sse_hub::FIRST_LINK_WAIT * 3,
        "20 feed opens took {took:?}: serialised behind the connect"
    );
    assert_eq!(bridge.connections_opened(), 0);
    // such a feed is degraded: the local hub still feeds it
    let mut feed = Box::pin(sse_hub::frames(
        hub.subscribe(),
        Upstream::Degraded,
        t,
        job,
        None,
        Vec::new(),
    ));
    hub.publish(message(t, job, "c1", 7));
    let f = tokio::time::timeout(WAIT, feed.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(f.data["n"], json!(7));
    // Redis comes up: the background task opens the link with no feed asking, and the next feed is bridged
    let proxy = r::proxy_at(addr).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    while bridge.connections_opened() == 0 {
        assert!(Instant::now() < deadline, "the link was never re-opened");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let sub = bridge.acquire(t, job, "j1").await;
    assert!(sub.is_ok(), "bridged once Redis answers");
    assert_eq!(bridge.connections_opened(), 1);
    drop(sub);
    proxy.abort();
}

// ---- F2: conversation SSE ids and Last-Event-ID replay; a silent Redis on the bridge -------------------------------

/// Conversation `c` of job `job` with `n` stored messages (`at_ms` 1..=n, texts `m1`..): their ids, oldest first.
async fn conversation_with(e: &TestEnv, t: Uuid, job: Uuid, c: &str, n: i64) -> Vec<Uuid> {
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel) VALUES ($1, $2, $3, 'test', 'staff')",
    )
    .bind(t)
    .bind(job)
    .bind(c)
    .execute(&e.owner)
    .await
    .unwrap();
    let mut ids = Vec::new();
    for i in 1..=n {
        let id: Uuid = sqlx::query_scalar(
            "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, author, text, at_ms) \
             VALUES ($1, $2, $3, $4, 'x', $5, $6) RETURNING message_id",
        )
        .bind(t)
        .bind(job)
        .bind(c)
        .bind(if i % 2 == 1 { "customer" } else { "agent" })
        .bind(format!("m{i}"))
        .bind(i)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        ids.push(id);
    }
    ids
}

fn live_message(t: Uuid, job: Uuid, c: &str, id: &str, text: &str) -> Frame {
    Frame {
        tenant: t,
        job_id: job,
        conversation_id: Some(c.into()),
        event: "message",
        data: json!({"conversation_id": c, "message_id": id, "role": "agent", "text": text}),
    }
}

#[tokio::test]
async fn conversation_feed_ids_messages_and_replays_after_last_event_id() {
    let j = setup("conv-replay").await;
    let ids = conversation_with(&j.e, j.t, j.job, "conv-r", 4).await;
    // another conversation of the job with later messages: never replayed into conv-r
    conversation_with(&j.e, j.t, j.job, "conv-other", 6).await;
    let path = "/api/jobs/j1/conversations/conv-r/events";
    let last = format!("msg/{}", ids[1]);
    let (s, feed) = open(&j.e.app, path, "u-ev", Some(&last)).await;
    assert_eq!(s, StatusCode::OK);
    let mut feed = feed.unwrap();
    let a = feed.next(WAIT).await.expect("replayed m3");
    let b = feed.next(WAIT).await.expect("replayed m4");
    assert_eq!(
        (a.event.as_str(), &a.data["text"]),
        ("message", &json!("m3"))
    );
    assert_eq!(a.id, Some(format!("msg/{}", ids[2])));
    assert_eq!(a.data["message_id"], json!(ids[2].to_string()));
    assert_eq!(a.data["conversation_id"], json!("conv-r"));
    assert_eq!(a.data["role"], json!("customer"));
    assert_eq!(a.data["at_ms"], json!(3));
    assert_eq!(
        (b.data["text"].clone(), b.id),
        (json!("m4"), Some(format!("msg/{}", ids[3])))
    );
    // live frames follow; a replayed message that also arrives live is sent once
    j.e.state.events.publish(live_message(
        j.t,
        j.job,
        "conv-r",
        &ids[3].to_string(),
        "m4",
    ));
    j.e.state
        .events
        .publish(live_message(j.t, j.job, "conv-r", "live-5", "m5"));
    let c = feed.next(WAIT).await.expect("live m5");
    assert_eq!(
        (c.data["text"].clone(), c.id),
        (json!("m5"), Some("msg/live-5".into()))
    );
    // a frame without a message id carries no SSE id (it never moves the client's Last-Event-ID)
    j.e.state.events.publish(Frame {
        tenant: j.t,
        job_id: j.job,
        conversation_id: Some("conv-r".into()),
        event: "csat",
        data: json!({"case_id": "CS-1"}),
    });
    let d = feed.next(WAIT).await.expect("the csat frame");
    assert_eq!((d.event.as_str(), d.id), ("csat", None));
}

#[tokio::test]
async fn conversation_feed_replays_nothing_for_an_id_it_does_not_hold() {
    let j = setup("conv-replay-unknown").await;
    let ids = conversation_with(&j.e, j.t, j.job, "conv-u", 3).await;
    let other = conversation_with(&j.e, j.t, j.job, "conv-v", 3).await;
    let path = "/api/jobs/j1/conversations/conv-u/events";
    // a message of ANOTHER conversation, an unknown uuid, a bus id, garbage: no replay
    for last in [
        format!("msg/{}", other[0]),
        format!("msg/{}", Uuid::new_v4()),
        "test/bus:op/1-0".to_string(),
        "msg/not-a-uuid".to_string(),
    ] {
        let (s, feed) = open(&j.e.app, path, "u-ev", Some(&last)).await;
        assert_eq!(s, StatusCode::OK, "{last}");
        let mut feed = feed.unwrap();
        j.e.state
            .events
            .publish(live_message(j.t, j.job, "conv-u", "live-9", "live"));
        let first = feed.next(WAIT).await.expect("the live frame");
        assert_eq!(
            first.data["text"],
            json!("live"),
            "{last}: replayed {first:?}"
        );
    }
    // the newest message as Last-Event-ID: nothing to replay either, the live frame is first
    let (_, feed) = open(&j.e.app, path, "u-ev", Some(&format!("msg/{}", ids[2]))).await;
    let mut feed = feed.unwrap();
    j.e.state
        .events
        .publish(live_message(j.t, j.job, "conv-u", "live-10", "after"));
    assert_eq!(feed.next(WAIT).await.unwrap().data["text"], json!("after"));
}

#[tokio::test]
async fn conversation_feed_still_opens_with_a_comment_at_once() {
    let j = setup("conv-opens").await;
    conversation_with(&j.e, j.t, j.job, "conv-o", 1).await;
    let (s, feed) = open(
        &j.e.app,
        "/api/jobs/j1/conversations/conv-o/events",
        "u-ev",
        Some("msg/x"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let first = feed
        .unwrap()
        .first_chunk(Duration::from_secs(2))
        .await
        .expect("a first byte at once");
    assert!(first.starts_with(": open"), "{first:?}");
}

/// Copy `from` to `to`, holding the bytes while `frozen` (the connection stays open, nothing is forwarded).
async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    frozen: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        while frozen.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

/// A TCP proxy to the shared Redis that can go silent: while `frozen`, bytes are held (the connections stay open),
/// so Redis "stops answering" mid-link.
async fn freezable_proxy() -> (
    std::net::SocketAddr,
    std::sync::Arc<std::sync::atomic::AtomicBool>,
    tokio::task::JoinHandle<()>,
) {
    let url = r::url().await;
    let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
    let upstream = rest.split('/').next().unwrap_or(rest).to_string();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let frozen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let f = frozen.clone();
    let task = tokio::spawn(async move {
        while let Ok((inbound, _)) = listener.accept().await {
            let Ok(out) = tokio::net::TcpStream::connect(&upstream).await else {
                continue;
            };
            let (ir, iw) = inbound.into_split();
            let (or, ow) = out.into_split();
            tokio::spawn(pump(ir, ow, f.clone()));
            tokio::spawn(pump(or, iw, f.clone()));
        }
    });
    (addr, frozen, task)
}

/// The audit card (one lock across SUBSCRIBE): Redis stops answering on an open link. 20 feeds of different jobs
/// open at once; before F2 each waited behind the lock for the previous one's SUBSCRIBE timeout (20 x 3 s). Now the
/// first failed SUBSCRIBE drops the link, the others give up on the lock within the budget, every bridged stream
/// closes, and the link re-opens once Redis answers again.
#[tokio::test]
async fn a_silent_redis_bounds_feed_opens_and_drops_the_link() {
    let (addr, frozen, proxy) = freezable_proxy().await;
    let bus = Bus::connect_lazy(&r::url_at(addr).await, &r::prefix("sse-silent")).unwrap();
    let hub: std::sync::Arc<dyn EventHub> = std::sync::Arc::new(LocalHub::default());
    let bridge = sse_hub::Bridge::new(&bus, hub.clone());
    let t = Uuid::new_v4();
    let job0 = Uuid::new_v4();
    let deadline = Instant::now() + Duration::from_secs(20);
    let first = loop {
        match bridge.acquire(t, job0, "j0").await {
            Ok(s) => break s,
            Err(_) => {
                assert!(
                    Instant::now() < deadline,
                    "never bridged while Redis answered"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    };
    assert_eq!(bridge.connections_opened(), 1);
    let mut bridged = Box::pin(sse_hub::frames(
        hub.subscribe(),
        Upstream::Bridged(first),
        t,
        job0,
        None,
        Vec::new(),
    ));
    frozen.store(true, std::sync::atomic::Ordering::SeqCst);
    let keys: Vec<String> = (1..=20).map(|i| format!("j{i}")).collect();
    let t0 = Instant::now();
    let opens =
        futures_util::future::join_all(keys.iter().map(|k| bridge.acquire(t, Uuid::new_v4(), k)))
            .await;
    let took = t0.elapsed();
    assert!(
        opens.iter().all(Result::is_err),
        "Redis is silent: no SUBSCRIBE is answered"
    );
    assert!(
        took < sse_hub::ACQUIRE_BUDGET * 2,
        "20 feed opens took {took:?}: each queued behind a SUBSCRIBE timeout"
    );
    // the link was dropped: the stream bridged before it closes (the client reconnects with Last-Event-ID)
    let end = tokio::time::timeout(WAIT, bridged.next()).await;
    assert!(
        matches!(end, Ok(None)),
        "the bridged stream stayed open: {end:?}"
    );
    // Redis answers again: the link re-opens (not before the 5 s back-off) and feeds are bridged
    frozen.store(false, std::sync::atomic::Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if bridge.acquire(t, Uuid::new_v4(), "j21").await.is_ok() {
            break;
        }
        assert!(Instant::now() < deadline, "the link never re-opened");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert_eq!(bridge.connections_opened(), 2, "one re-open after the drop");
    proxy.abort();
}

/// The audit card (publish_chat had no command timeout; staff and inbound responses await it): Redis stops answering
/// on an open connection whose own response timeout is far longer than [`sse_hub::CMD_TIMEOUT`]. The chat PUBLISH
/// gives up within the command timeout, and `publish` delivers the frame to the local hub instead.
#[tokio::test]
async fn a_silent_redis_bounds_chat_publish_and_falls_back_to_the_local_hub() {
    let (addr, frozen, proxy) = freezable_proxy().await;
    let bus = Bus::connect_with_response_timeout(
        &r::url_at(addr).await,
        &r::prefix("sse-publish"),
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    let mut e = common::env().await;
    e.state.redis = Some(bus.clone());
    let (t, job) = (Uuid::new_v4(), Uuid::new_v4());
    let scope = Scope::new(t, "j1", Env::Test).unwrap();
    // Redis answers: the frame goes through the bus, not the local hub
    sse_hub::publish_chat(&bus.scoped(&scope), "c1", "message", &json!({"n": 1}))
        .await
        .unwrap();
    frozen.store(true, std::sync::atomic::Ordering::SeqCst);
    let t0 = Instant::now();
    let r = sse_hub::publish_chat(&bus.scoped(&scope), "c1", "message", &json!({"n": 2})).await;
    let took = t0.elapsed();
    assert!(r.is_err(), "a silent Redis answered the PUBLISH");
    assert!(
        took < sse_hub::CMD_TIMEOUT + Duration::from_secs(2),
        "the PUBLISH waited {took:?}"
    );
    let mut local = e.state.events.subscribe();
    let t0 = Instant::now();
    sse_hub::publish(&e.state, &scope, job, "c1", "message", json!({"n": 3})).await;
    assert!(
        t0.elapsed() < sse_hub::CMD_TIMEOUT + Duration::from_secs(2),
        "publish stalled on a silent Redis"
    );
    let f = tokio::time::timeout(WAIT, local.recv())
        .await
        .expect("the local hub got the frame")
        .unwrap();
    assert_eq!(
        (f.tenant, f.job_id, f.conversation_id.as_deref(), f.event),
        (t, job, Some("c1"), "message")
    );
    assert_eq!(f.data["n"], json!(3));
    frozen.store(false, std::sync::atomic::Ordering::SeqCst);
    proxy.abort();
}
