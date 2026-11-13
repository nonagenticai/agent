//! PK T4: cut-time pack resolution (plan PK-4..PK-6). `POST /api/jobs/{job}/releases` first refreshes the tenant's
//! pack caches with no transaction open (`knowledge::packs::resolve::prepare_cut`), then approve step 5b refuses an
//! unusable pin with 422 `compile_failed` (`errors[{path: "knowledge:<stage>:<kind>/<pin>"}]`, one reason each:
//! excluded / unpublished / retired / missing / unavailable) and step 10b snapshots each pin's leaves into
//! `release_pack_leaf`. Real Postgres 18 (one container for the pk binary, a fresh database per test) and the
//! in-process strata fake (T2), whose platform owner is the `AGENT_PLATFORM_TENANT_ID` tenant.
use crate::common;

use crate::common::strata_fake::{self, StrataFake, lock};
use crate::common::{
    TestEnv, insert_tenant, insert_user, json as req, person, send, sqlstate, super_admin,
};
use agent_api::audit::Actor;
use agent_api::design::JobRef;
use agent_api::knowledge::packs::resolve;
use agent_api::platform::db::{rows, tags};
use agent_api::platform::hooks::NoHooks;
use agent_api::platform_db::begin_platform;
use agent_api::tenancy::begin_scoped;
use agent_model::kinds::{Kind, Op};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::time::Duration;
use uuid::Uuid;

/// The stage every test kind lives in.
const STAGE: &str = "pk";

struct W {
    e: TestEnv,
    fake: StrataFake,
    tenant: Uuid,
    job: JobRef,
    /// release_manager (every role)
    tok: String,
    /// the `selection_updated_at` of the saved selection
    seen: Value,
}

/// A complete selection of the seed options (compiles with no violation; tests/release_cut.rs `sel`).
fn sel() -> Value {
    json!({
        "profile": "retail_ecommerce",
        "industry": "retail",
        "departments": ["industry_dept/retail/customer_service"],
        "areas": ["order-support"],
        "dimensions": {"channel": ["chat"]}, "region": "eu",
    })
}

