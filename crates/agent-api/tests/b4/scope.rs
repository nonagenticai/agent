//! B4 T1 — scopes with their env (crate::runtime::scope), the registry agent-api keeps from the database
//! (crate::runtime::scopes: boot reconcile + `changed` after tenant-status writes and job creates), config nudges,
//! and agent-api without Redis.
use std::collections::BTreeSet;
use std::time::Duration;

use agent_api::runtime::bus::Streams;
use agent_api::runtime::{Bus, Env, Scope, config_nudge, scopes};
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

use crate::common::{self, TestEnv};
use crate::redis_env as r;

/// Every key name the runtime builds under a scope (bus, console, turn and phase machinery).
fn all_keys(root: &str, s: &Scope) -> Vec<String> {
    let st = Streams::scoped(root, s);
    vec![
        st.customer("returns"),
        st.op(),
        st.op_results(),
        st.dead(),
        st.console(),
        st.console_recent(),
        st.chat(),
        st.key("case:CS-1"),
        st.key("cases:deadline"),
        st.key("chat:conv-1:inbox"),
        st.key("replied:CS-1"),
        st.key("done:call:c1"),
        st.key("deliveries:x:runner:1-0"),
    ]
}

#[test]
fn test_scope_keys_never_collide_with_prod() {
    let tenants = [
        Uuid::parse_str(r::BOOT_TENANT).unwrap(),
        Uuid::parse_str(r::OTHER_TENANT).unwrap(),
    ];
    // `test` as a job key is the hardest case: its prod prefix is `s:{t}:test:`
    let jobs = ["customer_care_agent", "test", "t"];
    for root in ["", "agent:", "t:x:"] {
        let mut prod = BTreeSet::new();
        let mut test = BTreeSet::new();
        for t in tenants {
            for j in jobs {
                for env in Env::ALL {
                    let s = Scope::new(t, j, env).unwrap();
                    for k in all_keys(root, &s) {
                        // every key maps back to exactly its own scope
                        assert_eq!(Scope::owner_of(root, &k).as_ref(), Some(&s), "{k}");
                        let fresh = match env {
                            Env::Prod => prod.insert(k.clone()),
                            Env::Test => test.insert(k.clone()),
                        };
                        assert!(fresh, "{k} built twice");
                    }
                }
            }
        }
        assert!(prod.is_disjoint(&test), "root {root:?}");
        // root keys belong to no scope
        assert_eq!(Scope::owner_of(root, &format!("{root}scopes")), None);
        assert_eq!(Scope::owner_of(root, &format!("{root}hb:runner:c1")), None);
    }
}

#[test]
#[should_panic(expected = "a prod scope key may not start with")]
fn a_prod_key_in_the_test_key_space_is_refused() {
    let s = Scope::new(Uuid::parse_str(r::BOOT_TENANT).unwrap(), "care", Env::Prod).unwrap();
    // `s:{t}:care:` + `test:bus:op` would be the test scope's `bus:op`
    let _ = Streams::scoped("", &s).key("test:bus:op");
}

#[tokio::test]
async fn agent_api_boots_without_redis() {
    let e = common::env().await;
    assert!(e.state.redis.is_none());
    let (s, _) = common::send(&e.app, common::get("/health", None)).await;
    assert_eq!(s, StatusCode::OK);
    // the registry hooks are no-ops: a job create and a suspend still answer
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, "No Redis Co").await;
    create_job(&e, t, "care_desk").await;
    set_status(&e, t, "suspended").await;
}

