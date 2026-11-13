//! B4 T14 — bus admin (crate::bus_admin, routes `agents` / `streams` / `bus_flow`): the ported care-bus streams tests
//! (scoped stream entries, dead-letter replay / discard / paging, claim), heartbeats → instances, rates from the
//! `metrics:*` hashes `Bus` writes, the J24 flow per environment, the fleet, and the T19 Redis-sourced gauges.
//! Dead letters are produced the real way (a handler that keeps failing until `Bus::consume` gives up), so the
//! routes are proved against `Bus::dead_letter`, not against a hand-built shape.
use std::time::{Duration, Instant};

use agent_api::runtime::bus::{ConsumeOptions, Envelope, groups};
use agent_api::runtime::heartbeat::{self, Serving};
use agent_api::runtime::reply_gate;
use agent_api::runtime::{Bus, Env, Scope, scopes};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{self, TestEnv};
use crate::redis_env as r;

struct Job {
    e: TestEnv,
    t: Uuid,
    /// The env's root bus (its own `AGENT_PREFIX` on the shared Redis).
    root: Bus,
}

/// An env on the shared Redis: tenant with observer `u-obs`, operator `u-op` and job `j1`.
async fn setup(test: &str) -> Job {
    let vars = r::api_vars(test).await;
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, "Bus Admin Co").await;
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
    fn scope(&self, env: Env) -> Scope {
        Scope::new(self.t, "j1", env).unwrap()
    }

    fn bus(&self, env: Env) -> Bus {
        self.root.scoped(&self.scope(env))
    }

    async fn get(&self, sub: &str, path: &str) -> (StatusCode, Value) {
        let token = common::person(sub, &[]);
        common::send(
            &self.e.app,
            common::get(&format!("/api/jobs/j1{path}"), Some(&token)),
        )
        .await
    }

    async fn call(&self, method: &str, sub: &str, path: &str) -> (StatusCode, Value) {
        let token = common::person(sub, &[]);
        common::send(
            &self.e.app,
            common::json(
                method,
                &format!("/api/jobs/j1{path}"),
                Some(&token),
                json!({}),
            ),
        )
        .await
    }

    async fn audits(&self, action: &str) -> Vec<Value> {
        sqlx::query_scalar(
            "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = $2 ORDER BY id",
        )
        .bind(self.t)
        .bind(action)
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
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

async fn xlen(bus: &Bus, key: &str) -> u64 {
    redis::cmd("XLEN")
        .arg(key)
        .query_async(&mut bus.conn())
        .await
        .unwrap()
}

/// Read one entry of `stream` as `consumer` in `group` and never ack it.
async fn read_unacked(bus: &Bus, stream: &str, group: &str, consumer: &str) {
    bus.ensure_group(stream, group).await.unwrap();
    let _: redis::Value = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg(group)
        .arg(consumer)
        .arg("COUNT")
        .arg(1)
        .arg("STREAMS")
        .arg(stream)
        .arg(">")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
}

fn opts() -> ConsumeOptions {
    ConsumeOptions {
        batch: 16,
        block: Duration::from_millis(100),
        reclaim_idle: Duration::from_millis(200),
        max_deliveries: 2,
    }
}

/// A dead letter the real way: an `op.call` on `bus:op` whose runner handler always fails, until `Bus::consume`
/// gives up. Returns the origin (full key).
async fn dead_letter_via_consume(bus: &Bus, cid: &str) -> String {
    let origin = bus.streams.op();
    bus.publish(&origin, &op_call(cid)).await.unwrap();
    let (b, o) = (bus.clone(), origin.clone());
    let task = tokio::spawn(async move {
        b.consume(&o, groups::RUNNER, "c1", opts(), |_env| async {
            Err::<(), anyhow::Error>(anyhow::anyhow!("always fails"))
        })
        .await
    });
    r::wait_entries(bus, &bus.streams.dead(), 1, |_| true).await;
    task.abort();
    origin
}

/// XADD `env` on `stream` under the explicit entry id `id` (deterministic ordering across streams).
async fn xadd_at(bus: &Bus, stream: &str, id: &str, env: &Envelope) {
    let got: String = redis::cmd("XADD")
        .arg(stream)
        .arg(id)
        .arg("envelope")
        .arg(serde_json::to_string(env).unwrap())
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    assert_eq!(got, id);
}

/// `items[*].key` of a list.
fn keys(list: &Value, field: &str) -> Vec<String> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|r| r[field].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn stream_entries_are_scoped_redacted_and_404_outside_the_bus() {
    let j = setup("ba-entries").await;
    let bus = j.bus(Env::Prod);
    bus.publish(&bus.streams.op(), &op_call("C-1"))
        .await
        .unwrap();

    // colons URL-encoded resolve to the key; K50: the op payload is ids and codes only
    let (s, b) = j.get("u-obs", "/streams/bus%3Aop/entries?limit=10").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"][0]["stream"], "bus:op");
    assert_eq!(b["items"][0]["envelope"]["correlation_id"], "C-1");
    assert_eq!(b["items"][0]["envelope"]["payload"]["call_id"], "C-1");
    assert!(b["items"][0]["envelope"]["payload"].get("inputs").is_none());
    assert!(!b.to_string().contains("femke@example.com"), "{b}");
    assert!(b["next"].is_null());

    // keys outside the bus namespace, or absent, are 404 stream_not_found
    let outside = bus.streams.key("case:CS-1");
    let _: String = redis::cmd("XADD")
        .arg(&outside)
        .arg("*")
        .arg("a")
        .arg("b")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    for path in [
        "/streams/case%3ACS-1/entries",
        "/streams/bus%3Anope/entries",
    ] {
        let (s, b) = j.get("u-obs", path).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{path}: {b}");
        assert_eq!(b["error"]["code"], "stream_not_found");
    }

    // another tenant's job of the same key: its stream is not addressable here
    let other = j
        .root
        .scoped(&Scope::new(Uuid::new_v4(), "j1", Env::Prod).unwrap());
    other
        .publish(&other.streams.customer("faq"), &op_call("C-X"))
        .await
        .unwrap();
    let (s, _) = j
        .get("u-obs", "/streams/bus%3Acustomer%3Afaq/entries")
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // the test env's streams are a separate key space, reached with env=test only
    let test = j.bus(Env::Test);
    test.publish(&test.streams.op_results(), &op_call("C-T"))
        .await
        .unwrap();
    let (s, _) = j.get("u-obs", "/streams/bus%3Aop%3Aresults/entries").await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, b) = j
        .get("u-obs", "/streams/bus%3Aop%3Aresults/entries?env=test")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"][0]["envelope"]["correlation_id"], "C-T");

    let (s, b) = j
        .get("u-obs", "/streams/bus%3Aop/entries?before=nope")
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_cursor");
}