/// A tenant with one job (`POST /api/jobs` from the seed template) and a complete saved selection. `platform`: the
/// strata platform owner is also `AGENT_PLATFORM_TENANT_ID` (the owner listing is available, PK-5).
async fn world(platform: bool) -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let owner = Uuid::new_v4();
    lock(&fake.rag).platform_owner = Some(owner);
    let e = common::env_with(|v| {
        fake.env(&[tenant, owner], v);
        if platform {
            v.insert("AGENT_PLATFORM_TENANT_ID".into(), owner.to_string());
        }
    })
    .await;
    insert_tenant(&e.owner, tenant, &format!("PK cut {tenant}")).await;
    let mut r = req(
        "POST",
        "/api/jobs",
        Some(&super_admin()),
        json!({"template": format!("job/customer_care_agent@v{}", 4)}),
    );
    r.headers_mut()
        .insert("x-active-tenant-id", tenant.to_string().parse().unwrap());
    let (s, b) = send(&e.app, r).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let key = b["key"].as_str().expect("JobSummary.key").to_string();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(tenant)
        .bind(&key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let sub = format!("u-pk-cut-{}", tenant.simple());
    insert_user(&e.owner, &sub, tenant, "tenant_admin").await;
    let tok = person(&sub, &[]);
    let body = json!({"selection": sel(), "expected_updated_at": Value::Null});
    let (s, b) = send(
        &e.app,
        req("PUT", &format!("/api/jobs/{key}/target"), Some(&tok), body),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let seen = b["selection_updated_at"].clone();
    W {
        e,
        fake,
        tenant,
        job: JobRef { id, key },
        tok,
        seen,
    }
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

/// `[(path, message)]` of a 422 `compile_failed`.
fn errors(b: &Value) -> Vec<(String, String)> {
    b["error"]["errors"]
        .as_array()
        .expect("errors")
        .iter()
        .map(|x| {
            (
                x["path"].as_str().unwrap_or("").to_string(),
                x["message"].as_str().unwrap_or("").to_string(),
            )
        })
        .collect()
}

impl W {
    /// A knowledge kind `(STAGE, kind)` pinning `packs`, written as the owner (not through the ledger, so pins the
    /// bind route would refuse can be drafted).
    async fn pin(&self, kind: &str, packs: &[&str]) {
        let packs: Vec<String> = packs.iter().map(|p| p.to_string()).collect();
        sqlx::query(
            "INSERT INTO job_knowledge_kind (tenant_id, job_id, stage, kind, required, description, origin, \
                                             store_path, packs, packs_source, updated_by) \
             VALUES ($1, $2, $3, $4, false, 'pk kind', 'tenant', NULL, $5, 'binding', 'test') \
             ON CONFLICT (tenant_id, job_id, stage, kind) DO UPDATE SET packs = EXCLUDED.packs",
        )
        .bind(self.tenant)
        .bind(self.job.id)
        .bind(STAGE)
        .bind(kind)
        .bind(packs)
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// A published (or draft) pack of the platform owner with `leaves`; the leaf ids.
    fn pack(&self, key: &str, version: i32, leaves: &[&str], published: bool) -> Vec<Uuid> {
        lock(&self.fake.rag)
            .add_owned_pack(key, version, leaves, published)
            .1
    }

    fn retire(&self, key: &str, version: i64) {
        let mut g = lock(&self.fake.rag);
        let p = g
            .packs
            .iter_mut()
            .find(|p| p["key"] == json!(key) && p["version"].as_i64() == Some(version))
            .expect("pack");
        p["retired_at"] = json!("2026-10-21T09:00:00Z");
    }

    fn exclude(&self, prefix: &str) {
        lock(&self.fake.rag).exclusions.push((
            self.tenant,
            json!({"key_prefix": prefix, "reason": "regulation", "created_at": "2026-10-21T09:00:00Z"}),
        ));
    }

    /// strata_rag answers its next listings with a 503.
    fn rag_down(&self) {
        let mut g = lock(&self.fake.rag);
        for _ in 0..3 {
            g.script(
                "GET",
                "/api/platform/packs",
                503,
                json!({"error": {"code": "unavailable", "message": "down"}}),
            );
        }
    }

    async fn approve(&self) -> (StatusCode, Value) {
        send(
            &self.e.app,
            req(
                "POST",
                &format!("/api/jobs/{}/releases", self.job.key),
                Some(&self.tok),
                json!({"apply": false, "selection_seen_at": self.seen}),
            ),
        )
        .await
    }

    /// A cut that must succeed with a new release; its id.
    async fn cut(&self) -> String {
        let (s, b) = self.approve().await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        assert_eq!(b["created"], true, "{b}");
        b["release"]["id"].as_str().expect("release id").to_string()
    }

    /// A cut that must fail 422 `compile_failed`; its errors.
    async fn refused(&self) -> Vec<(String, String)> {
        let (s, b) = self.approve().await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(code(&b), "compile_failed", "{b}");
        errors(&b)
    }

    async fn snapshot_leaves(&self, release: &str, pin: &str) -> BTreeSet<Uuid> {
        let v: Vec<Uuid> = sqlx::query_scalar(
            "SELECT leaf_id FROM release_pack_leaf WHERE tenant_id = $1 AND job_id = $2 AND release_id = $3 AND pin = $4",
        )
        .bind(self.tenant)
        .bind(self.job.id)
        .bind(release)
        .bind(pin)
        .fetch_all(&self.e.owner)
        .await
        .unwrap();
        v.into_iter().collect()
    }

    async fn cached_leaves(&self, key: &str, version: i32) -> BTreeSet<Uuid> {
        let v: Vec<Uuid> = sqlx::query_scalar(
            "SELECT leaf_id FROM platform_pack_leaf WHERE tenant_id = $1 AND pack_key = $2 AND version = $3",
        )
        .bind(self.tenant)
        .bind(key)
        .bind(version)
        .fetch_all(&self.e.owner)
        .await
        .unwrap();
        v.into_iter().collect()
    }

    fn stores_requests(&self) -> usize {
        self.fake
            .rag_requests()
            .iter()
            .filter(|r| r.method == "GET" && r.path.ends_with("/stores"))
            .count()
    }
}

fn set(ids: &[Uuid]) -> BTreeSet<Uuid> {
    ids.iter().copied().collect()
}

// ---------------------------------------------------------------------------------------------------------------
// Happy path and the snapshot

#[tokio::test]
async fn happy_cut_snapshots_the_pack_subtree_leaves() {
    let w = world(true).await;
    let leaves = w.pack("industry/retail", 1, &["faq", "returns"], true);
    w.pin("faq", &["industry/retail@1"]).await;
    let id = w.cut().await;
    assert_eq!(
        w.snapshot_leaves(&id, "industry/retail@1").await,
        set(&leaves)
    );
    assert_eq!(w.cached_leaves("industry/retail", 1).await, set(&leaves));
    let (state, excluded): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT state, excluded_by FROM platform_pack WHERE tenant_id = $1 AND pack_key = 'industry/retail' AND version = 1",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(state.as_deref(), Some("published"));
    assert_eq!(excluded, None);
    // every snapshot row carries the pack's embed model
    let models: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM release_pack_leaf WHERE release_id = $1 AND embed_model IS NULL",
    )
    .bind(&id)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(models, 0);
}

#[tokio::test]
async fn a_cached_pin_is_not_fetched_again() {
    let w = world(true).await;
    w.pack("base", 1, &["a"], true);
    w.pin("faq", &["base@1"]).await;
    w.cut().await;
    assert_eq!(w.stores_requests(), 1);
    w.pack("job/clerk", 1, &["b"], true);
    w.pin("policy", &["job/clerk@1"]).await;
    w.cut().await;
    // only the new pin went to `/stores`: published leaves never change
    assert_eq!(w.stores_requests(), 2);
}

#[tokio::test]
async fn a_job_without_pins_makes_no_platform_call() {
    let w = world(true).await;
    w.cut().await;
    let platform = w
        .fake
        .rag_requests()
        .iter()
        .filter(|r| r.path.starts_with("/api/platform"))
        .count();
    assert_eq!(platform, 0);
}

#[tokio::test]
async fn release_pack_leaf_rows_cannot_be_updated() {
    let w = world(true).await;
    w.pack("base", 1, &["a"], true);
    w.pin("faq", &["base@1"]).await;
    let id = w.cut().await;
    // the owner: the trigger
    let err = sqlx::query("UPDATE release_pack_leaf SET embed_model = 'x@1' WHERE release_id = $1")
        .bind(&id)
        .execute(&w.e.owner)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
    let err = sqlx::query("DELETE FROM release_pack_leaf WHERE release_id = $1")
        .bind(&id)
        .execute(&w.e.owner)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
    // the runtime role: no UPDATE grant at all
    let mut tx = begin_scoped(&w.e.state.db, w.tenant).await.unwrap();
    let err = sqlx::query("UPDATE release_pack_leaf SET embed_model = 'x@1' WHERE release_id = $1")
        .bind(&id)
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
}

// ---------------------------------------------------------------------------------------------------------------
// One 422 per reason, with the exact path

#[tokio::test]
async fn excluded_pack_is_refused() {
    let w = world(true).await;
    w.pack("industry/retail/hr", 1, &["a"], true);
    w.exclude("industry/retail");
    w.pin("faq", &["industry/retail/hr@1"]).await;
    let e = w.refused().await;
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].0, "knowledge:pk:faq/industry/retail/hr@1");
    assert!(
        e[0].1.contains("industry/retail/hr@1")
            && e[0].1.contains("excluded")
            && e[0].1.contains("industry/retail"),
        "{}",
        e[0].1
    );
}

