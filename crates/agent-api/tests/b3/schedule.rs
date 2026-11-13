//! B3 T8b (D82): the source scheduler is built and switched off. Boot refuses `SOURCE_SCHEDULE_ENABLED=true`
//! (`source_schedule_disabled_by_policy`); the disabled flag spawns nothing and `GET …/sources/{id}` answers
//! `schedule_active: false`. The tick itself (reached only through the test constructor `Scheduler::unguarded`)
//! starts due sources once through the `resyncSource` path as the system actor, skips disabled, file and running
//! sources, evaluates the cron in UTC, and runs on one replica at a time (advisory try-lock). Real Postgres 18 (one
//! container for this binary, a fresh database per test) and the in-process strata fakes (`common::strata_fake`).
use crate::common;

use std::collections::HashMap;

use crate::common::strata_fake::{self, StrataFake, lock};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use agent_api::config::Config;
use agent_api::knowledge::schedule::{self, Cron, Scheduler, Tick};
use axum::http::StatusCode;
use base64::Engine as _;
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

const JOB: &str = "support";

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    vars.insert(
        "PROVIDER_SECRET_KEYS".to_string(),
        format!(r#"{{"s1":"{key}"}}"#),
    );
    vars.insert("PROVIDER_SECRET_KEY_CURRENT".to_string(), "s1".to_string());
}

struct W {
    e: TestEnv,
    fake: StrataFake,
    tenant: Uuid,
    leaf: Uuid,
    /// knowledge_manager
    km: String,
    /// an observer (no role)
    obs: String,
}

async fn member(e: &TestEnv, tenant: Uuid, role: &str) -> String {
    let sub = format!("u-{role}-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, role).await;
    person(&sub, &[])
}

/// A fresh tenant with the job `support` (nl, en), a leaf bound to a strata store, the keyring and the fakes.
async fn world() -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| {
        fake.env(&[tenant], v);
        with_keys(v);
    })
    .await;
    insert_tenant(&e.owner, tenant, &format!("T8b {tenant}")).await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, languages, created_by) \
         VALUES ($1, $2, $2, '[]', ARRAY['nl', 'en'], 'test')",
    )
    .bind(tenant)
    .bind(JOB)
    .execute(&e.owner)
    .await
    .unwrap();
    let sid = lock(&fake.rag).add_store(tenant, None, "leaf", "faq.policy");
    let leaf = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO knowledge_store (tenant_id, id, key, label, kind, strata_store_id, region, updated_by) \
         VALUES ($1, $2, 'faq.policy', 'FAQ policy', 'leaf', $3, 'eu', 'test')",
    )
    .bind(tenant)
    .bind(leaf)
    .bind(sid)
    .execute(&e.owner)
    .await
    .unwrap();
    let km = member(&e, tenant, "knowledge_manager").await;
    let obs = member(&e, tenant, "tenant_user").await;
    W {
        e,
        fake,
        tenant,
        leaf,
        km,
        obs,
    }
}

fn src_uri(rest: &str) -> String {
    format!("/api/jobs/{JOB}/sources{rest}")
}

fn at(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

impl W {
    fn website(&self, name: &str, schedule: Option<&str>) -> Value {
        json!({
            "name": name, "type": "website", "config": {"urls": ["https://help.example.nl/returns"]},
            "mapping": [], "target_store_id": self.leaf, "schedule": schedule, "enabled": true
        })
    }

    async fn create(&self, body: Value) -> Uuid {
        let (s, b) = send(
            &self.e.app,
            json("POST", &src_uri(""), Some(&self.km), body),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        Uuid::parse_str(b["id"].as_str().unwrap()).unwrap()
    }

    /// Moves source `id`'s creation (and its runs' starts) `minutes` into the past, so a cron fire lies between.
    async fn backdate(&self, id: Uuid, minutes: i32) {
        sqlx::query(
            "UPDATE source SET created_at = created_at - make_interval(mins => $2) WHERE id = $1",
        )
        .bind(id)
        .bind(minutes)
        .execute(&self.e.owner)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE source_run SET started_at = started_at - make_interval(mins => $2) WHERE source_id = $1",
        )
        .bind(id)
        .bind(minutes)
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    async fn set_created(&self, id: Uuid, t: DateTime<Utc>) {
        sqlx::query("UPDATE source SET created_at = $2 WHERE id = $1")
            .bind(id)
            .bind(t)
            .execute(&self.e.owner)
            .await
            .unwrap();
    }

    fn scheduler(&self) -> Scheduler {
        Scheduler::unguarded(self.e.state.clone(), self.tenant)
    }

    async fn tick(&self, now: DateTime<Utc>) -> Tick {
        self.scheduler()
            .tick_at(now)
            .await
            .unwrap()
            .expect("no other replica holds this tenant's tick lock")
    }

    fn ingest_creates(&self) -> usize {
        self.fake
            .ingest_requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path == "/api/ingest/jobs")
            .count()
    }

    async fn runs_of(&self, id: Uuid) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM source_run WHERE source_id = $1")
            .bind(id)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }
}