#[tokio::test]
async fn dead_letter_replay_republishes_to_origin_and_is_audited() {
    let j = setup("ba-replay").await;
    let bus = j.bus(Env::Prod);
    let origin = dead_letter_via_consume(&bus, "CS-2").await;

    let (s, list) = j.get("u-obs", "/dead-letters").await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let d = &list["items"][0];
    assert_eq!(d["stream"], "bus:dead");
    assert_eq!(d["env"], "prod");
    assert_eq!(d["dead"]["stream"], "bus:op", "unprefixed origin: {d}");
    assert_eq!(d["dead"]["reason_code"], "gave_up");
    assert_eq!(d["dead"]["deliveries"], 3);
    assert_eq!(d["envelope"]["correlation_id"], "CS-2");
    // K50 inside the dead letter's original too
    assert!(!list.to_string().contains("femke@example.com"), "{list}");
    let id = d["entry_id"].as_str().unwrap().to_string();

    // a member can read dead letters but never replay them
    let (s, b) = j
        .call("POST", "u-obs", &format!("/dead-letters/{id}/replay"))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");

    let (s, b) = j
        .call("POST", "u-op", &format!("/dead-letters/{id}/replay"))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["stream"], "bus:op");
    // published to the stored key as-is: the original stays (XACK does not delete), the replay adds one
    assert_eq!(xlen(&bus, &origin).await, 2);
    let (_, after) = j.get("u-obs", "/dead-letters").await;
    assert!(after["items"].as_array().unwrap().is_empty(), "{after}");
    let rows = j.audits("dead_letter.replay").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["stream"], "bus:op");
    assert_eq!(rows[0]["env"], "prod");
    assert_eq!(rows[0]["new_entry_id"], b["entry_id"]);

    // gone now: 404
    let (s, b) = j
        .call("POST", "u-op", &format!("/dead-letters/{id}/replay"))
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["code"], "dead_letter_not_found");
}