#[tokio::test]
async fn draft_pack_is_refused_unpublished() {
    let w = world(true).await;
    w.pack("job/clerk", 2, &["a"], false);
    w.pin("policy", &["job/clerk@2"]).await;
    let e = w.refused().await;
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].0, "knowledge:pk:policy/job/clerk@2");
    assert!(
        e[0].1.contains("job/clerk@2") && e[0].1.contains("not published"),
        "{}",
        e[0].1
    );
}

#[tokio::test]
async fn new_pin_to_a_retired_pack_is_refused() {
    let w = world(true).await;
    w.pack("base", 1, &["a"], true);
    w.retire("base", 1);
    w.pin("faq", &["base@1"]).await;
    let e = w.refused().await;
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].0, "knowledge:pk:faq/base@1");
    assert!(
        e[0].1.contains("base@1") && e[0].1.contains("retired"),
        "{}",
        e[0].1
    );
}

#[tokio::test]
async fn unknown_pack_is_refused_missing() {
    let w = world(true).await;
    w.pin("faq", &["industry/ghost@3"]).await;
    let e = w.refused().await;
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].0, "knowledge:pk:faq/industry/ghost@3");
    assert!(
        e[0].1.contains("industry/ghost@3") && e[0].1.contains("does not exist"),
        "{}",
        e[0].1
    );
}

