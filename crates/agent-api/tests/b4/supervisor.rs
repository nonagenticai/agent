//! B4 T1 — the scope registry and the per-scope supervisor (ported from care-bus-agents@96c608c
//! `crates/carebus/tests/supervisor.rs`): scopes start and stop with the registry, a failed scope task is restarted;
//! and the heartbeat (crate::runtime::heartbeat) expires when a process stops beating.
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_api::runtime::heartbeat::{self, Serving, Timing};
use agent_api::runtime::{Scope, scopes, supervisor};

use crate::redis_env as r;

/// Sets its flag when dropped: the scope task was aborted.
struct DropFlag(Arc<AtomicBool>);
impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, SeqCst);
    }
}

#[tokio::test]
async fn registry_lists_replaces_and_skips_garbage() {
    let root = r::root("registry").await;
    let (a, b, at) = (r::scope_a(), r::scope_b(), r::scope_a_test());
    assert!(scopes::list(&root).await.unwrap().is_empty());
    scopes::add(&root, &a).await.unwrap();
    scopes::add(&root, &a).await.unwrap();
    let _: i64 = redis::cmd("SADD")
        .arg(root.root_key(scopes::SCOPES_KEY))
        .arg("not-a-scope")
        .query_async(&mut root.conn())
        .await
        .unwrap();
    // a scope without its env (the care-bus form) is garbage here
    let _: i64 = redis::cmd("SADD")
        .arg(root.root_key(scopes::SCOPES_KEY))
        .arg(format!("{}/{}", a.tenant, a.job))
        .query_async(&mut root.conn())
        .await
        .unwrap();
    assert_eq!(scopes::list(&root).await.unwrap(), vec![a.clone()]);
    scopes::replace_all(&root, &[b.clone(), b.clone(), at.clone()])
        .await
        .unwrap();
    assert_eq!(
        scopes::list(&root).await.unwrap(),
        vec![at.clone(), b.clone()]
    );
    scopes::remove(&root, &b).await.unwrap();
    assert_eq!(scopes::list(&root).await.unwrap(), vec![at.clone()]);
    scopes::add(&root, &a).await.unwrap();
    scopes::replace_all(&root, &[]).await.unwrap();
    assert!(scopes::list(&root).await.unwrap().is_empty());
    // the registry lives at the environment root, not under any scope
    let scoped_members: Vec<String> = redis::cmd("SMEMBERS")
        .arg(root.scoped(&a).streams.key(scopes::SCOPES_KEY))
        .query_async(&mut root.conn())
        .await
        .unwrap();
    assert!(scoped_members.is_empty());
}

#[tokio::test]
async fn set_tenant_touches_only_that_tenants_members() {
    let root = r::root("set-tenant").await;
    let (a, b, at) = (r::scope_a(), r::scope_b(), r::scope_a_test());
    scopes::replace_all(&root, &[a.clone(), b.clone()])
        .await
        .unwrap();
    assert!(
        scopes::set_tenant(&root, a.tenant, &[a.clone(), at.clone()])
            .await
            .unwrap()
    );
    assert_eq!(
        scopes::list(&root).await.unwrap(),
        vec![a.clone(), at.clone(), b.clone()]
    );
    assert!(
        !scopes::set_tenant(&root, a.tenant, &[a.clone(), at.clone()])
            .await
            .unwrap(),
        "nothing changed: no write, no nudge"
    );
    // another tenant's scope in `want` is ignored
    assert!(
        scopes::set_tenant(&root, a.tenant, std::slice::from_ref(&b))
            .await
            .unwrap()
    );
    assert_eq!(scopes::list(&root).await.unwrap(), vec![b]);
}

#[tokio::test]
async fn scopes_start_and_stop() {
    let root = r::root("sup").await;
    let started = Arc::new(Mutex::new(Vec::<Scope>::new()));
    let a_dropped = Arc::new(AtomicBool::new(false));
    let (st, ad) = (started.clone(), a_dropped.clone());
    let sup = tokio::spawn(supervisor::run_per_scope(
        root.clone(),
        "test",
        move |bus, s| {
            let (st, ad) = (st.clone(), ad.clone());
            async move {
                assert_eq!(bus.scope(), Some(&s), "the task gets its scope's bus");
                let _guard = (s == r::scope_a()).then(|| DropFlag(ad));
                st.lock().unwrap().push(s);
                std::future::pending::<()>().await;
                Ok(())
            }
        },
    ));
    // the supervisor subscribes before it reads the registry; give it a moment, the 30 s resync is the backstop
    tokio::time::sleep(Duration::from_millis(200)).await;
    scopes::add(&root, &r::scope_a()).await.unwrap();
    r::eventually(|| started.lock().unwrap().contains(&r::scope_a())).await;
    // the test env of the same job is its own scope, with its own task
    scopes::add(&root, &r::scope_a_test()).await.unwrap();
    r::eventually(|| started.lock().unwrap().len() == 2).await;
    scopes::add(&root, &r::scope_b()).await.unwrap();
    r::eventually(|| started.lock().unwrap().len() == 3).await;
    assert!(!a_dropped.load(SeqCst));
    scopes::remove(&root, &r::scope_a()).await.unwrap();
    r::eventually(|| a_dropped.load(SeqCst)).await;
    // the others keep running and nothing was started twice
    assert_eq!(started.lock().unwrap().len(), 3);
    assert!(!sup.is_finished());
    sup.abort();
}