#[tokio::test]
async fn two_concurrent_replays_publish_once() {
    let j = setup("ba-race").await;
    let bus = j.bus(Env::Prod);
    let origin = dead_letter_via_consume(&bus, "CS-3").await;
    let (_, list) = j.get("u-obs", "/dead-letters").await;
    let id = list["items"][0]["entry_id"].as_str().unwrap().to_string();
    let path = format!("/dead-letters/{id}/replay");
    let (a, b) = tokio::join!(j.call("POST", "u-op", &path), j.call("POST", "u-op", &path));
    let mut codes = [a.0.as_u16(), b.0.as_u16()];
    codes.sort_unstable();
    assert_eq!(
        codes,
        [200, 404],
        "exactly one replay wins: {} {}",
        a.1,
        b.1
    );
    assert_eq!(
        xlen(&bus, &origin).await,
        2,
        "1 original + exactly 1 replay"
    );
    assert_eq!(
        j.audits("dead_letter.replay").await.len(),
        1,
        "only the winner is audited"
    );
}

#[tokio::test]
async fn dead_letters_are_paged_newest_first_and_discard_is_audited() {
    let j = setup("ba-page").await;
    let bus = j.bus(Env::Prod);
    let dead = bus.streams.dead();
    // Explicit entry ids: ids of two streams are ordered only by (ms, seq), never by publish order, and entries
    // published in the same millisecond on different streams get independent sequences. Prod holds 1000-0..2; the
    // test env's 1000-5 shares their millisecond and is newest by its sequence (the merge breaks ties on seq).
    let ids: Vec<String> = (0..3).map(|i| format!("1000-{i}")).collect();
    for (i, id) in ids.iter().enumerate() {
        let env = Envelope::new(
            "dead.letter",
            "runner",
            &format!("CS-{i}"),
            json!({"stream": bus.streams.customer("returns"), "entry_id": "1-0", "deliveries": 5,
                   "reason": "gave up after 5 deliveries", "original": null}),
        )
        .unwrap();
        xadd_at(&bus, &dead, id, &env).await;
    }
    let test = j.bus(Env::Test);
    let tid = "1000-5".to_string();
    xadd_at(
        &test,
        &test.streams.dead(),
        &tid,
        &Envelope::new(
            "dead.letter",
            "runner",
            "CS-T",
            json!({"stream": test.streams.op(), "reason": "x", "original": null}),
        )
        .unwrap(),
    )
    .await;

    let (s, p1) = j.get("u-obs", "/dead-letters?limit=2").await;
    assert_eq!(s, StatusCode::OK, "{p1}");
    assert_eq!(
        keys(&p1["items"], "entry_id"),
        [tid.clone(), ids[2].clone()]
    );
    assert_eq!(p1["items"][0]["env"], "test");
    let next = p1["next"]
        .as_str()
        .expect("full page has a cursor")
        .to_string();
    let (_, p2) = j
        .get("u-obs", &format!("/dead-letters?limit=2&before={next}"))
        .await;
    assert_eq!(
        keys(&p2["items"], "entry_id"),
        [ids[1].clone(), ids[0].clone()]
    );
    let (_, p3) = j
        .get(
            "u-obs",
            &format!(
                "/dead-letters?limit=2&before={}",
                p2["next"].as_str().unwrap_or(&ids[0])
            ),
        )
        .await;
    assert!(p3["items"].as_array().unwrap().is_empty(), "{p3}");
    assert!(p3["next"].is_null());
    let (s, _) = j.get("u-obs", "/dead-letters?before=nope").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // no original: not replayable, and left in place
    let (s, b) = j
        .call("POST", "u-op", &format!("/dead-letters/{}/replay", ids[0]))
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "not_replayable");
    assert_eq!(xlen(&bus, &dead).await, 3);

    // discard: 200 once, then 404; one audit row; a member may not
    let path = format!("/dead-letters/{}", ids[0]);
    let (s, _) = j.call("DELETE", "u-obs", &path).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, b) = j.call("DELETE", "u-op", &path).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["discarded"], ids[0].as_str());
    let (s, b) = j.call("DELETE", "u-op", &path).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["code"], "dead_letter_not_found");
    assert_eq!(j.audits("dead_letter.discard").await.len(), 1);
    // the test env's dead letter is discarded in its own scope
    let (s, _) = j
        .call("DELETE", "u-op", &format!("/dead-letters/{tid}"))
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(xlen(&test, &test.streams.dead()).await, 0);
    assert_eq!(
        j.audits("dead_letter.discard").await[1]["env"],
        "test",
        "audited with its env"
    );
}