/// PK-5: without the platform plane a pack the tenant cannot read is not classified: `unavailable`.
#[tokio::test]
async fn without_the_owner_listing_an_unreadable_pack_is_unavailable() {
    let w = world(false).await;
    w.pack("job/clerk", 2, &["a"], false);
    w.pin("policy", &["job/clerk@2"]).await;
    let e = w.refused().await;
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].0, "knowledge:pk:policy/job/clerk@2");
    assert!(
        e[0].1.contains("job/clerk@2") && e[0].1.contains("could not be resolved"),
        "{}",
        e[0].1
    );
    // a published pack still cuts without the owner listing
    let w = world(false).await;
    let leaves = w.pack("base", 1, &["a"], true);
    w.pin("faq", &["base@1"]).await;
    let id = w.cut().await;
    assert_eq!(w.snapshot_leaves(&id, "base@1").await, set(&leaves));
}

/// A failed owner listing keeps the cached owner state: a draft or retired pin is still refused for its real reason,
/// not collapsed to `unavailable` (the audit finding on `write_cache`).
#[tokio::test]
async fn a_failed_owner_listing_keeps_the_cached_reasons() {
    let w = world(true).await;
    w.pack("job/clerk", 2, &["a"], false);
    w.pack("base", 1, &["b"], true);
    w.retire("base", 1);
    w.pin("policy", &["job/clerk@2"]).await;
    w.pin("faq", &["base@1"]).await;
    let reasons = |e: &[(String, String)]| {
        let mut v: Vec<(String, bool, bool)> = e
            .iter()
            .map(|(p, m)| {
                (
                    p.clone(),
                    m.contains("not published"),
                    m.contains("retired"),
                )
            })
            .collect();
        v.sort();
        v
    };
    let want = vec![
        ("knowledge:pk:faq/base@1".to_string(), false, true),
        ("knowledge:pk:policy/job/clerk@2".to_string(), true, false),
    ];
    assert_eq!(reasons(&w.refused().await), want);
    // the owner's listing now fails; the tenant's own calls still answer
    let owner = lock(&w.fake.rag).platform_owner.expect("owner");
    lock(&w.fake.rag).script_for(
        owner,
        "GET",
        "/api/platform/packs",
        503,
        json!({"error": {"code": "unavailable", "message": "down"}}),
    );
    let e = w.refused().await;
    assert!(
        lock(&w.fake.rag).scripted.is_empty(),
        "the owner listing was called and failed"
    );
    assert_eq!(reasons(&e), want, "{e:?}");
    let states: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT pack_key, state FROM platform_pack WHERE tenant_id = $1 ORDER BY pack_key",
    )
    .bind(w.tenant)
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(
        states,
        vec![
            ("base".to_string(), Some("retired".to_string())),
            ("job/clerk".to_string(), Some("draft".to_string())),
        ]
    );
}

#[tokio::test]
async fn every_unusable_pin_is_named_once() {
    let w = world(true).await;
    let ok = w.pack("base", 1, &["a"], true);
    w.pack("job/clerk", 1, &["b"], false);
    w.pin("faq", &["base@1", "job/clerk@1"]).await;
    w.pin("policy", &["industry/ghost@1"]).await;
    let e = w.refused().await;
    let paths: Vec<&str> = e.iter().map(|(p, _)| p.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "knowledge:pk:faq/job/clerk@1",
            "knowledge:pk:policy/industry/ghost@1"
        ]
    );
    // nothing was cut
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM job_release WHERE job_id = $1")
        .bind(w.job.id)
        .fetch_one(&w.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(w.cached_leaves("base", 1).await, set(&ok));
}

// ---------------------------------------------------------------------------------------------------------------
// PK-6: earlier pins