async fn create_job(e: &TestEnv, t: Uuid, key: &str) {
    let (s, b) = common::send(
        &e.app,
        common::json(
            "POST",
            &format!("/api/tenants/{t}/jobs"),
            Some(&common::super_admin()),
            json!({"template": "job/customer_care_agent@v4", "key": key, "label": "Care"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

async fn set_status(e: &TestEnv, t: Uuid, status: &str) {
    let (s, b) = common::send(
        &e.app,
        common::json(
            "PATCH",
            &format!("/api/tenants/{t}"),
            Some(&common::super_admin()),
            json!({"status": status}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// The registry members of `t` (text form), from the state's own root bus.
async fn members_of(root: &Bus, t: Uuid) -> Vec<String> {
    scopes::list(root)
        .await
        .unwrap()
        .into_iter()
        .filter(|s| s.tenant == t)
        .map(|s| s.to_string())
        .collect()
}

#[tokio::test]
async fn suspended_tenant_scope_leaves_registry() {
    let vars = r::api_vars("suspend").await;
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let root = e
        .state
        .redis
        .clone()
        .expect("REDIS_URL set: the runtime bus");
    let (t, other) = (Uuid::new_v4(), Uuid::new_v4());
    common::insert_tenant(&e.owner, t, "Susco").await;
    common::insert_tenant(&e.owner, other, "Otherco").await;
    assert!(members_of(&root, t).await.is_empty());

    // a job create registers both envs of the job
    create_job(&e, t, "care_desk").await;
    create_job(&e, other, "care_desk").await;
    let both = vec![format!("{t}/care_desk/prod"), format!("{t}/care_desk/test")];
    assert_eq!(members_of(&root, t).await, both);

    // a suspend removes them (and only them); a resume brings them back
    set_status(&e, t, "suspended").await;
    assert!(members_of(&root, t).await.is_empty());
    assert_eq!(members_of(&root, other).await.len(), 2);
    set_status(&e, t, "active").await;
    assert_eq!(members_of(&root, t).await, both);
}

#[tokio::test]
async fn boot_reconciles_the_registry_from_the_database() {
    let vars = r::api_vars("reconcile").await;
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let root = e.state.redis.clone().unwrap();
    let (t, gone) = (Uuid::new_v4(), Uuid::new_v4());
    common::insert_tenant(&e.owner, t, "Reco").await;
    create_job(&e, t, "care_desk").await;
    // Redis lost the registry, and holds a scope no database row backs
    let key = root.root_key(scopes::SCOPES_KEY);
    let _: i64 = redis::cmd("DEL")
        .arg(&key)
        .query_async(&mut root.conn())
        .await
        .unwrap();
    scopes::add(&root, &Scope::new(gone, "ghost", Env::Prod).unwrap())
        .await
        .unwrap();

    // a second agent-api boot on the same database and prefix rebuilds it exactly
    let mut v = common::base_env(&e.state.cfg.migrate_database_url);
    v.extend(vars.clone());
    let cfg = agent_api::config::Config::from_lookup(|k| v.get(k).cloned()).unwrap();
    let st = agent_api::state::build_for_tests(cfg, common::TEST_SECRET)
        .await
        .unwrap();
    let rebuilt = st.redis.as_ref().unwrap();
    assert_eq!(
        members_of(rebuilt, t).await,
        vec![format!("{t}/care_desk/prod"), format!("{t}/care_desk/test")]
    );
    // add-only: a scope no database row backs survives one pass, and leaves on the second
    assert_eq!(
        members_of(rebuilt, gone).await,
        vec![format!("{gone}/ghost/prod")],
        "one pass without it in the database is not enough"
    );
    scopes::reconcile(&st.db, rebuilt).await.unwrap();
    assert!(
        members_of(rebuilt, gone).await.is_empty(),
        "a scope no database row backs is dropped on the second pass"
    );
}

#[tokio::test]
async fn reconcile_never_drops_a_scope_added_during_the_pass() {
    let vars = r::api_vars("reco-race").await;
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let root = e.state.redis.clone().unwrap();
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, &format!("Reco race {t}")).await;
    // the pass reads the database...
    let snapshot = scopes::all_from_db(&e.state.db).await.unwrap();
    // ...then a job create commits and registers its scopes (crate::runtime::scopes::changed)...
    create_job(&e, t, "late_desk").await;
    let late = vec![format!("{t}/late_desk/prod"), format!("{t}/late_desk/test")];
    assert_eq!(members_of(&root, t).await, late);
    // ...then the pass writes from its stale snapshot: the new scopes stay
    let p = scopes::apply_pass(&root, &snapshot).await.unwrap();
    assert_eq!(p.removed, 0, "{p:?}");
    assert_eq!(
        members_of(&root, t).await,
        late,
        "the late scopes survive the stale pass"
    );
    // the next pass reads them from the database: their miss is forgotten, so a later stale pass cannot drop them
    scopes::reconcile(&e.state.db, &root).await.unwrap();
    scopes::apply_pass(&root, &snapshot).await.unwrap();
    assert_eq!(
        members_of(&root, t).await,
        late,
        "misses are consecutive: a pass that saw them resets the count"
    );
}

#[tokio::test]
async fn a_scope_deleted_in_the_database_leaves_after_the_second_pass() {
    let vars = r::api_vars("reco-delete").await;
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let root = e.state.redis.clone().unwrap();
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, &format!("Reco delete {t}")).await;
    create_job(&e, t, "care_desk").await;
    scopes::reconcile(&e.state.db, &root).await.unwrap();
    let both = vec![format!("{t}/care_desk/prod"), format!("{t}/care_desk/test")];
    assert_eq!(members_of(&root, t).await, both);
    // the database stops naming the tenant's scopes (no `changed` ran)
    sqlx::query("UPDATE tenants SET status = 'suspended' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let first = scopes::apply_pass(&root, &scopes::all_from_db(&e.state.db).await.unwrap())
        .await
        .unwrap();
    assert_eq!(first.removed, 0, "{first:?}");
    assert_eq!(members_of(&root, t).await, both, "kept after one pass");
    let second = scopes::apply_pass(&root, &scopes::all_from_db(&e.state.db).await.unwrap())
        .await
        .unwrap();
    assert!(second.removed >= 2, "{second:?}");
    assert!(
        members_of(&root, t).await.is_empty(),
        "removed after the second pass"
    );
    let counted: i64 = redis::cmd("HLEN")
        .arg(root.root_key(scopes::SCOPES_MISSING_KEY))
        .query_async(&mut root.conn())
        .await
        .unwrap();
    assert_eq!(counted, 0, "no miss counter outlives its scope");
}

#[tokio::test]
async fn reconciler_retries_until_redis_accepts_then_rebuilds() {
    let vars = r::api_vars("reco-retry").await;
    let prefix = vars[1].1.clone();
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let real = e.state.redis.clone().unwrap();
    let t = Uuid::new_v4();
    common::insert_tenant(&e.owner, t, &format!("Reco retry {t}")).await;
    create_job(&e, t, "care_desk").await;
    // the registry is empty, as in prod after a boot whose Redis was not accepting yet
    let _: i64 = redis::cmd("DEL")
        .arg(real.root_key(scopes::SCOPES_KEY))
        .query_async(&mut real.conn())
        .await
        .unwrap();
    // the reconciler's Redis address answers nothing yet
    let addr = r::dead_addr().await;
    let down = Bus::connect_lazy(&r::url_at(addr).await, &prefix).unwrap();
    let task = scopes::spawn_reconciler(e.state.db.clone(), down, Duration::from_secs(3600));
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert!(
        members_of(&real, t).await.is_empty(),
        "nothing can be registered while Redis is down"
    );
    // Redis starts accepting at that address (a proxy to the test server)
    let proxy = r::proxy_at(addr).await;
    let want = vec![format!("{t}/care_desk/prod"), format!("{t}/care_desk/test")];
    let until = std::time::Instant::now() + Duration::from_secs(45);
    let mut got = Vec::new();
    while std::time::Instant::now() < until {
        got = members_of(&real, t).await;
        if got == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    task.abort();
    proxy.abort();
    assert_eq!(got, want, "the reconciler retried until Redis accepted");
}

#[tokio::test]
async fn config_nudges_reach_a_follower_and_never_fail_the_caller() {
    let root = r::root("nudge").await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Scope>();
    let follower = root.clone();
    let task = tokio::spawn(async move {
        config_nudge::follow(&follower, move |s| {
            let _ = tx.send(s);
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let a = r::scope_a_test();
    // no Redis configured, and a job key no scope can carry: both are silent no-ops
    config_nudge::publish(None, a.tenant, &a.job, Env::Test).await;
    config_nudge::publish(Some(&root), a.tenant, "Not A Key", Env::Test).await;
    // the follower subscribes on its own connection: re-send the valid nudge until it is heard
    let mut got = None;
    for _ in 0..15 {
        config_nudge::publish(Some(&root), a.tenant, &a.job, Env::Test).await;
        if let Ok(Some(s)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
            got = Some(s);
            break;
        }
    }
    let got = got.expect("a nudge within 3 s");
    assert_eq!(got, a, "the first nudge received is the valid one");
    assert_eq!(config_nudge::parse(&a.to_string()), Some(a.clone()));
    assert_eq!(config_nudge::parse("x/y"), None);
    task.abort();

    // Redis down: publish returns (logged + counted), it does not fail or hang the design write
    let down = Bus::connect_lazy("redis://127.0.0.1:9", "t:down:").unwrap();
    tokio::time::timeout(
        Duration::from_secs(20),
        config_nudge::publish(Some(&down), a.tenant, &a.job, Env::Prod),
    )
    .await
    .expect("publish returns while Redis is down");
}