#[tokio::test]
async fn dead_letters_list_is_503_when_redis_errors_never_empty() {
    // "unavailable is not zero": make bus:dead a plain string, so XREVRANGE fails with WRONGTYPE
    let j = setup("ba-dead-err").await;
    let bus = j.bus(Env::Prod);
    let _: () = redis::cmd("SET")
        .arg(bus.streams.dead())
        .arg("not a stream")
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let (s, b) = j.get("u-obs", "/dead-letters").await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(b["error"]["code"], "unavailable");
}

#[tokio::test]
async fn replay_scope_mismatch_is_409() {
    let j = setup("ba-mismatch").await;
    let bus = j.bus(Env::Prod);
    // another tenant's envelope XADDed raw onto this scope's bus:op: the runner dead-letters it unhandled
    let mut foreign = op_call("CS-F");
    foreign.tenant_id = Some(Uuid::new_v4());
    foreign.job = Some("j1".into());
    foreign.env = Some(Env::Prod);
    let stream = bus.streams.op();
    let _: String = redis::cmd("XADD")
        .arg(&stream)
        .arg("*")
        .arg("envelope")
        .arg(serde_json::to_string(&foreign).unwrap())
        .query_async(&mut bus.conn())
        .await
        .unwrap();
    let (b2, st2) = (bus.clone(), stream.clone());
    let task = tokio::spawn(async move {
        b2.consume(&st2, groups::RUNNER, "c1", opts(), |_env| async {
            Ok::<(), anyhow::Error>(())
        })
        .await
    });
    r::wait_entries(&bus, &bus.streams.dead(), 1, |_| true).await;
    task.abort();

    let (_, list) = j.get("u-obs", "/dead-letters").await;
    assert_eq!(
        list["items"][0]["dead"]["reason_code"], "scope_mismatch",
        "{list}"
    );
    let id = list["items"][0]["entry_id"].as_str().unwrap().to_string();
    let (s, b) = j
        .call("POST", "u-op", &format!("/dead-letters/{id}/replay"))
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "scope_mismatch");
    // refused before anything moved: still a dead letter, the origin untouched, nothing audited
    assert_eq!(xlen(&bus, &bus.streams.dead()).await, 1);
    assert_eq!(xlen(&bus, &stream).await, 1);
    assert!(j.audits("dead_letter.replay").await.is_empty());
    // discarding it is the way out
    let (s, _) = j
        .call("DELETE", "u-op", &format!("/dead-letters/{id}"))
        .await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn claim_unknown_group_404_and_claim_is_audited() {
    let j = setup("ba-claim").await;
    let bus = j.bus(Env::Prod);
    let key = bus.streams.customer("returns");
    bus.publish(&key, &op_call("CS-9")).await.unwrap();
    read_unacked(&bus, &key, "g", "dead-consumer").await;
    let path = "/streams/bus%3Acustomer%3Areturns/groups/g/claim";

    let (s, _) = j.call("POST", "u-obs", path).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "claimStuck is the operator's");
    // not idle for 30 s yet: nothing claimed, still audited
    let (s, b) = j.call("POST", "u-op", path).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["claimed"], 0);
    for p in [
        "/streams/bus%3Acustomer%3Areturns/groups/nope/claim",
        "/streams/case%3ACS-9/groups/g/claim",
        "/streams/bus%3Acustomer%3Areturns/groups/g/claim?env=test",
    ] {
        let (s, b) = j.call("POST", "u-op", p).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{p}: {b}");
        assert_eq!(b["error"]["code"], "group_not_found");
    }
    let rows = j.audits("stream.claim_stuck").await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["claimed"], 0);
}