// ---- boot / flag -------------------------------------------------------------------------------------------------

#[test]
fn boot_refuses_schedule_enabled_true() {
    let cfg = |v: Option<&str>| {
        let mut vars = common::base_env("postgres://owner:pw@127.0.0.1:5432/b3_schedule_cfg");
        if let Some(v) = v {
            vars.insert("SOURCE_SCHEDULE_ENABLED".to_string(), v.to_string());
        }
        Config::from_lookup(|k| vars.get(k).cloned())
    };
    for on in ["true", "1", "yes", "on", "TRUE"] {
        let err = cfg(Some(on)).expect_err("enabling the schedule is refused at boot");
        assert!(
            format!("{err:#}").contains("source_schedule_disabled_by_policy"),
            "{err:#}"
        );
    }
    assert!(
        !cfg(None).unwrap().source_schedule_enabled,
        "off by default"
    );
    assert!(!cfg(Some("false")).unwrap().source_schedule_enabled);
    assert!(cfg(Some("maybe")).is_err(), "a malformed flag fails loud");
}

#[tokio::test]
async fn disabled_scheduler_spawns_no_task() {
    let e = common::env().await;
    assert!(!e.state.cfg.source_schedule_enabled);
    assert!(
        schedule::spawn(e.state.clone()).is_none(),
        "the disabled flag spawns nothing"
    );
    assert!(!schedule::active());
}

