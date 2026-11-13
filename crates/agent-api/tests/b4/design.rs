//! B4 T4 — how the runtime reads a job's design (crate::runtime::design): Live (the current release's snapshot, or
//! the baseline of a job never released) vs Draft (the working rows at `draft_rev`), the per-scope cache a nudge
//! drops, and the nudges themselves — queued by the design writers inside their transaction and relayed to Redis only
//! after it commits. Real Postgres 18 + Redis, the real router, a job created from `job/customer_care_agent@v4`.
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_api::design::{self, CaseDesign, DesignOf, DraftRef, DraftWrite, Label, Place};
use agent_api::runtime::config_nudge::{self, CONFIG_CHANNEL};
use agent_api::runtime::design::{DesignCache, RuntimeDesign, start_nudge_relay};
use agent_api::runtime::{Bus, Env, Scope};
use axum::http::StatusCode;
use futures_util::{Stream, StreamExt};
use serde_json::{Value, json};

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv, insert_user, json as req, person, send};
use crate::redis_env as r;

/// A job of its own tenant, a release manager's token, and (with Redis) the state's root bus.
pub struct Fx {
    pub e: TestEnv,
    pub f: ContractJob,
    pub tok: String,
}

/// A fresh environment; `redis` = a test name: `REDIS_URL` + a fresh prefix are set (the state gets its root bus).
pub async fn setup(redis: Option<&str>) -> Fx {
    setup_on(redis, false).await
}

/// [`setup`] with the job on the B4 test template (`latest`, `support::template`): activities with their operations,
/// the retail spaces' `activity` (R2) and the procedures — a designed procedure's steps are checked against the grant
/// of its space's activity. Without it the job is on the bare offering.
pub async fn setup_on(redis: Option<&str>, latest: bool) -> Fx {
    let e = match redis {
        Some(name) => {
            let vars = r::api_vars(name).await;
            common::env_with(|v| v.extend(vars.clone())).await
        }
        None => common::env().await,
    };
    // `latest`: the offering plus the content the B4 runtime tests author (`support::template`); else the bare offering
    let f = if latest {
        let f = crate::b4::support::new_job(&e).await;
        // the routing publish gate (R5): every operation the activities name is routed on Production
        crate::b4::support::route_prod(&e, &f).await;
        f
    } else {
        design_contract::new_job(&e).await
    };
    let sub = format!("u-b4-design-{}", f.tenant.simple());
    insert_user(&e.owner, &sub, f.tenant, "tenant_admin").await;
    let tok = person(&sub, &[]);
    Fx { e, f, tok }
}

fn uri(fx: &Fx, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", fx.f.job.job_key)
}