#[tokio::test]
async fn retired_pack_pinned_by_the_last_release_cuts_again_from_its_rows() {
    let w = world(true).await;
    let leaves = w.pack("base", 1, &["a", "b"], true);
    w.pin("faq", &["base@1"]).await;
    let first = w.cut().await;
    w.retire("base", 1);
    // a draft change elsewhere so the second cut is a new release
    w.pack("job/clerk", 1, &["c"], true);
    w.pin("policy", &["job/clerk@1"]).await;
    let second = w.cut().await;
    assert_ne!(first, second);
    assert_eq!(w.snapshot_leaves(&second, "base@1").await, set(&leaves));
    let state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM platform_pack WHERE tenant_id = $1 AND pack_key = 'base' AND version = 1",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(state.as_deref(), Some("retired"));
}

#[tokio::test]
async fn snapshot_falls_back_to_the_last_release_rows() {
    let w = world(true).await;
    let leaves = w.pack("base", 1, &["a"], true);
    w.pin("faq", &["base@1"]).await;
    w.cut().await;
    // the leaf cache is gone and strata no longer answers `/stores` for the pin: the new release copies the last
    // release's rows (a pin the newest release holds is kept, PK-6)
    sqlx::query("DELETE FROM platform_pack_leaf WHERE tenant_id = $1")
        .bind(w.tenant)
        .execute(&w.e.owner)
        .await
        .unwrap();
    lock(&w.fake.rag)
        .packs
        .retain(|p| p["key"] != json!("base"));
    w.pack("job/clerk", 1, &["c"], true);
    w.pin("policy", &["job/clerk@1"]).await;
    let second = w.cut().await;
    assert_eq!(w.snapshot_leaves(&second, "base@1").await, set(&leaves));
}

#[tokio::test]
async fn excluded_pack_is_refused_even_when_previously_pinned() {
    let w = world(true).await;
    w.pack("industry/retail", 1, &["a"], true);
    w.pin("faq", &["industry/retail@1"]).await;
    w.cut().await;
    w.exclude("industry");
    w.pack("job/clerk", 1, &["c"], true);
    w.pin("policy", &["job/clerk@1"]).await;
    let e = w.refused().await;
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].0, "knowledge:pk:faq/industry/retail@1");
    assert!(e[0].1.contains("excluded"), "{}", e[0].1);
}

// ---------------------------------------------------------------------------------------------------------------
// strata down

#[tokio::test]
async fn strata_down_with_cached_leaves_still_cuts() {
    let w = world(true).await;
    let leaves = w.pack("base", 1, &["a"], true);
    w.pin("faq", &["base@1"]).await;
    w.cut().await;
    // a second kind on the same (cached) pin: a new release
    w.pin("policy", &["base@1"]).await;
    w.rag_down();
    let second = w.cut().await;
    assert_eq!(w.snapshot_leaves(&second, "base@1").await, set(&leaves));
}

#[tokio::test]
async fn strata_down_without_cache_is_503() {
    let w = world(true).await;
    w.pack("base", 1, &["a"], true);
    w.pin("faq", &["base@1"]).await;
    w.rag_down();
    let (s, b) = w.approve().await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{b}");
    assert_eq!(code(&b), "unavailable", "{b}");
}

// ---------------------------------------------------------------------------------------------------------------
// PK-4: no transaction across strata

/// Backends of the app role on this database sitting idle inside a transaction opened more than 250ms ago (a
/// transaction held across the parked strata call is older; a quick background one, e.g. an approved hook, is not).
async fn idle_in_tx(w: &W) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity \
          WHERE datname = current_database() AND usename = 'agent_app' \
            AND state IN ('idle in transaction', 'idle in transaction (aborted)') \
            AND xact_start < clock_timestamp() - interval '250 milliseconds'",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap()
}

