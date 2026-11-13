//! B4 T1 — the bus (crate::runtime::bus), ported from care-bus-agents@96c608c `crates/carebus/tests/ack.rs`: the
//! recipient's XACK is mirrored live and recomputable, the scope (with its env) is stamped on every publish and a
//! mismatched entry is dead-lettered, a root bus refuses every stream operation, and the console sees `op.*`
//! payloads as ids and codes only (K50).
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering::SeqCst};
use std::time::Duration;

use agent_api::runtime::bus::{ConsumeOptions, Envelope, groups};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::redis_env as r;

/// Collect the console frames of `bus` for `window`, split into (publishes, acks).
async fn frames_for(
    frames: &mut redis::aio::PubSubStream,
    window: Duration,
) -> (Vec<Value>, Vec<Value>) {
    let mut publishes = Vec::new();
    let mut acks = Vec::new();
    let deadline = tokio::time::Instant::now() + window;
    while tokio::time::Instant::now() < deadline {
        let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(200), frames.next()).await
        else {
            continue;
        };
        let v: Value = serde_json::from_str(&m.get_payload::<String>().unwrap()).unwrap();
        if v["kind"] == "ack" {
            acks.push(v);
        } else {
            publishes.push(v);
        }
    }
    (publishes, acks)
}

#[tokio::test]
async fn the_recipients_ack_is_mirrored_and_recomputable_and_a_failed_handler_stays_pending() {
    let bus = r::bus("ack").await;
    let stream = bus.streams.customer("faq");
    let mut pubsub = bus.client().get_async_pubsub().await.unwrap();
    pubsub.subscribe(bus.streams.console()).await.unwrap();
    let mut frames = pubsub.into_on_message();

    let ok = Envelope::new(
        "faq.request.qualified",
        "interaction",
        "CASE-A",
        json!({"fail": false}),
    )
    .unwrap();
    let ok_id = bus.publish(&stream, &ok).await.unwrap();
    let bad = Envelope::new(
        "faq.request.qualified",
        "interaction",
        "CASE-B",
        json!({"fail": true}),
    )
    .unwrap();
    let bad_id = bus.publish(&stream, &bad).await.unwrap();
    // nobody has read the stream yet
    assert!(
        !bus.is_acked(&stream, groups::ORCHESTRATOR, &ok_id)
            .await
            .unwrap()
    );
    assert!(
        !bus.is_acked(&bus.streams.customer("nope"), groups::ORCHESTRATOR, &ok_id)
            .await
            .unwrap()
    );

    // The orchestrator (recipient of request.qualified) fails CASE-B; the interaction group reads and acks
    // everything as well.
    let handler = |env: Envelope| async move {
        if env.payload["fail"] == true {
            anyhow::bail!("boom")
        }
        Ok(())
    };
    let opts = ConsumeOptions {
        reclaim_idle: Duration::from_secs(60), // no redelivery inside this test
        ..r::fast()
    };
    let (b1, b2, s1, s2, o1, o2) = (
        bus.clone(),
        bus.clone(),
        stream.clone(),
        stream.clone(),
        opts.clone(),
        opts,
    );
    let t1 = tokio::spawn(async move {
        b1.consume(&s1, groups::ORCHESTRATOR, "c1", o1, handler)
            .await
    });
    let t2 = tokio::spawn(async move {
        b2.consume(&s2, groups::INTERACTION, "c2", o2, |_| async { Ok(()) })
            .await
    });
    let (publishes, acks) = frames_for(&mut frames, Duration::from_secs(3)).await;
    t1.abort();
    t2.abort();

    // the publish mirror names the recipient group and starts unacked
    assert_eq!(publishes.len(), 2, "{publishes:?}");
    for p in &publishes {
        assert_eq!(p["consumer_group"], "orchestrator", "{p}");
        assert_eq!(p["acked"], false, "{p}");
    }
    // exactly one ack frame: the orchestrator's for CASE-A. The interaction group's acks are not the recipient's,
    // and CASE-B's handler failed, so it was never acked.
    assert_eq!(acks.len(), 1, "{acks:?}");
    assert_eq!(acks[0]["entry_id"], ok_id.as_str());
    assert_eq!(acks[0]["consumer_group"], "orchestrator");
    assert_eq!(acks[0]["correlation_id"], "CASE-A");
    assert_eq!(acks[0]["stream"], stream.as_str());

    // the rebuild path recomputes the same answer from Redis
    assert!(
        bus.is_acked(&stream, groups::ORCHESTRATOR, &ok_id)
            .await
            .unwrap()
    );
    assert!(
        !bus.is_acked(&stream, groups::ORCHESTRATOR, &bad_id)
            .await
            .unwrap(),
        "delivered but in the PEL is pending"
    );
    assert!(
        bus.is_acked(&stream, groups::INTERACTION, &bad_id)
            .await
            .unwrap()
    );
    assert!(
        !bus.is_acked(&stream, "no-such-group", &ok_id)
            .await
            .unwrap()
    );

    // the JSON backlog keeps every console frame, newest first
    let recent: Vec<String> = redis::cmd("LRANGE")
        .arg(bus.streams.console_recent())
        .arg(0)
        .arg(-1)
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(recent.len(), 3, "{recent:?}");
    let newest: Value = serde_json::from_str(&recent[0]).unwrap();
    assert_eq!(newest["kind"], "ack");
    let ttl: i64 = redis::cmd("TTL")
        .arg(bus.streams.console_recent())
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert!(ttl > 0 && ttl <= 86_400, "{ttl}");
}