#[tokio::test]
async fn agent_without_heartbeat_shows_stopped_instance() {
    let j = setup("ba-agents").await;
    let bus = j.bus(Env::Prod);
    bus.publish(&bus.streams.op(), &op_call("CS-A"))
        .await
        .unwrap();
    // a runner read it and died: a consumer in the group, no heartbeat
    read_unacked(&bus, &bus.streams.op(), groups::RUNNER, "runner-ghost-1").await;
    // a live runner: two boots in 24 h (one restart), serving release cfg-1 in prod and draft rev 4 in test
    let serving = Serving::default();
    serving.set(&j.scope(Env::Prod), Some("cfg-1".into()), None);
    serving.set(&j.scope(Env::Test), None, Some(4));
    let now = agent_api::runtime::bus::now_ms();
    heartbeat::record_start(&j.root, groups::RUNNER, "runner-live-2", now - 60_000)
        .await
        .unwrap();
    heartbeat::record_start(&j.root, groups::RUNNER, "runner-live-2", now)
        .await
        .unwrap();
    heartbeat::beat(
        &j.root,
        groups::RUNNER,
        "runner-live-2",
        now,
        &serving,
        Duration::from_secs(60),
    )
    .await
    .unwrap();

    let (s, b) = j.get("u-obs", "/agents").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        keys(&b["items"], "id"),
        ["interaction", "orchestrator", "runner", "post_gate"]
    );
    let runner = &b["items"][2];
    assert_eq!(runner["kind"], "runner");
    assert_eq!(runner["group"], "runner");
    assert_eq!(runner["consumers"], 1);
    assert_eq!(runner["pending"], 1);
    assert_eq!(runner["status"], "ok");
    assert_eq!(runner["streams"], json!(["bus:op"]));
    assert!(runner["oldest_pending_s"].is_u64(), "{runner}");
    assert_eq!(runner["restarts_24h"], 1);
    let inst = runner["instances"].as_array().unwrap();
    let ghost = inst.iter().find(|i| i["name"] == "runner-ghost-1").unwrap();
    assert_eq!(ghost["state"], "stopped", "{ghost}");
    assert!(ghost["last_heartbeat"].is_null());
    let live = inst.iter().find(|i| i["name"] == "runner-live-2").unwrap();
    assert_eq!(live["state"], "running", "{live}");
    assert_eq!(live["restarts"], 1);
    assert!(live["last_heartbeat"].is_string());
    assert_eq!(runner["prod"]["release"], "cfg-1");
    assert!(runner["prod"]["rev"].is_null());
    assert_eq!(runner["test"]["rev"], 4);
    assert!(runner["test"]["release"].is_null());
    assert_eq!(runner["version"], env!("CARGO_PKG_VERSION"));
    // no consumer and no beat: idle, no test copy, no instances
    let inter = &b["items"][0];
    assert_eq!(inter["status"], "idle");
    assert!(inter["test"].is_null(), "{inter}");
    assert!(inter["instances"].as_array().unwrap().is_empty());
    assert!(inter["last_heartbeat"].is_null());
    // R1 without a beat: idle, no instances
    let gate = &b["items"][3];
    assert_eq!(gate["kind"], "gate");
    assert_eq!(gate["status"], "idle");
    assert!(gate["instances"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn the_post_gate_is_reported_from_its_own_beats() {
    let j = setup("ba-post-gate").await;
    let serving = Serving::default();
    serving.set(&j.scope(Env::Prod), Some("cfg-2".into()), None);
    let now = agent_api::runtime::bus::now_ms();
    heartbeat::record_start(&j.root, reply_gate::ROLE, "interaction-7", now)
        .await
        .unwrap();
    heartbeat::beat(
        &j.root,
        reply_gate::ROLE,
        "interaction-7",
        now,
        &serving,
        Duration::from_secs(60),
    )
    .await
    .unwrap();

    let (s, b) = j.get("u-obs", "/agents").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let gate = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["id"] == "post_gate")
        .expect("post_gate listed")
        .clone();
    assert_eq!(gate["kind"], "gate");
    assert_eq!(gate["group"], "post_gate");
    assert_eq!(gate["status"], "ok", "{gate}");
    assert_eq!(gate["consumers"], 1);
    assert_eq!(gate["pending"], 0);
    assert_eq!(gate["lag"], 0);
    assert_eq!(gate["streams"], json!([]));
    assert!(
        gate["note"]
            .as_str()
            .unwrap()
            .contains("interaction daemon")
    );
    assert!(gate["last_heartbeat"].is_string());
    assert_eq!(gate["prod"]["release"], "cfg-2");
    let inst = gate["instances"].as_array().unwrap();
    assert_eq!(inst.len(), 1);
    assert_eq!(inst[0]["name"], "interaction-7");
    assert_eq!(inst[0]["state"], "running");
    // the interaction agent's own group numbers are untouched by R1's beat
    assert_eq!(b["items"][0]["status"], "idle");
}

#[tokio::test]
async fn agent_rates_come_from_the_handlers_bus_counts() {
    let j = setup("ba-rates").await;
    let bus = j.bus(Env::Prod);
    let stream = bus.streams.op();
    for i in 0..3 {
        bus.publish(&stream, &op_call(&format!("CS-R{i}")))
            .await
            .unwrap();
    }
    let (b2, st2) = (bus.clone(), stream.clone());
    let task = tokio::spawn(async move {
        b2.consume(&st2, groups::RUNNER, "c1", opts(), |_env| async {
            Ok::<(), anyhow::Error>(())
        })
        .await
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    let runner = loop {
        let (s, b) = j.get("u-obs", "/agents").await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let runner = b["items"][2].clone();
        // the handled counter can reach 3 before the last XACK clears the pending list: wait for both
        if runner["handled_1h"] == 3 && runner["pending"] == 0 {
            break runner;
        }
        assert!(
            Instant::now() < deadline,
            "handled_1h never reached 3 with nothing pending: {runner}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    task.abort();
    assert_eq!(runner["errors_1h"], 0);
    assert_eq!(runner["per_min"], 0.6, "3 handled over the 5-minute window");
    assert_eq!(runner["pending"], 0);
    // the orchestrator, which never handled an entry, has no rate
    let (_, b) = j.get("u-obs", "/agents").await;
    assert_eq!(b["items"][1]["handled_1h"], 0);
}

#[tokio::test]
async fn test_env_flow_uses_test_prefix() {
    let j = setup("ba-flow").await;
    let (prod, test) = (j.bus(Env::Prod), j.bus(Env::Test));
    let qualified = Envelope::new(
        "returns.request.qualified",
        "agent-api",
        "CS-Q",
        json!({"case_id": "CS-Q"}),
    )
    .unwrap();
    test.publish(&test.streams.customer("returns"), &qualified)
        .await
        .unwrap();
    test.publish(&test.streams.op(), &op_call("CS-Q"))
        .await
        .unwrap();
    prod.publish(&prod.streams.customer("faq"), &qualified)
        .await
        .unwrap();

    let (s, f) = j.get("u-obs", "/bus-flow?env=test").await;
    assert_eq!(s, StatusCode::OK, "{f}");
    assert_eq!(f["env"], "test");
    assert_eq!(f["prefix"], format!("s:{}:j1:test:", j.t));
    assert_eq!(f["reads"], "draft");
    assert_eq!(
        keys(&f["topics"], "key"),
        ["bus:customer:returns", "bus:op"]
    );
    let op = &f["topics"][1];
    assert_eq!(op["bus"], "op");
    assert_eq!(op["producers"], json!(["orchestrator"]));
    assert_eq!(op["rate_per_min"], 0.2, "1 entry over 5 minutes");
    assert_eq!(op["maxlen"], 100_000);
    let hop = |id: &str| {
        f["hops"]
            .as_array()
            .unwrap()
            .iter()
            .find(|h| h["id"] == id)
            .unwrap_or_else(|| panic!("no hop {id}: {f}"))
            .clone()
    };
    assert_eq!(hop("qualified")["rate_per_min"], 0.2);
    assert_eq!(hop("qualified")["streams"], json!(["bus:customer:returns"]));
    assert_eq!(hop("channel-in")["label"], "Test chat (staff)");
    assert_eq!(hop("orchestrator")["status"], "idle", "nobody consumes");

    let (s, f) = j.get("u-obs", "/bus-flow?env=prod").await;
    assert_eq!(s, StatusCode::OK, "{f}");
    assert_eq!(f["prefix"], format!("s:{}:j1:", j.t));
    assert_eq!(f["reads"], "live");
    assert_eq!(keys(&f["topics"], "key"), ["bus:customer:faq"]);

    let (s, b) = j.get("u-obs", "/bus-flow?env=live").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_query");
}

#[tokio::test]
async fn agent_fleet_reports_the_draft_revision_and_no_release() {
    let j = setup("ba-fleet").await;
    let rev: i64 =
        sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE tenant_id = $1 AND key = 'j1'")
            .bind(j.t)
            .fetch_one(&j.e.owner)
            .await
            .unwrap();
    let (s, f) = j.get("u-obs", "/agents/fleet").await;
    assert_eq!(s, StatusCode::OK, "{f}");
    assert!(f["production"]["release"].is_null(), "{f}");
    assert!(f["production"]["released_at"].is_null());
    assert_eq!(f["test"]["rev"], rev);
    assert_eq!(f["test"]["unpublished"], 0);
    assert!(f["approved_waiting"].is_null());
    assert_eq!(f["follow_s"], 35);
    assert!(f["as_of"].is_string());
}

#[tokio::test]
async fn without_redis_the_bus_reads_are_503_and_the_fleet_answers() {
    let e = common::env().await;
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, "No Redis Co").await;
    common::insert_user(&e.owner, "u-obs", t, "tenant_user").await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'j1', 'J', '[]', 'test')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    assert!(e.state.redis.is_none());
    let token = common::person("u-obs", &[]);
    for path in [
        "/agents",
        "/bus-flow",
        "/dead-letters",
        "/streams/bus%3Aop/entries",
    ] {
        let (s, b) = common::send(
            &e.app,
            common::get(&format!("/api/jobs/j1{path}"), Some(&token)),
        )
        .await;
        assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{path}: {b}");
        assert_eq!(b["error"]["code"], "unavailable");
    }
    let (s, b) = common::send(
        &e.app,
        common::get("/api/jobs/j1/agents/fleet", Some(&token)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn gauges_export_the_redis_numbers() {
    let j = setup("ba-gauges").await;
    let scope = j.scope(Env::Prod);
    scopes::add(&j.root, &scope).await.unwrap();
    let bus = j.bus(Env::Prod);
    for i in 0..2 {
        bus.publish(&bus.streams.op(), &op_call(&format!("CS-G{i}")))
            .await
            .unwrap();
    }
    read_unacked(&bus, &bus.streams.op(), groups::RUNNER, "runner-x").await;
    read_unacked(&bus, &bus.streams.op(), groups::RUNNER, "runner-x").await;
    bus.publish(
        &bus.streams.dead(),
        &Envelope::new("dead.letter", "runner", "CS-D", json!({"reason": "x"})).unwrap(),
    )
    .await
    .unwrap();
    let now = agent_api::runtime::bus::now_ms();
    heartbeat::beat(
        &j.root,
        groups::ORCHESTRATOR,
        "orchestrator-1",
        now,
        &Serving::default(),
        Duration::from_secs(60),
    )
    .await
    .unwrap();

    let g = agent_api::bus_admin::read_gauges(&j.e.state.db, &j.root)
        .await
        .unwrap();
    assert_eq!(g.pending.get("runner"), Some(&2));
    assert_eq!(g.pending.get("orchestrator"), Some(&0));
    assert_eq!(g.lag.get("runner"), Some(&0));
    assert_eq!(g.dead_letters, 1);
    assert_eq!(g.handover_breached, 0);
    let age = g.heartbeat_age_s["orchestrator"].expect("a live orchestrator");
    assert!((0.0..60.0).contains(&age), "{age}");
    assert_eq!(g.heartbeat_age_s["runner"], None, "no runner beats");

    // exported on the process's recorder (installed first: a gauge set before it exists is lost; label sets carry
    // no tenant)
    let handle = agent_api::observability::metrics_handle();
    agent_api::bus_admin::refresh_gauges(&j.e.state.db, &j.root)
        .await
        .unwrap();
    let text = handle.render();
    let value = |series: &str| -> f64 {
        let prefix = format!("{series} ");
        text.lines()
            .find_map(|l| l.strip_prefix(prefix.as_str()))
            .unwrap_or_else(|| panic!("no series {series}:\n{text}"))
            .trim()
            .parse()
            .unwrap()
    };
    assert_eq!(value("agent_bus_pending{group=\"runner\"}"), 2.0);
    assert_eq!(value("agent_bus_lag{group=\"runner\"}"), 0.0);
    assert_eq!(value("agent_dead_letters"), 1.0);
    assert_eq!(value("agent_handover_breached"), 0.0);
    assert!(value("agent_heartbeat_age_seconds{role=\"orchestrator\"}") < 60.0);
    assert!(
        value("agent_heartbeat_age_seconds{role=\"runner\"}").is_infinite(),
        "no live runner is loud, not 0"
    );
    assert!(
        !text.contains(&j.t.to_string()),
        "no tenant id in any label"
    );
}