/// The fake holds each platform call in turn (the tenant listing, then the exclusions, then `/stores`); while it is
/// held no `agent_app` backend of this database is idle in a transaction.
#[tokio::test]
async fn no_transaction_is_open_during_the_strata_calls() {
    let w = world(true).await;
    w.pack("base", 1, &["a"], true);
    w.pin("faq", &["base@1"]).await;
    let held = [
        "/api/platform/packs",
        "/api/platform/exclusions",
        "/api/platform/packs/base/1/stores",
    ];
    for (i, path) in held.iter().enumerate() {
        // a fresh pin each round, so `/stores` is called every time
        if i > 0 {
            let version = i32::try_from(i).unwrap();
            w.pack("job/clerk", version, &["b"], true);
            let pin = format!("job/clerk@{version}");
            w.pin("policy", &[pin.as_str()]).await;
            sqlx::query("DELETE FROM platform_pack_leaf WHERE tenant_id = $1")
                .bind(w.tenant)
                .execute(&w.e.owner)
                .await
                .unwrap();
        }
        let pause = lock(&w.fake.rag).pause_at("GET", path);
        let app = w.e.app.clone();
        let r = req(
            "POST",
            &format!("/api/jobs/{}/releases", w.job.key),
            Some(&w.tok),
            json!({"apply": false, "selection_seen_at": w.seen}),
        );
        let call = tokio::spawn(async move { send(&app, r).await });
        tokio::time::timeout(Duration::from_secs(20), pause.reached.notified())
            .await
            .unwrap_or_else(|_| panic!("{path} was never called"));
        // the request is parked inside the strata call; give a leaked transaction time to show
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(
            idle_in_tx(&w).await,
            0,
            "a transaction is open across {path}"
        );
        pause.release.notify_one();
        let (s, b) = call.await.unwrap();
        assert_eq!(s, StatusCode::CREATED, "{path}: {b}");
    }
}

// ---------------------------------------------------------------------------------------------------------------
// adopt_pins

/// With `adopt`, the `knowledge_pack` pins of the adopted families' latest tags are prepared too (a superset of
/// what the adopt re-pins); without it they are not.
#[tokio::test]
async fn prepare_cut_with_adopt_prepares_the_latest_tag_pins() {
    let w = world(true).await;
    let leaves = w.pack("industry/adopted", 1, &["a", "b"], true);
    let n = Uuid::new_v4().simple().to_string();
    let family = format!("industry/pk_adopt_{n}");
    sqlx::query("INSERT INTO layer (key, scope, industry, family) VALUES ($1, 'industry', $2, $1)")
        .bind(&family)
        .bind(format!("pk_adopt_{n}"))
        .execute(&w.e.owner)
        .await
        .unwrap();
    let actor = Actor::system("service:agent-api-pk-test");
    let mut tx = begin_platform(&w.e.state.db).await.unwrap();
    // D107 (KH T3): a pack reference names a declared kind
    rows::write_revision(
        &mut tx,
        &actor,
        &family,
        Kind::StageKnowledgeKind,
        "support|faq",
        Op::Add,
        json!({"label": "faq"}),
        "pk-author",
    )
    .await
    .expect("declare the kind");
    for (key, op, fields) in [
        (
            "support|faq|industry/adopted",
            Op::Add,
            json!({"version": 1}),
        ),
        (
            "support|faq|industry/dropped",
            Op::Add,
            json!({"version": 4}),
        ),
    ] {
        rows::write_revision(
            &mut tx,
            &actor,
            &family,
            Kind::KnowledgePack,
            key,
            op,
            fields,
            "pk-author",
        )
        .await
        .expect("write the pack row");
    }
    rows::write_revision(
        &mut tx,
        &actor,
        &family,
        Kind::KnowledgePack,
        "support|faq|industry/dropped",
        Op::Tombstone,
        json!({}),
        "pk-author",
    )
    .await
    .expect("tombstone");
    tags::tag_family(
        &mut tx, &actor, &family, "pk tag", "pk-owner", &NoHooks, false,
    )
    .await
    .expect("tag the family");
    tx.commit().await.unwrap();

    resolve::prepare_cut(&w.e.state, w.tenant, &w.job, None)
        .await
        .expect("no pins");
    assert!(w.cached_leaves("industry/adopted", 1).await.is_empty());
    assert_eq!(w.stores_requests(), 0);

    let adopt = vec![family.clone()];
    resolve::prepare_cut(&w.e.state, w.tenant, &w.job, Some(adopt.as_slice()))
        .await
        .expect("adopted pins prepared");
    assert_eq!(w.cached_leaves("industry/adopted", 1).await, set(&leaves));
    // the tombstoned row is not a pin
    let dropped: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM platform_pack WHERE tenant_id = $1 AND pack_key = 'industry/dropped'",
    )
    .bind(w.tenant)
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(dropped, 0);
}