#[tokio::test]
async fn a_dead_letter_frame_has_no_consumer_group() {
    let bus = r::bus("ack-dead").await;
    let mut pubsub = bus.client().get_async_pubsub().await.unwrap();
    pubsub.subscribe(bus.streams.console()).await.unwrap();
    let mut frames = pubsub.into_on_message();
    let env = Envelope::new("dead.letter", "x", "CASE-D", json!({})).unwrap();
    bus.publish(&bus.streams.dead(), &env).await.unwrap();
    let m = tokio::time::timeout(Duration::from_secs(2), frames.next())
        .await
        .unwrap()
        .unwrap();
    let v: Value = serde_json::from_str(&m.get_payload::<String>().unwrap()).unwrap();
    assert_eq!(v["consumer_group"], Value::Null);
    assert_eq!(v["acked"], Value::Null);
}

#[tokio::test]
async fn scoped_keys_are_prefixed() {
    let root = r::root("scopedkeys").await;
    let s = r::scope_a();
    let p = root.root_prefix().to_string();
    assert_eq!(
        root.scoped(&s).streams.customer("faq"),
        format!("{p}s:{}:{}:bus:customer:faq", s.tenant, s.job)
    );
    assert_eq!(
        root.scoped(&s).streams.key("case:C1"),
        format!("{p}s:{}:{}:case:C1", s.tenant, s.job)
    );
    assert_eq!(
        root.scoped(&r::scope_a_test()).streams.op(),
        format!("{p}s:{}:{}:test:bus:op", s.tenant, s.job)
    );
    assert_eq!(root.scoped(&s).scope(), Some(&s));
    // the registry key stays at the environment root, also from a scoped bus
    assert_eq!(root.scoped(&s).root_key("scopes"), format!("{p}scopes"));
    assert_eq!(root.root_key("scopes"), format!("{p}scopes"));
    assert_eq!(root.scoped(&s).root().scope(), None);
}

