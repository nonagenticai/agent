//! B3 T10: per-language knowledge coverage (D68, plan §5.4 `knowledgeCoverage`, §0 R9/R10): the route against the
//! TS reference case (`web/src/lib/tenant/coverage-D68.ts` + its vitest case), the job's draft languages, 404, R10
//! pack-only cells (`platform`, never a gap, level `partial`), and B2's `language_level` hook (local state:
//! uploaded files). Real Postgres 18 (one container for this binary, a fresh database per test) and the strata fakes.
use crate::common;

use crate::common::strata_fake::{self, StrataFake, lock};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send, super_admin};
use agent_api::design::JobRef;
use agent_api::design::coverage::{Level, language_level};
use agent_api::tenancy::begin_scoped;
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

struct W {
    e: TestEnv,
    fake: StrataFake,
    tenant: Uuid,
    job: JobRef,
    obs: String,
}

/// A tenant with one job from the seed template whose template kinds are removed (each test inserts its own).
async fn world() -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| fake.env(&[tenant], v)).await;
    insert_tenant(&e.owner, tenant, &format!("T10 coverage {tenant}")).await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenants/{tenant}/jobs"),
            Some(&super_admin()),
            json!({"template": "job/customer_care_agent@v4", "key": "care_cov", "label": "Care"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id: Uuid =
        sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = 'care_cov'")
            .bind(tenant)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    sqlx::query("DELETE FROM job_knowledge_kind WHERE job_id = $1")
        .bind(id)
        .execute(&e.owner)
        .await
        .unwrap();
    let sub = format!("u-cov-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, "tenant_user").await;
    W {
        e,
        fake,
        tenant,
        job: JobRef {
            id,
            key: "care_cov".into(),
        },
        obs: person(&sub, &[]),
    }
}

impl W {
    async fn languages(&self, langs: &[&str]) {
        let langs: Vec<String> = langs.iter().map(|l| l.to_string()).collect();
        sqlx::query("UPDATE jobs SET languages = $2 WHERE id = $1")
            .bind(self.job.id)
            .bind(langs)
            .execute(&self.e.owner)
            .await
            .unwrap();
    }

    async fn kind(&self, kind: &str, store: Option<&str>, required: bool, packs: &[&str]) {
        let packs: Vec<String> = packs.iter().map(|p| p.to_string()).collect();
        sqlx::query(
            "INSERT INTO job_knowledge_kind (tenant_id, job_id, stage, kind, required, store_path, packs, updated_by) \
             VALUES ($1, $2, 'support', $3, $4, $5, $6, 'test')",
        )
        .bind(self.tenant)
        .bind(self.job.id)
        .bind(kind)
        .bind(required)
        .bind(store)
        .bind(packs)
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// A root leaf `key` (bound in the fake when `bound`); (agent id, strata id).
    async fn leaf(&self, key: &str, bound: bool) -> (Uuid, Option<Uuid>) {
        let sid = bound.then(|| lock(&self.fake.rag).add_store(self.tenant, None, "leaf", key));
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO knowledge_store (tenant_id, id, key, label, kind, strata_store_id, region, updated_by) \
             VALUES ($1, $2, $3, $3, 'leaf', $4, 'eu', 'test')",
        )
        .bind(self.tenant)
        .bind(id)
        .bind(key)
        .bind(sid)
        .execute(&self.e.owner)
        .await
        .unwrap();
        (id, sid)
    }

    /// A strata document in `sid` whose metadata names `language`.
    fn doc(&self, sid: Uuid, language: &str) {
        lock(&self.fake.rag).documents.push((
            self.tenant,
            json!({"id": Uuid::new_v4(), "store_id": sid, "source": "x.pdf", "title": "x",
                   "content_hash": "h", "token_count": 1, "chunk_count": 1, "source_id": null,
                   "source_run": null, "metadata": {"language": language},
                   "created_at": "2026-10-21T10:00:00Z"}),
        ));
    }

    /// One direct upload into `store` in `language`: `ok` files and one failed file.
    async fn upload(&self, store: Uuid, language: &str, ok: usize) {
        let mut files: Vec<Value> = (0..ok)
            .map(|i| {
                json!({"name": format!("f{i}.pdf"), "bytes": 1, "content_type": "application/pdf",
                            "ingest_job_id": null, "error": null})
            })
            .collect();
        files.push(
            json!({"name": "bad.pdf", "bytes": 1, "content_type": "application/pdf",
                          "ingest_job_id": null, "error": "unreadable"}),
        );
        sqlx::query(
            "INSERT INTO knowledge_upload (tenant_id, id, store_id, language, files, created_by) \
             VALUES ($1, $2, $3, $4, $5, 'test')",
        )
        .bind(self.tenant)
        .bind(Uuid::new_v4())
        .bind(store)
        .bind(language)
        .bind(Value::Array(files))
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    async fn coverage(&self) -> Value {
        let uri = format!("/api/jobs/{}/knowledge/coverage", self.job.key);
        let (s, b) = send(&self.e.app, get(&uri, Some(&self.obs))).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        b
    }

    async fn level(&self, lang: &str) -> Level {
        let mut tx = begin_scoped(&self.e.state.db, self.tenant).await.unwrap();
        let l = language_level(&mut tx, &self.job, lang).await.unwrap();
        tx.commit().await.unwrap();
        l
    }

    /// A cached, listed, readable pack with one cached leaf (PK T5: a pack covers a cell only with leaves).
    async fn cached_pack(&self, key: &str, version: i32) {
        sqlx::query(
            "INSERT INTO platform_pack (tenant_id, pack_key, version, root_id, listed) VALUES ($1, $2, $3, $4, true)",
        )
        .bind(self.tenant)
        .bind(key)
        .bind(version)
        .bind(Uuid::new_v4())
        .execute(&self.e.owner)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO platform_pack_leaf (tenant_id, pack_key, version, leaf_id, path) \
             VALUES ($1, $2, $3, $4, ARRAY['root', 'leaf'])",
        )
        .bind(self.tenant)
        .bind(key)
        .bind(version)
        .bind(Uuid::new_v4())
        .execute(&self.e.owner)
        .await
        .unwrap();
    }
}

fn row<'a>(c: &'a Value, kind: &str) -> &'a Value {
    c["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["kind"] == kind)
        .unwrap_or_else(|| panic!("no row {kind}: {c}"))
}

/// The vitest case "counts documents per kind × served language; unbound kinds miss every language; others are
/// unserved", through the route (strata documents, language from metadata).
#[tokio::test]
async fn coverage_matches_ts_reference() {
    let w = world().await;
    w.languages(&["nl", "en"]).await;
    let (_, sa) = w.leaf("sa", true).await;
    let (_, sc) = w.leaf("sc", true).await;
    let (sa, sc) = (sa.unwrap(), sc.unwrap());
    w.kind("a", Some("sa"), true, &[]).await;
    w.kind("b", None, true, &[]).await;
    w.kind("c", Some("sc"), false, &[]).await;
    w.doc(sa, "nl");
    w.doc(sa, "nl");
    w.doc(sa, "de");
    w.doc(sc, "en");
    let c = w.coverage().await;
    assert_eq!(c["job"], "care_cov");
    assert_eq!(c["languages"], json!(["nl", "en"]));
    let a = row(&c, "a");
    assert_eq!(a["documents"], json!({"nl": 2, "en": 0}));
    assert_eq!(a["missing"], json!(["en"]));
    assert_eq!(a["store"], "sa");
    let b = row(&c, "b");
    assert_eq!(b["documents"], json!({"nl": 0, "en": 0}));
    assert_eq!(b["missing"], json!(["nl", "en"]));
    assert_eq!(b["store"], Value::Null);
    let cc = row(&c, "c");
    assert_eq!(cc["documents"], json!({"nl": 0, "en": 1}));
    assert_eq!(cc["missing"], json!(["nl"]));
    assert_eq!(c["gaps"], 3, "only required kinds count: {c}");
    assert_eq!(c["unserved"], json!({"de": 1}));
}

#[tokio::test]
async fn coverage_uses_draft_languages() {
    let w = world().await;
    w.kind("a", None, true, &[]).await;
    w.languages(&["fr", "de"]).await;
    let c = w.coverage().await;
    assert_eq!(
        c["languages"],
        json!(["fr", "de"]),
        "the draft languages (jobs.languages)"
    );
    assert_eq!(row(&c, "a")["missing"], json!(["fr", "de"]));
    assert_eq!(c["gaps"], 2);
}

#[tokio::test]
async fn coverage_unknown_job_404() {
    let w = world().await;
    let (s, b) = send(
        &w.e.app,
        get("/api/jobs/nope/knowledge/coverage", Some(&w.obs)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["error"]["code"], "job_not_found");
}

/// R10: agent cannot list a pack's documents, so a kind with no tenant document in a language but a listed pack is
/// `platform` there (not missing, not a gap) and the language is `partial`; tenant documents still count first.
#[tokio::test]
async fn coverage_pack_only_kind_is_platform_not_gap() {
    let w = world().await;
    w.languages(&["nl", "en"]).await;
    w.cached_pack("base", 1).await;
    let (_, sid) = w.leaf("sa", true).await;
    w.doc(sid.unwrap(), "en");
    w.kind("a", Some("sa"), true, &["base@1"]).await;
    let c = w.coverage().await;
    let a = row(&c, "a");
    assert_eq!(a["platform"], json!(["nl"]), "{c}");
    assert_eq!(a["missing"], json!([]));
    assert_eq!(a["documents"]["en"], 1, "tenant documents count first");
    assert_eq!(c["gaps"], 0);
    assert_eq!(w.level("nl").await, Level::Partial);
    // an unlisted (or unreadable) pin does not cover anything
    sqlx::query("UPDATE platform_pack SET listed = false WHERE tenant_id = $1")
        .bind(w.tenant)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let c = w.coverage().await;
    assert_eq!(row(&c, "a")["missing"], json!(["nl"]));
    assert_eq!(c["gaps"], 1);
    assert_eq!(w.level("nl").await, Level::None);
    // PK T5: listed again but excluded → nothing; not excluded → platform; without cached leaves → nothing
    let wr = &w;
    let set = |sql: &'static str| async move {
        sqlx::query(sql)
            .bind(wr.tenant)
            .execute(&wr.e.owner)
            .await
            .unwrap();
        let c = wr.coverage().await;
        (
            row(&c, "a")["platform"].clone(),
            row(&c, "a")["missing"].clone(),
        )
    };
    assert_eq!(
        set("UPDATE platform_pack SET listed = true, excluded_by = 'base' WHERE tenant_id = $1")
            .await,
        (json!([]), json!(["nl"])),
        "excluded"
    );
    assert_eq!(
        set("UPDATE platform_pack SET excluded_by = NULL WHERE tenant_id = $1").await,
        (json!(["nl"]), json!([])),
        "readable again"
    );
    assert_eq!(
        set("DELETE FROM platform_pack_leaf WHERE tenant_id = $1").await,
        (json!([]), json!(["nl"])),
        "no cached leaves"
    );
}

/// B2's hook from local state: the uploaded files (a failed file does not count) of each required kind's store.
#[tokio::test]
async fn language_level_full_partial_none() {
    let w = world().await;
    let (sa, _) = w.leaf("sa", false).await;
    let (sb, _) = w.leaf("sb", false).await;
    w.kind("a", Some("sa"), true, &[]).await;
    w.kind("b", Some("sb"), true, &[]).await;
    w.kind("c", None, false, &[]).await;
    w.upload(sa, "nl", 1).await;
    w.upload(sb, "nl", 2).await;
    w.upload(sa, "en", 1).await;
    w.upload(sb, "de", 0).await;
    assert_eq!(w.level("nl").await, Level::Full);
    assert_eq!(w.level("en").await, Level::Partial);
    assert_eq!(
        w.level("de").await,
        Level::None,
        "a failed file is no document"
    );
    // the languages view (B2) reads the same hook
    let (s, b) = send(
        &w.e.app,
        get(&format!("/api/jobs/{}/languages", w.job.key), Some(&w.obs)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let level_of = |k: &str| {
        b["options"]
            .as_array()
            .unwrap()
            .iter()
            .find(|o| o["key"] == k)
            .map(|o| o["knowledge"].clone())
    };
    assert_eq!(level_of("nl"), Some(json!("full")));
    assert_eq!(level_of("en"), Some(json!("partial")));
    assert_eq!(level_of("de"), Some(json!("none")));
    // no required kind: full (fixture parity)
    sqlx::query("UPDATE job_knowledge_kind SET required = false WHERE job_id = $1")
        .bind(w.job.id)
        .execute(&w.e.owner)
        .await
        .unwrap();
    assert_eq!(w.level("fr").await, Level::Full);
}
