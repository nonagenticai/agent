//! B4 audit of agent#43: `claimStuck` moves a group's stuck entries (idle ≥ 30 s, so no live consumer reclaimed
//! them) to `bus:dead` with reason `stuck`; nothing is ever left pending on `agent-api-admin`, and a replay puts the
//! entry back where a live consumer processes it. A stuck entry stamped for another scope is dead-lettered as
//! `scope_mismatch` (never replayable into this scope). Also: `agentFleet` and `caseload.fleet` are one computation.
use std::time::Duration;

use agent_api::bus_admin::CLAIM_CONSUMER;
use agent_api::runtime::bus::{ConsumeOptions, Envelope, groups};
use agent_api::runtime::{Bus, Env, Scope};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{self, TestEnv};
use crate::redis_env as r;

struct Job {
    e: TestEnv,
    t: Uuid,
    root: Bus,
}

async fn setup(test: &str) -> Job {
    let vars = r::api_vars(test).await;
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, "Claim Stuck Co").await;
    common::insert_user(&e.owner, "u-obs", t, "tenant_user").await;
    common::insert_user(&e.owner, "u-op", t, "operator").await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'j1', 'J', '[]', 'test')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    let root = e.state.redis.clone().expect("REDIS_URL is set");
    Job { e, t, root }
}

impl Job {
    fn bus(&self, env: Env) -> Bus {
        self.root.scoped(&Scope::new(self.t, "j1", env).unwrap())
    }

    async fn get(&self, sub: &str, path: &str) -> (StatusCode, Value) {
        let token = common::person(sub, &[]);
        common::send(
            &self.e.app,
            common::get(&format!("/api/jobs/j1{path}"), Some(&token)),
        )
        .await
    }

    async fn post(&self, sub: &str, path: &str) -> (StatusCode, Value) {
        let token = common::person(sub, &[]);
        common::send(
            &self.e.app,
            common::json(
                "POST",
                &format!("/api/jobs/j1{path}"),
                Some(&token),
                json!({}),
            ),
        )
        .await
    }
}

fn op_call(cid: &str) -> Envelope {
    Envelope::new(
        "op.call",
        groups::ORCHESTRATOR,
        cid,
        json!({"call_id": cid, "operation": "order.read", "inputs": {"email": "femke@example.com"}}),
    )
    .unwrap()
}

/// XADD a raw envelope (no re-stamping); its entry id.
async fn xadd_raw(bus: &Bus, stream: &str, env: &Envelope) -> String {
    redis::cmd("XADD")
        .arg(stream)
        .arg("*")
        .arg("envelope")
        .arg(serde_json::to_string(env).unwrap())
        .query_async(&mut bus.conn())
        .await
        .unwrap()
}

/// Deliver every new entry of `stream` to `consumer` in `group`, never ack, and make them idle for 60 s (a
/// consumer that read them and died a minute ago).
async fn stuck_on(bus: &Bus, stream: &str, group: &str, consumer: &str) -> Vec<String> {
    bus.ensure_group(stream, group).await.unwrap();
    let _: redis::Value = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg(group)
        .arg(consumer)
        .arg("COUNT")
        .arg(100)
        .arg("STREAMS")
        .arg(stream)
        .arg(">")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let ids: Vec<String> = pending(bus, stream, group)
        .await
        .into_iter()
        .map(|p| p.0)
        .collect();
    for id in &ids {
        let _: redis::Value = redis::cmd("XCLAIM")
            .arg(stream)
            .arg(group)
            .arg(consumer)
            .arg(0)
            .arg(id)
            .arg("IDLE")
            .arg(60_000)
            .arg("JUSTID")
            .query_async(&mut bus.conn())
            .await
            .unwrap();
    }
    ids
}

/// The group's pending entries: (id, consumer, idle ms, deliveries).
async fn pending(bus: &Bus, stream: &str, group: &str) -> Vec<(String, String, u64, u64)> {
    redis::cmd("XPENDING")
        .arg(stream)
        .arg(group)
        .arg("-")
        .arg("+")
        .arg(1000)
        .query_async(&mut bus.conn())
        .await
        .unwrap()
}