/// Save a complete selection (the first save); returns `selection_updated_at` (an approve's `selection_seen_at`).
pub async fn put_target(fx: &Fx) -> Value {
    let body =
        json!({"selection": design_contract::cuttable_selection(), "expected_updated_at": null});
    let (s, b) = send(
        &fx.e.app,
        req("PUT", &uri(fx, "target"), Some(&fx.tok), body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["selection_updated_at"].clone()
}

pub async fn put_languages(fx: &Fx, langs: Value) {
    let body = json!({"languages": langs});
    let (s, b) = send(
        &fx.e.app,
        req("PUT", &uri(fx, "languages"), Some(&fx.tok), body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Approve (cut) a new release; `apply` = approve and apply in one call. Returns the release id.
pub async fn approve(fx: &Fx, at: &Value, apply: bool) -> String {
    let body = json!({"apply": apply, "selection_seen_at": at});
    let (s, b) = send(
        &fx.e.app,
        req("POST", &uri(fx, "releases"), Some(&fx.tok), body),
    )
    .await;
    assert!(s == StatusCode::CREATED || s == StatusCode::OK, "{s}: {b}");
    assert_eq!(b["created"], true, "{b}");
    assert_eq!(b["applied"], apply, "{b}");
    b["release"]["id"].as_str().expect("release id").to_string()
}

/// Apply release `id`; returns the direction (`apply` | `rollback` | `reapply`).
pub async fn apply(fx: &Fx, id: &str) -> String {
    let path = uri(fx, &format!("releases/{id}/apply"));
    let (s, b) = send(&fx.e.app, req("POST", &path, Some(&fx.tok), json!({}))).await;
    assert_eq!(s, StatusCode::OK, "apply {id}: {b}");
    assert_eq!(b["applied"], true, "{b}");
    b["direction"].as_str().unwrap_or("").to_string()
}

pub async fn draft_rev(fx: &Fx) -> i64 {
    sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(fx.f.job.job_id)
        .fetch_one(&fx.e.owner)
        .await
        .unwrap()
}

/// `RuntimeDesign::load` in a job-scoped transaction (as `agent_app`, through RLS).
pub async fn load(fx: &Fx, of: DesignOf) -> RuntimeDesign {
    let mut tx = fx.f.job.begin(&fx.e.state.db).await.unwrap();
    let d = RuntimeDesign::load(&mut tx, &fx.e.state.design, &fx.f.job_ref(), &of)
        .await
        .unwrap_or_else(|e| panic!("load {of:?}: {e:?}"));
    tx.commit().await.unwrap();
    d
}

fn langs(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| (*s).to_string()).collect()
}

/// R1 = [en, de] approved and applied, then [en, de, fr] saved to the draft. Returns R1.
async fn live_r1_draft_fr(fx: &Fx) -> String {
    let at = put_target(fx).await;
    put_languages(fx, json!(["en", "de"])).await;
    let r1 = approve(fx, &at, true).await;
    put_languages(fx, json!(["en", "de", "fr"])).await;
    r1
}

#[tokio::test]
async fn live_reads_the_active_snapshot_not_the_draft() {
    let fx = setup_on(None, true).await;
    let r1 = live_r1_draft_fr(&fx).await;
    let live = load(&fx, DesignOf::Live).await;
    assert_eq!(
        live.languages,
        langs(&["en", "de"]),
        "the release, not the draft"
    );
    assert_eq!(
        live.case,
        CaseDesign::Live {
            release_id: Some(r1)
        }
    );
    // the template content compiled from the job's pins: spaces, procedures, the catalogue
    assert!(
        live.template_errors.is_empty(),
        "{:?}",
        live.template_errors
    );
    assert!(
        live.spaces.by_key("retail:order-support:returns").is_some(),
        "{:?}",
        live.spaces
    );
    assert!(live.procedures.iter().any(|p| p.name == "return_request"));
    assert_eq!(live.selection.industry, "retail");
}

#[tokio::test]
async fn draft_reads_working_rows_at_draft_rev() {
    let fx = setup(None).await;
    live_r1_draft_fr(&fx).await;
    let (s, b) = send(
        &fx.e.app,
        req(
            "PUT",
            &uri(&fx, "settings"),
            Some(&fx.tok),
            json!({"stance": "Hello from us."}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let rev = draft_rev(&fx).await;
    let draft = load(&fx, DesignOf::Draft).await;
    assert_eq!(draft.languages, langs(&["en", "de", "fr"]));
    assert_eq!(draft.case, CaseDesign::Draft { draft_rev: rev });
    assert_eq!(draft.settings.get("stance"), Some(&json!("Hello from us.")));
    // the same job's Live does not see either draft change
    let live = load(&fx, DesignOf::Live).await;
    assert_eq!(live.languages, langs(&["en", "de"]));
    assert_ne!(live.settings.get("stance"), Some(&json!("Hello from us.")));
    // a release by id is not a runtime read
    let mut tx = fx.f.job.begin(&fx.e.state.db).await.unwrap();
    let other = DesignOf::Release("r-1".into());
    let refused = RuntimeDesign::load(&mut tx, &fx.e.state.design, &fx.f.job_ref(), &other).await;
    assert!(refused.is_err());
}

#[tokio::test]
async fn runtime_settings_carry_the_template_bases() {
    // the ledger's open item (T4): the runtime used to see the job's overrides only, never the base values in force
    let fx = setup(None).await;
    let at = put_target(&fx).await;
    let (s, view) = send(&fx.e.app, common::get(&uri(&fx, "settings"), Some(&fx.tok))).await;
    assert_eq!(s, StatusCode::OK, "{view}");
    let effective: BTreeMap<String, Value> = view["effective"]
        .as_object()
        .expect("JobSettingsView.effective")
        .iter()
        .filter(|(_, v)| v.is_string())
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    assert!(!effective.is_empty(), "the template gives bases: {view}");
    // no override: the draft's settings are the bases (what the settings view shows in force), plus the preamble
    // base — the job's folded identity instruction, which no tenant overrides (KH T4, D108)
    let d0 = load(&fx, DesignOf::Draft).await.settings;
    let preamble = d0
        .get("preamble")
        .filter(|v| v.is_string())
        .cloned()
        .expect("the seed's identity is the preamble base");
    let mut bases = effective.clone();
    bases.insert("preamble".to_string(), preamble);
    assert_eq!(d0, bases);
    // an override replaces its base; an override of null hides it
    let hidden = effective.keys().find(|k| k.as_str() != "actor").cloned();
    let mut body = json!({"actor": "Hello from us."});
    if let Some(h) = &hidden {
        body[h.as_str()] = Value::Null;
    }
    let (s, b) = send(
        &fx.e.app,
        req("PUT", &uri(&fx, "settings"), Some(&fx.tok), body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let mut want = bases.clone();
    want.insert("actor".to_string(), json!("Hello from us."));
    if let Some(h) = &hidden {
        want.remove(h);
    }
    assert_eq!(load(&fx, DesignOf::Draft).await.settings, want);
    // released: the release bundle's bases under the release's overrides
    let r1 = approve(&fx, &at, true).await;
    let live = load(&fx, DesignOf::Live).await;
    assert_eq!(
        live.case,
        CaseDesign::Live {
            release_id: Some(r1.clone())
        }
    );
    assert_eq!(live.settings, want);
    let frozen: Value = sqlx::query_scalar(
        "SELECT bundle -> 'settings' FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(fx.f.job.job_id)
    .bind(&r1)
    .fetch_one(&fx.e.owner)
    .await
    .unwrap();
    for (k, v) in want.iter().filter(|(k, _)| k.as_str() != "actor") {
        assert_eq!(frozen.get(k), Some(v), "{k} is the release's base");
    }
}

#[tokio::test]
async fn rollback_switches_live_source() {
    let fx = setup(None).await;
    let r1 = live_r1_draft_fr(&fx).await;
    let at = put_target_seen(&fx).await;
    let r2 = approve(&fx, &at, false).await;
    assert_eq!(apply(&fx, &r2).await, "apply");
    let live = load(&fx, DesignOf::Live).await;
    assert_eq!(live.languages, langs(&["en", "de", "fr"]));
    assert_eq!(
        live.case,
        CaseDesign::Live {
            release_id: Some(r2)
        }
    );

    assert_eq!(apply(&fx, &r1).await, "rollback");
    let live = load(&fx, DesignOf::Live).await;
    assert_eq!(
        live.languages,
        langs(&["en", "de"]),
        "live follows the rollback"
    );
    assert_eq!(
        live.case,
        CaseDesign::Live {
            release_id: Some(r1)
        }
    );
}

/// The current `selection_updated_at` (`GET …/target`).
pub async fn put_target_seen(fx: &Fx) -> Value {
    let (s, b) = send(&fx.e.app, common::get(&uri(fx, "target"), Some(&fx.tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["selection_updated_at"].clone()
}

#[tokio::test]
async fn never_released_job_reads_baseline() {
    let fx = setup(None).await;
    let before = load(&fx, DesignOf::Live).await;
    assert_eq!(before.case, CaseDesign::Live { release_id: None });
    let draft_langs = if before.languages == langs(&["nl"]) {
        json!(["fr"])
    } else {
        json!(["nl"])
    };
    put_languages(&fx, draft_langs.clone()).await;
    let live = load(&fx, DesignOf::Live).await;
    assert_eq!(live.case, CaseDesign::Live { release_id: None });
    assert_eq!(live.languages, before.languages, "Live = the baseline");
    let draft = load(&fx, DesignOf::Draft).await;
    assert_eq!(
        serde_json::to_value(&draft.languages).unwrap(),
        draft_langs,
        "the draft has the save"
    );
    assert_ne!(live.languages, draft.languages);
}

// ---- nudges -------------------------------------------------------------------------------------------------------

/// The nudge channel's messages.
type Nudges = Pin<Box<dyn Stream<Item = redis::Msg>>>;

/// A Pub/Sub subscription to the root's nudge channel, made before it returns.
async fn subscribe(root: &Bus) -> Nudges {
    let mut ps = root.client().get_async_pubsub().await.unwrap();
    ps.subscribe(root.root_key(CONFIG_CHANNEL)).await.unwrap();
    Box::pin(ps.into_on_message())
}

/// The next nudge payload within `wait`, if any.
async fn next_nudge(msgs: &mut Nudges, wait: Duration) -> Option<String> {
    match tokio::time::timeout(wait, msgs.next()).await {
        Ok(Some(m)) => Some(m.get_payload::<String>().unwrap()),
        _ => None,
    }
}

fn scope_text(fx: &Fx, env: Env) -> String {
    Scope::new(fx.f.tenant, fx.f.job.job_key.clone(), env)
        .unwrap()
        .to_string()
}

/// One tracked `languages` write in the job's transaction, committed or rolled back.
async fn track_languages(fx: &Fx, value: Value, commit: bool) {
    let reg = fx.e.state.design.clone();
    let (tenant, jr) = (fx.f.tenant, fx.f.job_ref());
    let w = DraftWrite {
        area: "settings",
        page: "J14",
        kind: "languages",
        r#ref: DraftRef::new(),
        label: Label::Now("languages".into()),
    };
    let mut tx = fx.f.job.begin(&fx.e.state.db).await.unwrap();
    let out = design::track::<(), _>(&mut tx, &fx.e.state.design, &fx.f.job, w, move |c| {
        Box::pin(async move {
            let at = Place {
                tenant,
                job: Some(&jr),
            };
            reg.target("languages")?
                .set(c, at, &DraftRef::new(), &Some(value))
                .await
        })
    })
    .await
    .unwrap();
    assert!(out.change.is_some(), "the write changed the draft");
    if commit {
        tx.commit().await.unwrap();
    } else {
        tx.rollback().await.unwrap();
    }
}

#[tokio::test]
async fn track_publishes_test_nudge() {
    let fx = setup(Some("design-track-nudge")).await;
    let root = fx.e.state.redis.clone().expect("the state's root bus");
    let relay = start_nudge_relay(fx.e.state.db.clone(), root.clone())
        .await
        .unwrap();
    let mut msgs = subscribe(&root).await;

    // a write that rolls back is never announced (mutation: a nudge sent before commit arrives here)
    track_languages(&fx, json!(["nl", "fr"]), false).await;
    assert_eq!(
        next_nudge(&mut msgs, Duration::from_millis(800)).await,
        None,
        "a rolled-back write sends no nudge"
    );
    // the committed write is: the job's test scope, never prod
    track_languages(&fx, json!(["nl", "fr"]), true).await;
    let got = next_nudge(&mut msgs, Duration::from_secs(5)).await;
    assert_eq!(got, Some(scope_text(&fx, Env::Test)));
    assert_eq!(
        next_nudge(&mut msgs, Duration::from_millis(300)).await,
        None
    );
    relay.abort();
}

#[tokio::test]
async fn apply_publishes_prod_nudge_after_commit() {
    let fx = setup(Some("design-apply-nudge")).await;
    let root = fx.e.state.redis.clone().expect("the state's root bus");
    let relay = start_nudge_relay(fx.e.state.db.clone(), root.clone())
        .await
        .unwrap();
    let at = put_target(&fx).await;
    put_languages(&fx, json!(["en", "de"])).await;
    let r1 = approve(&fx, &at, false).await;
    // the writes above announced their test nudges; start listening after them
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut msgs = subscribe(&root).await;

    // an apply whose transaction rolls back announces nothing
    let mut tx = fx.f.job.begin(&fx.e.state.db).await.unwrap();
    let done = agent_api::release::apply::apply(&mut tx, &fx.e.state, &fx.f.job, &r1)
        .await
        .unwrap();
    assert!(done.applied);
    tx.rollback().await.unwrap();
    let prod = scope_text(&fx, Env::Prod);
    let mut early = Vec::new();
    let quiet = Instant::now() + Duration::from_millis(800);
    while Instant::now() < quiet {
        if let Some(p) = next_nudge(&mut msgs, Duration::from_millis(100)).await {
            early.push(p);
        }
    }
    assert!(
        !early.contains(&prod),
        "a rolled-back apply sends no nudge: {early:?}"
    );

    // the committed apply announces the job's prod scope
    assert_eq!(apply(&fx, &r1).await, "apply");
    let mut seen = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && !seen.contains(&prod) {
        if let Some(p) = next_nudge(&mut msgs, Duration::from_millis(200)).await {
            seen.push(p);
        }
    }
    assert!(seen.contains(&prod), "{seen:?}");
    relay.abort();
}

/// The scope's design through the cache (loaded in a job-scoped transaction on a miss).
async fn cached_get(fx: &Fx, cache: &DesignCache, s: &Scope) -> Arc<RuntimeDesign> {
    let mut tx = fx.f.job.begin(&fx.e.state.db).await.unwrap();
    let d = cache
        .get_or_load(&mut tx, &fx.e.state.design, s, &fx.f.job_ref())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    d
}

#[tokio::test]
async fn nudge_drops_cache_under_one_second() {
    let fx = setup(Some("design-cache-nudge")).await;
    let root = fx.e.state.redis.clone().expect("the state's root bus");
    let relay = start_nudge_relay(fx.e.state.db.clone(), root.clone())
        .await
        .unwrap();
    let cache = Arc::new(DesignCache::default());
    let follower = cache.follow(root.clone());
    let s = Scope::new(fx.f.tenant, fx.f.job.job_key.clone(), Env::Test).unwrap();

    // wait until the follower is subscribed: a nudge for the scope drops its cached design
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        cached_get(&fx, &cache, &s).await;
        config_nudge::publish(Some(&root), s.tenant, &s.job, Env::Test).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        if cache.cached(&s).is_none() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the cache follower never subscribed"
        );
    }

    let d0 = cached_get(&fx, &cache, &s).await;
    assert!(cache.cached(&s).is_some(), "a load is cached");
    // a prod nudge of the same job is another scope: the test design stays
    config_nudge::publish(Some(&root), s.tenant, &s.job, Env::Prod).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(cache.cached(&s).is_some(), "another scope's nudge keeps it");

    put_languages(&fx, json!(["fr", "de", "nl"])).await;
    let saved = Instant::now();
    while cache.cached(&s).is_some() {
        assert!(
            saved.elapsed() < Duration::from_secs(1),
            "the committed write's nudge dropped the cached design within 1 s"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let d1 = cached_get(&fx, &cache, &s).await;
    assert_eq!(d1.languages, langs(&["fr", "de", "nl"]));
    let CaseDesign::Draft { draft_rev: r0 } = d0.case.clone() else {
        panic!("{:?}", d0.case)
    };
    assert_eq!(d1.case, CaseDesign::Draft { draft_rev: r0 + 1 });
    follower.abort();
    relay.abort();
}