#[tokio::test]
async fn source_view_answers_schedule_active_false() {
    let w = world().await;
    let id = w.create(w.website("Nightly", Some("0 3 * * *"))).await;
    let (s, b) = send(&w.e.app, get(&src_uri(&format!("/{id}")), Some(&w.obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["schedule"], "0 3 * * *");
    assert_eq!(b["schedule_active"], false, "{b}");
    let (s, list) = send(&w.e.app, get(&src_uri(""), Some(&w.obs))).await;
    assert_eq!(s, StatusCode::OK, "{list}");
    let items = list["items"].as_array().expect("a source list");
    assert!(
        items.iter().all(|i| i["schedule_active"] == false),
        "{list}"
    );
}

#[tokio::test]
async fn out_of_range_schedule_422_invalid_schedule() {
    let w = world().await;
    let (s, b) = send(
        &w.e.app,
        json(
            "POST",
            &src_uri(""),
            Some(&w.km),
            w.website("Bad hour", Some("0 24 * * *")),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_schedule", "{b}");
}

// ---- the tick (test constructor; never spawned) ------------------------------------------------------------------

#[tokio::test]
async fn scheduler_runs_due_sources_once() {
    let w = world().await;
    let id = w.create(w.website("Every minute", Some("* * * * *"))).await;
    w.backdate(id, 10).await;

    let r = w.tick(Utc::now()).await;
    assert_eq!(
        r,
        Tick {
            started: 1,
            skipped: 0,
            failed: 0
        }
    );
    assert_eq!(w.ingest_creates(), 1);
    let (started_by, started_at): (String, DateTime<Utc>) =
        sqlx::query_as("SELECT started_by, started_at FROM source_run WHERE source_id = $1")
            .bind(id)
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    assert_eq!(started_by, schedule::SYSTEM);
    let audit: Vec<(String, String, Value)> = sqlx::query_as(
        "SELECT actor_id, actor_type, details FROM audit_event \
          WHERE tenant_id = $1 AND action = 'source.resync' AND resource_id = $2",
    )
    .bind(w.tenant)
    .bind(id.to_string())
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0].0, schedule::SYSTEM);
    assert_eq!(audit[0].1, "system");
    assert_eq!(audit[0].2["trigger"], "schedule", "{audit:?}");

    // settle the job: the run is no longer running, yet the fire it answered is not run again
    {
        let mut g = lock(&w.fake.ingest);
        for (_, j) in g.jobs.iter_mut() {
            j["status"] = json!("completed");
            j["document_ids"] = json!([Uuid::new_v4().to_string()]);
        }
    }
    let (s, runs) = send(
        &w.e.app,
        get(&src_uri(&format!("/{id}/runs")), Some(&w.obs)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{runs}");
    assert_ne!(runs["items"][0]["status"], "running", "{runs}");
    let again = w.tick(started_at).await;
    assert_eq!(again, Tick::default(), "the same fire runs once");
    assert_eq!(w.ingest_creates(), 1);
    assert_eq!(w.runs_of(id).await, 1);
}

#[tokio::test]
async fn scheduler_tick_skips_while_another_replica_holds_the_lock() {
    let w = world().await;
    let id = w.create(w.website("Locked", Some("* * * * *"))).await;
    w.backdate(id, 10).await;
    let mut other = w.e.owner.acquire().await.unwrap();
    let key = format!("agent:source-schedule:{}", w.tenant);
    let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
        .bind(&key)
        .fetch_one(&mut *other)
        .await
        .unwrap();
    assert!(got);
    let r = w.scheduler().tick_at(Utc::now()).await.unwrap();
    assert!(r.is_none(), "the other replica runs this tick");
    assert_eq!(w.ingest_creates(), 0);
    sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
        .bind(&key)
        .execute(&mut *other)
        .await
        .unwrap();
    drop(other);
    assert_eq!(w.tick(Utc::now()).await.started, 1);
}

#[tokio::test]
async fn scheduler_skips_disabled_and_running_sources() {
    let w = world().await;
    let mut disabled = w.website("Disabled", Some("* * * * *"));
    disabled["enabled"] = json!(false);
    let disabled = w.create(disabled).await;
    let unscheduled = w.create(w.website("Manual only", None)).await;
    let file = w
        .create(json!({
            "name": "Uploads", "type": "file", "config": {}, "mapping": [], "target_store_id": w.leaf,
            "schedule": "* * * * *", "enabled": true
        }))
        .await;
    let running = w.create(w.website("Running", Some("* * * * *"))).await;
    let (s, b) = send(
        &w.e.app,
        json(
            "POST",
            &src_uri(&format!("/{running}/resync")),
            Some(&w.km),
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    for id in [disabled, unscheduled, file, running] {
        w.backdate(id, 10).await;
    }
    assert_eq!(w.ingest_creates(), 1);

    let r = w.tick(Utc::now()).await;
    assert_eq!(
        r,
        Tick {
            started: 0,
            skipped: 1,
            failed: 0
        },
        "only the running source was due, and it was skipped"
    );
    assert_eq!(w.ingest_creates(), 1, "nothing new reached strata_ingest");
    assert_eq!(w.runs_of(disabled).await, 0);
    assert_eq!(w.runs_of(unscheduled).await, 0);
    assert_eq!(w.runs_of(file).await, 0);
    assert_eq!(w.runs_of(running).await, 1);
}

#[tokio::test]
async fn scheduler_respects_cron_and_timezone_utc() {
    // pure: the cron is read in UTC, whatever offset `now` is written in
    let daily = Cron::parse("0 3 * * *").unwrap();
    let since = at("2026-10-20T00:00:00Z");
    assert!(!due_at(&daily, since, "2026-10-20T02:59:59Z"));
    assert!(due_at(&daily, since, "2026-10-20T03:00:00Z"));
    assert!(
        !due_at(&daily, since, "2026-10-20T03:00:00+02:00"),
        "03:00 at +02:00 is 01:00 UTC"
    );
    assert!(due_at(&daily, since, "2026-10-20T05:00:00+02:00"));
    assert!(!due_at(
        &daily,
        at("2026-10-20T03:00:10Z"),
        "2026-10-20T03:30:00Z"
    ));

    // through the tick: nothing before the UTC fire, one run at it
    let w = world().await;
    let id = w.create(w.website("Nightly", Some("0 3 * * *"))).await;
    w.set_created(id, since).await;
    assert_eq!(
        w.tick(at("2026-10-20T03:30:00+02:00")).await,
        Tick::default(),
        "01:30 UTC"
    );
    assert_eq!(w.tick(at("2026-10-20T02:59:30Z")).await, Tick::default());
    assert_eq!(w.runs_of(id).await, 0);
    assert_eq!(w.tick(at("2026-10-20T05:00:30+02:00")).await.started, 1);
    assert_eq!(w.runs_of(id).await, 1);
    assert_eq!(w.ingest_creates(), 1);
}

fn due_at(c: &Cron, since: DateTime<Utc>, now: &str) -> bool {
    schedule::due(c, since, at(now))
}