#[tokio::test]
async fn stuck_entries_become_dead_letters_never_parked_on_the_admin_consumer() {
    let j = setup("cs-stuck").await;
    let bus = j.bus(Env::Prod);
    let stream = bus.streams.op();
    bus.publish(&stream, &op_call("CS-1")).await.unwrap();
    let mut foreign = op_call("CS-99");
    foreign.tenant_id = Some(Uuid::new_v4());
    foreign.job = Some("j1".into());
    foreign.env = Some(Env::Prod);
    xadd_raw(&bus, &stream, &foreign).await;
    let ids = stuck_on(&bus, &stream, groups::RUNNER, "runner-dead").await;
    assert_eq!(ids.len(), 2);

    let (s, b) = j
        .post("u-op", "/streams/bus%3Aop/groups/runner/claim")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"claimed": 2}), "the response shape is unchanged");

    // nothing pending in the group: not on the dead consumer, and never parked on agent-api-admin
    let p = pending(&bus, &stream, groups::RUNNER).await;
    assert!(p.is_empty(), "{p:?}");
    assert!(!p.iter().any(|e| e.1 == CLAIM_CONSUMER));

    // both on bus:dead: the own-scope one `stuck` (replayable), the foreign one `scope_mismatch`
    let (s, list) = j.get("u-obs", "/dead-letters").await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{list}");
    let by_origin = |id: &str| {
        items
            .iter()
            .find(|i| i["dead"]["entry_id"] == id)
            .unwrap_or_else(|| panic!("no dead letter for {id}: {list}"))
            .clone()
    };
    let own = by_origin(&ids[0]);
    assert_eq!(own["dead"]["reason_code"], "stuck", "{own}");
    assert_eq!(own["dead"]["stream"], "bus:op", "{own}");
    assert_eq!(own["env"], "prod");
    let theirs = by_origin(&ids[1]);
    assert_eq!(theirs["dead"]["reason_code"], "scope_mismatch", "{theirs}");
    let (s, b) = j
        .post(
            "u-op",
            &format!(
                "/dead-letters/{}/replay",
                theirs["entry_id"].as_str().unwrap()
            ),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");

    // a repeat claim finds nothing and moves nothing
    let (_, b) = j
        .post("u-op", "/streams/bus%3Aop/groups/runner/claim")
        .await;
    assert_eq!(b, json!({"claimed": 0}));

    // replayed, a live runner processes it and the group ends empty
    let (s, b) = j
        .post(
            "u-op",
            &format!("/dead-letters/{}/replay", own["entry_id"].as_str().unwrap()),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (b2, st2) = (bus.clone(), stream.clone());
    let task = tokio::spawn(async move {
        b2.consume(
            &st2,
            groups::RUNNER,
            "runner-live",
            ConsumeOptions {
                batch: 16,
                block: Duration::from_millis(100),
                reclaim_idle: Duration::from_secs(30),
                max_deliveries: 5,
            },
            move |env| {
                let tx = tx.clone();
                async move {
                    let _ = tx.send(env.correlation_id);
                    Ok::<(), anyhow::Error>(())
                }
            },
        )
        .await
    });
    let cid = tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("the replayed entry is processed")
        .unwrap();
    task.abort();
    assert_eq!(cid, "CS-1");
    assert!(pending(&bus, &stream, groups::RUNNER).await.is_empty());

    let audits: Vec<Value> = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'stream.claim_stuck' ORDER BY id",
    )
    .bind(j.t)
    .fetch_all(&j.e.owner)
    .await
    .unwrap();
    assert_eq!(audits.len(), 2);
    assert_eq!(audits[0]["claimed"], 2);
}

#[tokio::test]
async fn an_entry_not_idle_30s_stays_with_its_consumer() {
    let j = setup("cs-fresh").await;
    let bus = j.bus(Env::Prod);
    let stream = bus.streams.op();
    bus.publish(&stream, &op_call("CS-2")).await.unwrap();
    bus.ensure_group(&stream, groups::RUNNER).await.unwrap();
    let _: redis::Value = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg(groups::RUNNER)
        .arg("runner-busy")
        .arg("COUNT")
        .arg(1)
        .arg("STREAMS")
        .arg(&stream)
        .arg(">")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let (s, b) = j
        .post("u-op", "/streams/bus%3Aop/groups/runner/claim")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b, json!({"claimed": 0}));
    let p = pending(&bus, &stream, groups::RUNNER).await;
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].1, "runner-busy");
    assert_eq!(r::entries(&bus, &bus.streams.dead()).await.len(), 0);
}

#[tokio::test]
async fn agent_fleet_and_caseload_fleet_are_the_same_computation() {
    let j = setup("cs-fleet").await;
    let (s, a) = j.get("u-obs", "/agents/fleet").await;
    assert_eq!(s, StatusCode::OK, "{a}");
    let (s, l) = j.get("u-obs", "/caseload").await;
    assert_eq!(s, StatusCode::OK, "{l}");
    let mut c = l["fleet"].clone();
    let mut a = a;
    assert!(a["as_of"].is_string() && c["as_of"].is_string());
    a["as_of"] = Value::Null;
    c["as_of"] = Value::Null;
    assert_eq!(a, c, "agentFleet and caseload.fleet differ");
    assert_eq!(a["follow_s"], 35);
}
