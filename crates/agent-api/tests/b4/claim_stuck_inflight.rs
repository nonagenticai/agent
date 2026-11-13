//! B4 audit of agent#58: `claimStuck` must not dead-letter an entry a LIVE consumer is still handling. A handler
//! legitimately runs past 30 s (LLM and tool calls) while its consumer reads nothing, so the entry is idle > 30 s;
//! the owner's heartbeat (`hb:{group}:{consumer}`) tells it apart from an entry whose consumer died.
use std::time::Duration;

use agent_api::bus_admin;
use agent_api::runtime::bus::{Envelope, groups};
use agent_api::runtime::heartbeat::{self, Serving};
use serde_json::json;

use crate::redis_env as r;

/// XREADGROUP everything new as `consumer`, never ack, then make each entry idle for 60 s; the ids.
async fn read_idle(bus: &agent_api::runtime::Bus, stream: &str, consumer: &str) -> Vec<String> {
    let _: redis::Value = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg(groups::RUNNER)
        .arg(consumer)
        .arg("STREAMS")
        .arg(stream)
        .arg(">")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let pending: Vec<(String, String, u64, u64)> = redis::cmd("XPENDING")
        .arg(stream)
        .arg(groups::RUNNER)
        .arg("-")
        .arg("+")
        .arg(100)
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let mut ids = Vec::new();
    for (id, owner, _, _) in pending {
        if owner != consumer {
            continue;
        }
        let _: redis::Value = redis::cmd("XCLAIM")
            .arg(stream)
            .arg(groups::RUNNER)
            .arg(consumer)
            .arg(0)
            .arg(&id)
            .arg("IDLE")
            .arg(60_000)
            .arg("JUSTID")
            .query_async(&mut bus.conn())
            .await
            .unwrap();
        ids.push(id);
    }
    ids
}

#[tokio::test]
async fn an_entry_whose_consumer_still_beats_is_not_dead_lettered() {
    let root = r::root("cs-inflight").await;
    let bus = root.scoped(&r::scope_a());
    let stream = bus.streams.op();
    let env = |cid: &str| {
        Envelope::new(
            "op.call",
            groups::ORCHESTRATOR,
            cid,
            json!({"call_id": cid, "operation": "order.read", "inputs": {}}),
        )
        .unwrap()
    };
    bus.ensure_group(&stream, groups::RUNNER).await.unwrap();

    // runner-busy is in a long handler: entry idle 60 s, process alive. runner-dead died with its entry.
    bus.publish(&stream, &env("CS-BUSY")).await.unwrap();
    let busy = read_idle(&bus, &stream, "runner-busy").await;
    bus.publish(&stream, &env("CS-DEAD")).await.unwrap();
    let dead = read_idle(&bus, &stream, "runner-dead").await;
    assert_eq!((busy.len(), dead.len()), (1, 1));
    heartbeat::beat(
        &root,
        groups::RUNNER,
        "runner-busy",
        agent_api::runtime::bus::now_ms(),
        &Serving::default(),
        Duration::from_secs(60),
    )
    .await
    .unwrap();

    let n = bus_admin::claim(&bus, "bus:op", groups::RUNNER)
        .await
        .unwrap();
    assert_eq!(n, Some(1), "only the dead consumer's entry is taken");

    let pending: Vec<(String, String, u64, u64)> = redis::cmd("XPENDING")
        .arg(&stream)
        .arg(groups::RUNNER)
        .arg("-")
        .arg("+")
        .arg(100)
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(pending.len(), 1, "{pending:?}");
    assert_eq!(pending[0].0, busy[0]);
    assert_eq!(pending[0].1, "runner-busy", "still with its live owner");
    let dead_letters = r::entries(&bus, &bus.streams.dead()).await;
    assert_eq!(dead_letters.len(), 1, "{dead_letters:?}");
    assert_eq!(dead_letters[0].correlation_id, "CS-DEAD");
}