#[tokio::test]
async fn unscoped_bus_refuses_publish() {
    let root = r::root("unscoped").await;
    let e = Envelope::new("x.request.qualified", "t", "c", json!({})).unwrap();
    let err = root.publish("bus:customer:faq", &e).await.unwrap_err();
    assert!(err.to_string().contains("not scoped"), "{err:#}");
    let err = root
        .ensure_group("bus:customer:faq", "g")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not scoped"), "{err:#}");
    let err = root
        .consume("bus:customer:faq", "g", "c1", r::fast(), |_| async {
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not scoped"), "{err:#}");
    let err = root
        .consume_all(vec![], "g", "c1", r::fast(), |_| async { Ok(()) })
        .await
        .unwrap_err();
    assert!(err.to_string().contains("not scoped"), "{err:#}");
}

#[tokio::test]
async fn publish_stamps_scope_and_env() {
    let root = r::root("stamp").await;
    for s in [r::scope_a(), r::scope_a_test()] {
        let bus = root.scoped(&s);
        let e = Envelope::new("x.request.qualified", "t", "CASE-S", json!({})).unwrap();
        assert_eq!(e.tenant_id, None);
        bus.publish(&bus.streams.customer("faq"), &e).await.unwrap();
        let got = r::entries(&bus, &bus.streams.customer("faq")).await;
        assert_eq!(got.len(), 1, "{s}");
        assert_eq!(got[0].tenant_id, Some(s.tenant));
        assert_eq!(got[0].job.as_deref(), Some(s.job.as_str()));
        assert_eq!(got[0].env, Some(s.env));
    }
}

/// XADD `env` raw (no stamping) and run one orchestrator consumer until one dead letter appears; returns the dead
/// letters and how often the handler ran.
async fn consume_foreign(bus: &agent_api::runtime::Bus, env: &Envelope) -> (Vec<Envelope>, u32) {
    let stream = bus.streams.customer("faq");
    let _: String = redis::cmd("XADD")
        .arg(&stream)
        .arg("*")
        .arg("envelope")
        .arg(serde_json::to_string(env).unwrap())
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let calls = Arc::new(AtomicU32::new(0));
    let (b, st, c) = (bus.clone(), stream.clone(), calls.clone());
    let task = tokio::spawn(async move {
        b.consume(&st, groups::ORCHESTRATOR, "c1", r::fast(), move |_env| {
            let c = c.clone();
            async move {
                c.fetch_add(1, SeqCst);
                Ok(())
            }
        })
        .await
    });
    let dead = r::wait_entries(bus, &bus.streams.dead(), 1, |_| true).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    task.abort();
    // the entry is acked, not left pending for redelivery
    let pending: redis::streams::StreamPendingReply = redis::cmd("XPENDING")
        .arg(&stream)
        .arg(groups::ORCHESTRATOR)
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(pending.count(), 0);
    (dead, calls.load(SeqCst))
}

#[tokio::test]
async fn scope_mismatch_dead_letters() {
    let root = r::root("mismatch").await;
    // another tenant's envelope on scope a's stream
    let b = r::scope_b();
    let mut foreign = Envelope::new("faq.request.qualified", "t", "CASE-X", json!({})).unwrap();
    foreign.tenant_id = Some(b.tenant);
    foreign.job = Some(b.job.clone());
    foreign.env = Some(b.env);
    let (dead, calls) = consume_foreign(&root.scoped(&r::scope_a()), &foreign).await;
    assert_eq!(calls, 0);
    assert_eq!(dead.len(), 1);
    let reason = dead[0].payload["reason"].as_str().unwrap();
    assert!(reason.contains("scope_mismatch"), "{reason}");
    assert_eq!(
        dead[0].payload["original"]["tenant_id"],
        b.tenant.to_string()
    );

    // the same tenant and job but the prod env, on the test scope's stream: a test-env consumer never acts on it
    let a = r::scope_a();
    let mut prod = Envelope::new("faq.request.qualified", "t", "CASE-P", json!({})).unwrap();
    prod.tenant_id = Some(a.tenant);
    prod.job = Some(a.job.clone());
    prod.env = Some(a.env);
    let (dead, calls) = consume_foreign(&root.scoped(&r::scope_a_test()), &prod).await;
    assert_eq!(calls, 0);
    let reason = dead[0].payload["reason"].as_str().unwrap();
    assert!(reason.contains("scope_mismatch"), "{reason}");
    assert!(reason.ends_with("/test"), "{reason}");

    // no env at all (a pre-env writer) is a mismatch too (a fresh root: scope a's dead stream already has one)
    let mut bare = Envelope::new("faq.request.qualified", "t", "CASE-N", json!({})).unwrap();
    bare.tenant_id = Some(a.tenant);
    bare.job = Some(a.job.clone());
    let fresh = r::root("mismatch-bare").await;
    let (dead, calls) = consume_foreign(&fresh.scoped(&a), &bare).await;
    assert_eq!(calls, 0);
    assert!(
        dead[0].payload["reason"]
            .as_str()
            .unwrap()
            .contains("stamped no scope")
    );
}

#[tokio::test]
async fn a_handler_that_keeps_failing_is_dead_lettered_after_max_deliveries() {
    let bus = r::bus("giveup").await;
    let stream = bus.streams.op();
    let call = Envelope::new(
        "op.call",
        groups::ORCHESTRATOR,
        "CASE-F",
        json!({"call_id": "c9"}),
    )
    .unwrap();
    bus.publish(&stream, &call).await.unwrap();
    let calls = Arc::new(AtomicU32::new(0));
    let (b, st, c) = (bus.clone(), stream.clone(), calls.clone());
    let task = tokio::spawn(async move {
        b.consume(&st, groups::RUNNER, "c1", r::fast(), move |_env| {
            let c = c.clone();
            async move {
                c.fetch_add(1, SeqCst);
                Err::<(), anyhow::Error>(anyhow::anyhow!("always"))
            }
        })
        .await
    });
    let dead = r::wait_entries(&bus, &bus.streams.dead(), 1, |_| true).await;
    task.abort();
    // max_deliveries = 3: three runs, the fourth delivery gives up
    assert_eq!(calls.load(SeqCst), 3);
    let reason = dead[0].payload["reason"].as_str().unwrap();
    assert!(reason.contains("gave up after 3 deliveries"), "{reason}");
    assert_eq!(dead[0].correlation_id, "CASE-F");
}

#[tokio::test]
async fn the_console_sees_op_payloads_as_ids_and_codes_only() {
    let bus = r::bus("k50").await;
    let mut pubsub = bus.client().get_async_pubsub().await.unwrap();
    pubsub.subscribe(bus.streams.console()).await.unwrap();
    let mut frames = pubsub.into_on_message();
    let secret = "customer says: my card is 4111";
    let call = Envelope::new(
        "returns.op.call",
        groups::ORCHESTRATOR,
        "CASE-K",
        json!({"call_id": "c1", "case_id": "CS-7", "operation": "order.read",
               "inputs": {"note": secret}, "text": secret}),
    )
    .unwrap();
    bus.publish(&bus.streams.op(), &call).await.unwrap();
    let m = tokio::time::timeout(Duration::from_secs(2), frames.next())
        .await
        .unwrap()
        .unwrap();
    let text = m.get_payload::<String>().unwrap();
    assert!(!text.contains("4111"), "{text}");
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        v["envelope"]["payload"],
        json!({"call_id": "c1", "case_id": "CS-7", "operation": "order.read"})
    );
    assert_eq!(v["consumer_group"], "runner");
    // the backlog holds the same redacted frame; the stream itself keeps the full payload for the runner
    let recent: Vec<String> = redis::cmd("LRANGE")
        .arg(bus.streams.console_recent())
        .arg(0)
        .arg(-1)
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(recent.len(), 1);
    assert!(!recent[0].contains("4111"));
    let on_stream = r::entries(&bus, &bus.streams.op()).await;
    assert_eq!(on_stream[0].payload["text"], secret);
}