#[tokio::test]
async fn restarts_a_failed_scope_task() {
    let root = r::root("sup-restart").await;
    scopes::add(&root, &r::scope_a()).await.unwrap();
    let calls = Arc::new(AtomicU32::new(0));
    let c = calls.clone();
    let sup = tokio::spawn(supervisor::run_per_scope(
        root.clone(),
        "test",
        move |_bus, _s| {
            let c = c.clone();
            async move {
                if c.fetch_add(1, SeqCst) == 0 {
                    anyhow::bail!("first run fails");
                }
                std::future::pending::<()>().await;
                Ok(())
            }
        },
    ));
    let deadline = Instant::now() + Duration::from_secs(7);
    while calls.load(SeqCst) < 2 {
        assert!(
            Instant::now() < deadline,
            "restarted {} times",
            calls.load(SeqCst)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    sup.abort();
}

async fn exists(root: &agent_api::runtime::Bus, key: &str) -> bool {
    redis::cmd("EXISTS")
        .arg(key)
        .query_async::<i64>(&mut root.conn())
        .await
        .unwrap()
        == 1
}

#[tokio::test]
async fn heartbeat_expires_after_60s() {
    let root = r::root("hb").await;
    // the production timing: a beat every 10 s that lives 60 s
    assert_eq!(heartbeat::INTERVAL, Duration::from_secs(10));
    assert_eq!(heartbeat::TTL, Duration::from_secs(60));
    let serving = Serving::default();
    serving.set(&r::scope_a_test(), None, Some(7));
    let task = tokio::spawn(heartbeat::run(
        root.clone(),
        "runner".into(),
        "runner-1".into(),
        serving.clone(),
    ));
    let key = heartbeat::key(&root, "runner", "runner-1");
    let mut fields = std::collections::HashMap::<String, String>::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while fields.is_empty() {
        assert!(Instant::now() < deadline, "no beat within 5 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
        fields = redis::cmd("HGETALL")
            .arg(&key)
            .query_async(&mut root.conn())
            .await
            .unwrap();
    }
    task.abort();
    assert_eq!(fields["role"], "runner");
    assert_eq!(fields["consumer"], "runner-1");
    assert_eq!(fields["version"], env!("CARGO_PKG_VERSION"));
    let serving_json: serde_json::Value = serde_json::from_str(&fields["serving"]).unwrap();
    assert_eq!(
        serving_json[r::scope_a_test().to_string()]["rev"],
        serde_json::json!(7)
    );
    assert_eq!(
        serving_json[r::scope_a_test().to_string()]["release"],
        serde_json::Value::Null
    );
    let ttl_ms: i64 = redis::cmd("PTTL")
        .arg(&key)
        .query_async(&mut root.conn())
        .await
        .unwrap();
    assert!(ttl_ms > 50_000 && ttl_ms <= 60_000, "{ttl_ms}");
    // the boot was recorded for restarts_24h
    let starts: Vec<String> = redis::cmd("ZRANGE")
        .arg(heartbeat::starts_key(&root, "runner"))
        .arg(0)
        .arg(-1)
        .query_async(&mut root.conn())
        .await
        .unwrap();
    assert_eq!(starts.len(), 1, "{starts:?}");
    assert!(starts[0].starts_with("runner-1@"));

    // a process that stops beating disappears once the expiry passes (scaled down: a 1 s expiry)
    let key2 = heartbeat::key(&root, "runner", "runner-2");
    let task = tokio::spawn(heartbeat::run_with(
        root.clone(),
        "runner".into(),
        "runner-2".into(),
        Serving::default(),
        Timing {
            interval: Duration::from_millis(200),
            ttl: Duration::from_secs(1),
        },
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !exists(&root, &key2).await {
        assert!(Instant::now() < deadline, "no beat within 5 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // still alive while beating, past one expiry
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(
        exists(&root, &key2).await,
        "a beating process stays registered"
    );
    task.abort();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert!(!exists(&root, &key2).await, "a silent process expires");
}
