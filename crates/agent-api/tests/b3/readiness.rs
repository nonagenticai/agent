//! B3 T12: the `providers` and `knowledge` readiness contributors (crate::knowledge::readiness; plan B3 T12, D67/D68/D93,
//! §0 R9/R10/R11): `slot_not_deployed` blocking per required W5 slot on the job's pinned model set, the internal
//! tenant never blocking, the go-live gaps `key_required`, refused LLM bindings, `knowledge_unbound`,
//! `store_unbound`, `knowledge_pack_unavailable` and `knowledge_coverage_gap`, each read in the design checked.
//! Real Postgres 18 (one container for this binary, a fresh database per test); every fact is local state, so no
//! strata fake runs.
use crate::common;

use crate::common::design_contract::{self, ContractJob};
use crate::common::{TestEnv, get, insert_user, person, send};
use agent_api::design;
use agent_api::knowledge::readiness::{KnowledgeReadiness, ProvidersReadiness};
use agent_api::readiness::{
    self, AreaKey, AreaStatus, Assessed, Assessment, Blocks, ReadOf, ReadinessContributor,
    ReadinessGap, ReadinessStatus,
};
use agent_api::release::bundle::Requires;
use axum::http::StatusCode;
use serde_json::json;
use uuid::Uuid;

struct W {
    e: TestEnv,
    f: ContractJob,
}

/// A fresh tenant with one job (seed template v4), its baseline taken before any fixture row below is written.
async fn world() -> W {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    let w = W { e, f };
    let st = &w.e.state;
    let mut tx = w.f.job.begin(&st.db).await.unwrap();
    design::ensure_job(&mut tx, &st.design, &w.f.job_ref())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    w
}

fn requires(slots: &[&str]) -> Requires {
    Requires {
        slots: slots.iter().map(|s| s.to_string()).collect(),
        routes: Vec::new(),
        test_routes: Vec::new(),
    }
}

fn keys(gaps: &[ReadinessGap]) -> Vec<String> {
    gaps.iter().map(|g| g.key.clone()).collect()
}

fn find<'g>(gaps: &'g [ReadinessGap], key: &str) -> &'g ReadinessGap {
    gaps.iter()
        .find(|g| g.key == key)
        .unwrap_or_else(|| panic!("no gap {key} in {:?}", keys(gaps)))
}

fn slots_of(a: &Assessment) -> Vec<String> {
    a.blocking
        .iter()
        .map(|m| format!("{}.{}", m.dependency, m.slot))
        .collect()
}

impl W {
    fn tenant(&self) -> Uuid {
        self.f.tenant
    }

    /// One contributor's assessment of the job (`of`), the checked bundle requiring `slots`.
    async fn assess(&self, area: AreaKey, of: ReadOf, slots: &[&str]) -> Assessment {
        let st = &self.e.state;
        let jr = self.f.job_ref();
        let mut tx = self.f.job.begin(&st.db).await.unwrap();
        let mut checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, of)
            .await
            .unwrap();
        checked.requires = requires(slots);
        let cx = checked.cx(&st.design, st.compiler.as_ref());
        let a = match area {
            AreaKey::Providers => ProvidersReadiness.assess(&mut tx, &cx).await,
            _ => KnowledgeReadiness.assess(&mut tx, &cx).await,
        }
        .unwrap();
        tx.commit().await.unwrap();
        a
    }

    /// The production registry (`AppState.readiness` = `readiness::registry()`) over the same check.
    async fn engine(&self, of: ReadOf, slots: &[&str]) -> Assessed {
        let st = &self.e.state;
        let jr = self.f.job_ref();
        let mut tx = self.f.job.begin(&st.db).await.unwrap();
        let mut checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, of)
            .await
            .unwrap();
        checked.requires = requires(slots);
        let cx = checked.cx(&st.design, st.compiler.as_ref());
        let a = st.readiness.assess(&mut tx, &cx).await.unwrap();
        tx.commit().await.unwrap();
        a
    }

    /// The publish gate of the production registry (approve-with-apply: no release named).
    async fn gate(&self, of: ReadOf, slots: &[&str]) -> Result<(), (StatusCode, &'static str)> {
        let st = &self.e.state;
        let jr = self.f.job_ref();
        let mut tx = self.f.job.begin(&st.db).await.unwrap();
        let mut checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, of)
            .await
            .unwrap();
        checked.requires = requires(slots);
        let cx = checked.cx(&st.design, st.compiler.as_ref());
        let out = st.readiness.gate(&mut tx, &cx, None).await;
        tx.rollback().await.unwrap();
        out.map_err(|e| e.status_and_code())
    }

    /// What strata last reported for `slot` (`provider_slot_remote.observed`).
    async fn observe(&self, slot: &str, source: &str) {
        sqlx::query(
            "INSERT INTO provider_slot_remote (tenant_id, slot, observed_fpr, observed, observed_at) \
             VALUES ($1, $2, 'none', $3, now()) \
             ON CONFLICT (tenant_id, slot) DO UPDATE \
                SET observed = EXCLUDED.observed, observed_fpr = EXCLUDED.observed_fpr, \
                    observed_at = EXCLUDED.observed_at",
        )
        .bind(self.tenant())
        .bind(slot)
        .bind(json!({ "source": source }))
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// The tenant's working (draft) row of `slot`, configured with `model`.
    async fn configure(&self, slot: &str, service: &str, model: &str) {
        sqlx::query(
            "INSERT INTO provider_slot (tenant_id, slot, service, configured, kind, base_url, model, version, \
                                        updated_by) \
             VALUES ($1, $2, $3, true, 'openai_compatible', 'https://p.example/v1', $4, 1, 't12')",
        )
        .bind(self.tenant())
        .bind(slot)
        .bind(service)
        .bind(model)
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// A knowledge store of the tenant (`kind` leaf | group); returns its id.
    async fn store(
        &self,
        key: &str,
        kind: &str,
        parent: Option<Uuid>,
        strata: Option<Uuid>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO knowledge_store (tenant_id, id, key, label, parent_id, kind, strata_store_id, region, \
                                          updated_by) \
             VALUES ($1, $2, $3, $3, $4, $5, $6, 'eu', 't12')",
        )
        .bind(self.tenant())
        .bind(id)
        .bind(key)
        .bind(parent)
        .bind(kind)
        .bind(strata)
        .execute(&self.e.owner)
        .await
        .unwrap();
        id
    }

    /// A working knowledge kind of the job (the draft), written after the baseline.
    async fn kind(&self, kind: &str, required: bool, store: Option<&str>, packs: &[&str]) {
        let packs: Vec<String> = packs.iter().map(|p| p.to_string()).collect();
        sqlx::query(
            "INSERT INTO job_knowledge_kind (tenant_id, job_id, stage, kind, required, description, origin, \
                                             store_path, packs, updated_by) \
             VALUES ($1, $2, 't12', $3, $4, 'Returns policy.', 'base', $5, $6, 't12')",
        )
        .bind(self.tenant())
        .bind(self.f.job.job_id)
        .bind(kind)
        .bind(required)
        .bind(store)
        .bind(packs)
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// A cached platform pack (`platform_pack`, the tenant's last listing).
    async fn pack(&self, key: &str, version: i32, listed: bool, unreadable: bool) {
        sqlx::query(
            "INSERT INTO platform_pack (tenant_id, pack_key, version, root_id, listed, unreadable_at) \
             VALUES ($1, $2, $3, $4, $5, CASE WHEN $6 THEN now() END)",
        )
        .bind(self.tenant())
        .bind(key)
        .bind(version)
        .bind(Uuid::new_v4())
        .bind(listed)
        .bind(unreadable)
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// One cached leaf of pack `key@version` (`platform_pack_leaf`): the pin can be searched.
    async fn pack_leaf(&self, key: &str, version: i32) {
        sqlx::query(
            "INSERT INTO platform_pack_leaf (tenant_id, pack_key, version, leaf_id, path) \
             VALUES ($1, $2, $3, $4, '{}')",
        )
        .bind(self.tenant())
        .bind(key)
        .bind(version)
        .bind(Uuid::new_v4())
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// The `initial` generation of root store `store`, running `embed` (`None` = the deployment default).
    async fn generation(&self, store: Uuid, embed: Option<&str>) {
        sqlx::query(
            "INSERT INTO store_generation (tenant_id, id, store_id, embed, build, state, created_by) \
             VALUES ($1, gen_random_uuid(), $2, $3, 'initial', 'ready', 't12')",
        )
        .bind(self.tenant())
        .bind(store)
        .bind(embed)
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// `qualify` bound to a provider of kind openai (soft-deleted when `deleted`; with a stored key when `keyed` —
    /// readiness reads only whether one is stored, so the sealed value is a `v2:` placeholder the check constraint
    /// accepts). Its id.
    async fn bind_qualify(&self, deleted: bool, keyed: bool) -> Uuid {
        let pid = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO llm_provider (tenant_id, id, name, kind, base_url, models, updated_by, deleted_at, \
                                       secret_sealed, secret_fpr, secret_updated_at) \
             VALUES ($1, $2, $3, 'openai', 'https://llm.example/v1', ARRAY['m1'], 't12', \
                     CASE WHEN $4 THEN now() END, CASE WHEN $5 THEN 'v2:placeholder' END, \
                     CASE WHEN $5 THEN 'fpr' END, CASE WHEN $5 THEN now() END)",
        )
        .bind(self.tenant())
        .bind(pid)
        .bind(format!("t12 provider {}", pid.simple()))
        .bind(deleted)
        .bind(keyed)
        .execute(&self.e.owner)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO llm_use_binding (tenant_id, \"use\", provider_id, model, updated_by) \
             VALUES ($1, 'qualify', $2, 'm1', 't12')",
        )
        .bind(self.tenant())
        .bind(pid)
        .execute(&self.e.owner)
        .await
        .unwrap();
        pid
    }

    async fn platform_owned(&self) {
        sqlx::query("UPDATE tenants SET platform_owned = true WHERE id = $1")
            .bind(self.tenant())
            .execute(&self.e.owner)
            .await
            .unwrap();
    }

    fn kinds_href(&self) -> String {
        format!("/admin/jobs/{}/knowledge-kinds", self.f.job.job_key)
    }
}

fn dor(a: &Assessment, key: &str) -> (ReadinessStatus, String) {
    let s = a
        .steps
        .iter()
        .find(|s| s.key == key)
        .unwrap_or_else(|| panic!("no step {key}"));
    (s.status, s.detail.clone())
}

// ---- providers ------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn missing_required_slot_is_blocking_slot_not_deployed() {
    let w = world().await;
    for slot in ["embed", "ocr", "transcribe"] {
        w.observe(slot, "unconfigured").await;
    }
    let slots = [
        "strata_rag.embed",
        "strata_rag.chat",
        "strata_ingest.ocr",
        "strata_ingest.transcribe",
    ];
    let a = w.assess(AreaKey::Providers, ReadOf::Draft, &slots).await;
    assert_eq!(slots_of(&a), ["strata_rag.embed", "strata_ingest.ocr"]);
    let embed = &a.blocking[0];
    assert_eq!(embed.href, "/admin/providers?tab=knowledge");
    assert_eq!(embed.detail, "W5 slot strata_rag.embed has no deployment");
    assert_eq!(a.blocking[1].href, "/admin/providers?tab=ingest");
    assert_eq!(a.area.status, AreaStatus::Gap);
    assert_eq!((a.area.ready, a.area.total), (2, 4));
    assert_eq!(
        a.area.summary,
        "Missing: strata_rag.embed, strata_ingest.ocr"
    );
    assert_eq!(
        dor(&a, "dor_slots"),
        (
            ReadinessStatus::Missing,
            "Missing: strata_rag.embed, strata_ingest.ocr".to_string()
        )
    );

    // the draft configures embed: the draft is deployed, the live design (the baseline) is not
    w.configure("embed", "strata_rag", "e-1").await;
    let draft = w.assess(AreaKey::Providers, ReadOf::Draft, &slots).await;
    assert_eq!(slots_of(&draft), ["strata_ingest.ocr"]);
    let live = w.assess(AreaKey::Providers, ReadOf::Release, &slots).await;
    assert_eq!(slots_of(&live), ["strata_rag.embed", "strata_ingest.ocr"]);

    // the production registry refuses the publish on it
    assert_eq!(
        w.gate(ReadOf::Draft, &["strata_ingest.ocr"]).await,
        Err((StatusCode::CONFLICT, "release_not_ready"))
    );
    assert_eq!(w.gate(ReadOf::Draft, &["strata_rag.embed"]).await, Ok(()));
}

#[tokio::test]
async fn deployment_default_counts_as_deployed() {
    let w = world().await;
    w.observe("embed", "default").await;
    w.observe("chat", "tenant").await;
    let slots = ["strata_rag.embed", "strata_rag.chat", "strata_rag.rerank"];
    let a = w.assess(AreaKey::Providers, ReadOf::Draft, &slots).await;
    assert!(a.blocking.is_empty(), "{:?}", slots_of(&a));
    assert_eq!(
        dor(&a, "dor_slots"),
        (ReadinessStatus::Done, "3 slots deployed".to_string())
    );
    // a slot no service holds is never deployed (fail loud, not silently passed)
    let a = w
        .assess(AreaKey::Providers, ReadOf::Draft, &["strata_rag.ocr"])
        .await;
    assert_eq!(slots_of(&a), ["strata_rag.ocr"]);
}

#[tokio::test]
async fn internal_tenant_slots_never_block() {
    let w = world().await;
    w.observe("embed", "unconfigured").await;
    w.observe("vision", "unconfigured").await;
    let slots = ["strata_rag.embed", "strata_ingest.vision"];
    let before = w.assess(AreaKey::Providers, ReadOf::Draft, &slots).await;
    assert_eq!(slots_of(&before), slots, "a customer tenant is blocked");
    w.platform_owned().await;
    let a = w.assess(AreaKey::Providers, ReadOf::Draft, &slots).await;
    assert!(a.blocking.is_empty(), "{:?}", slots_of(&a));
    assert!(a.go_live.is_empty(), "{:?}", keys(&a.go_live));
    assert_eq!(a.area.status, AreaStatus::Ok);
}

#[tokio::test]
async fn readiness_reads_pinned_model_set_not_tenant_slot() {
    let w = world().await;
    let root = w.store("kb", "group", None, None).await;
    w.store("policy", "leaf", Some(root), Some(Uuid::new_v4()))
        .await;
    w.generation(root, None).await;
    w.kind("returns", true, Some("kb/policy"), &[]).await;
    w.observe("embed", "unconfigured").await;
    // the tenant slot runs a model now, but the job's pinned generation was built on the (absent) default
    w.configure("embed", "strata_rag", "e-tenant").await;
    let a = w
        .assess(AreaKey::Providers, ReadOf::Draft, &["strata_rag.embed"])
        .await;
    assert_eq!(slots_of(&a), ["strata_rag.embed"]);

    // the pinned generation runs a model and the tenant slot is gone: deployed
    sqlx::query("UPDATE store_generation SET embed = 'e-1' WHERE store_id = $1")
        .bind(root)
        .execute(&w.e.owner)
        .await
        .unwrap();
    sqlx::query("DELETE FROM provider_slot WHERE tenant_id = $1 AND slot = 'embed'")
        .bind(w.tenant())
        .execute(&w.e.owner)
        .await
        .unwrap();
    let a = w
        .assess(AreaKey::Providers, ReadOf::Draft, &["strata_rag.embed"])
        .await;
    assert!(a.blocking.is_empty(), "{:?}", slots_of(&a));
}

/// An unbound `qualify` is no gap: it runs on the inference-router under the tenant's own licence, so neither
/// `llm_unbound` nor `no_llm_provider` exists any more; binding a keyed provider leaves the board clean too.
#[tokio::test]
async fn unbound_qualify_is_no_gap() {
    let w = world().await;
    let a = w.assess(AreaKey::Providers, ReadOf::Draft, &[]).await;
    assert!(a.go_live.is_empty(), "{:?}", keys(&a.go_live));
    assert!(a.blocking.is_empty());
    assert_eq!(a.area.status, AreaStatus::Ok);

    w.bind_qualify(false, true).await;
    let a = w.assess(AreaKey::Providers, ReadOf::Draft, &[]).await;
    assert!(a.go_live.is_empty(), "{:?}", keys(&a.go_live));
    assert_eq!(a.area.status, AreaStatus::Ok);
}

/// `qualify` bound to a provider WITHOUT a key: `qualify::model` builds no model, so every turn answers
/// `TURN_FAILED` — readiness says so (a go-live gap, not a block) instead of reading the binding as ready. Storing a
/// key clears it.
#[tokio::test]
async fn key_required_when_qualify_provider_has_no_key() {
    let w = world().await;
    let pid = w.bind_qualify(false, false).await;
    let a = w.assess(AreaKey::Providers, ReadOf::Draft, &[]).await;
    assert_eq!(keys(&a.go_live), ["key_required:qualify"]);
    let g = find(&a.go_live, "key_required:qualify");
    assert_eq!(g.area, AreaKey::Providers);
    assert_eq!(g.subject, "qualify");
    assert_eq!(g.title, "The LLM provider has no key");
    assert_eq!(
        g.why,
        format!(
            "t12 provider {}: key required. Until a key is stored, turns cannot be answered, every customer message \
             gets \"the assistant is unavailable\".",
            pid.simple()
        )
    );
    assert_eq!(g.href, "/admin/providers");
    assert!(a.blocking.is_empty());
    assert_eq!(a.area.status, AreaStatus::Warn);

    sqlx::query(
        "UPDATE llm_provider SET secret_sealed = 'v2:placeholder', secret_fpr = 'fpr', secret_updated_at = now() \
         WHERE id = $1",
    )
        .bind(pid)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let a = w.assess(AreaKey::Providers, ReadOf::Draft, &[]).await;
    assert!(a.go_live.is_empty(), "{:?}", keys(&a.go_live));
    assert_eq!(a.area.status, AreaStatus::Ok);
}

#[tokio::test]
async fn deleted_provider_binding_is_go_live_gap_in_the_design_checked() {
    let w = world().await;
    // keyless: a deleted provider is provider_deleted only, never also key_required
    w.bind_qualify(true, false).await;
    let draft = w.assess(AreaKey::Providers, ReadOf::Draft, &[]).await;
    assert_eq!(keys(&draft.go_live), ["provider_deleted:llm-use:qualify"]);
    let g = find(&draft.go_live, "provider_deleted:llm-use:qualify");
    assert_eq!(g.subject, "qualify");
    assert!(g.why.contains("which was deleted"), "{}", g.why);
    assert!(draft.blocking.is_empty());
    // the live design (the baseline, taken before the binding) has no binding at all: no gap
    let live = w.assess(AreaKey::Providers, ReadOf::Release, &[]).await;
    assert!(live.go_live.is_empty(), "{:?}", keys(&live.go_live));
}

// ---- knowledge ------------------------------------------------------------------------------------------------------

/// The blocking knowledge items (D110, KH T8) of an assessment: `slot`s.
fn unbound(a: &Assessment) -> Vec<String> {
    a.blocking.iter().map(|m| m.slot.clone()).collect()
}

/// The contract job's one department (its template's default selection).
const CS: &str = "industry_dept/retail/customer_service";

#[tokio::test]
async fn knowledge_unbound_required_kind_without_store() {
    let w = world().await;
    w.kind("returns", true, None, &[]).await;
    w.kind("faq_optional", false, None, &[]).await;
    let a = w.assess(AreaKey::Knowledge, ReadOf::Draft, &[]).await;
    // D110 (KH T8): blocking, per department; never a go-live warning
    assert_eq!(unbound(&a), [format!("{CS}:t12|returns")]);
    let m = &a.blocking[0];
    assert_eq!(m.dependency, "knowledge");
    assert_eq!(
        m.detail,
        format!(
            "Required knowledge kind returns (T12 stage) of department {CS} has no knowledge: bind it to a store or \
             pin a platform pack."
        )
    );
    assert_eq!(m.href, w.kinds_href());
    assert!(!keys(&a.go_live).iter().any(|k| k.contains("t12:returns")));
    assert_eq!(a.area.status, AreaStatus::Gap);
    let (status, detail) = dor(&a, "dor_knowledge");
    assert_eq!(status, ReadinessStatus::Missing);
    assert!(detail.contains("returns (T12)"), "{detail}");
    assert!(!detail.contains("faq_optional"), "{detail}");
    assert_eq!(
        w.gate(ReadOf::Draft, &[]).await,
        Err((StatusCode::CONFLICT, "release_not_ready"))
    );
}

#[tokio::test]
async fn knowledge_reads_the_design_checked_not_the_working_rows() {
    let w = world().await;
    w.kind("returns", true, None, &[]).await;
    let draft = w.assess(AreaKey::Knowledge, ReadOf::Draft, &[]).await;
    assert!(unbound(&draft).contains(&format!("{CS}:t12|returns")));
    // the live design is the baseline, taken before the kind existed
    let live = w.assess(AreaKey::Knowledge, ReadOf::Release, &[]).await;
    assert!(
        !unbound(&live).iter().any(|k| k.contains(":t12|")),
        "{:?}",
        unbound(&live)
    );
}

#[tokio::test]
async fn required_kind_with_pack_only_is_bound() {
    let w = world().await;
    w.pack("base", 1, true, false).await;
    w.pack_leaf("base", 1).await;
    w.kind("returns", true, None, &["base@1"]).await;
    let a = w.assess(AreaKey::Knowledge, ReadOf::Draft, &[]).await;
    let ks = keys(&a.go_live);
    assert!(!ks.iter().any(|k| k.contains("t12:returns")), "{ks:?}");
    assert!(!ks.iter().any(|k| k.contains("base@1")), "{ks:?}");
    assert!(unbound(&a).is_empty(), "{:?}", unbound(&a));
}

#[tokio::test]
async fn unlisted_pin_is_go_live_gap_knowledge_pack_unavailable() {
    let w = world().await;
    w.pack("base", 1, false, false).await;
    // optional: the pin gap is the subject here (a required kind with nothing to search blocks, KH T8)
    w.kind("returns", false, None, &["base@1"]).await;
    let a = w.assess(AreaKey::Knowledge, ReadOf::Draft, &[]).await;
    let g = find(&a.go_live, "knowledge_pack_unavailable:base@1");
    assert_eq!(g.subject, "base@1");
    assert_eq!(g.title, "Platform pack base@1 is unavailable");
    assert!(
        g.why.contains("retired, excluded or deleted — re-pin"),
        "{}",
        g.why
    );
    assert!(g.why.contains("returns (T12)"), "{}", g.why);
    assert_eq!(g.fix_label, "Re-pin base");
    assert!(
        !keys(&a.go_live).iter().any(|k| k.contains("t12:returns")),
        "a pin gap, not an unbound kind"
    );
    let (status, _) = dor(&a, "dor_knowledge");
    assert_eq!(status, ReadinessStatus::Missing);
    // through the engine: a go-live gap, never a blocking item
    let ev = w.engine(ReadOf::Draft, &[]).await;
    assert!(ev.missing.is_empty(), "{:?}", ev.missing);
    let g = find(&ev.go_live, "knowledge_pack_unavailable:base@1");
    assert_eq!((g.blocks, g.area), (Blocks::GoLive, AreaKey::Knowledge));
    assert_eq!(w.gate(ReadOf::Draft, &[]).await, Ok(()));
}

#[tokio::test]
async fn unreadable_pin_is_gap() {
    let w = world().await;
    w.pack("base", 1, true, true).await;
    w.kind("returns", false, None, &["base@1"]).await;
    let a = w.assess(AreaKey::Knowledge, ReadOf::Draft, &[]).await;
    let g = find(&a.go_live, "knowledge_pack_unavailable:base@1");
    assert!(g.why.contains("could not be read"), "{}", g.why);
}

#[tokio::test]
async fn unknown_pin_without_cache_row_is_gap() {
    let w = world().await;
    w.kind("returns", true, None, &["industry/retail@2"]).await;
    let a = w.assess(AreaKey::Knowledge, ReadOf::Draft, &[]).await;
    let g = find(&a.go_live, "knowledge_pack_unavailable:industry/retail@2");
    assert!(g.why.contains("never listed"), "{}", g.why);
    assert_eq!(g.fix_label, "Re-pin industry/retail");
}

#[tokio::test]
async fn listed_pin_no_gap() {
    let w = world().await;
    w.pack("base", 1, true, false).await;
    w.pack("base", 2, false, false).await;
    w.kind("returns", true, None, &["base@1"]).await;
    let a = w.assess(AreaKey::Knowledge, ReadOf::Draft, &[]).await;
    let ks = keys(&a.go_live);
    assert!(
        !ks.iter()
            .any(|k| k.starts_with("knowledge_pack_unavailable:")),
        "{ks:?}"
    );
}

#[tokio::test]
async fn store_unbound_is_bound_leaf_without_strata_store() {
    let w = world().await;
    let root = w.store("kb", "group", None, None).await;
    let leaf = w.store("policy", "leaf", Some(root), None).await;
    w.kind("returns", false, Some("kb/policy"), &[]).await;
    w.kind("warranty", false, Some("kb/gone"), &[]).await;
    let a = w.assess(AreaKey::Knowledge, ReadOf::Draft, &[]).await;
    let g = find(&a.go_live, "store_unbound:t12:returns");
    assert_eq!(
        g.title,
        "returns is bound to kb/policy, which has no knowledge store (T12 stage)"
    );
    find(&a.go_live, "store_unbound:t12:warranty");
    assert!(a.blocking.is_empty());

    sqlx::query("UPDATE knowledge_store SET strata_store_id = $2 WHERE id = $1")
        .bind(leaf)
        .bind(Uuid::new_v4())
        .execute(&w.e.owner)
        .await
        .unwrap();
    let a = w.assess(AreaKey::Knowledge, ReadOf::Draft, &[]).await;
    let ks = keys(&a.go_live);
    assert!(
        !ks.contains(&"store_unbound:t12:returns".to_string()),
        "{ks:?}"
    );
    assert!(
        ks.contains(&"store_unbound:t12:warranty".to_string()),
        "{ks:?}"
    );
}

#[tokio::test]
async fn knowledge_coverage_gap_is_warning_never_blocking() {
    let w = world().await;
    sqlx::query("UPDATE jobs SET languages = ARRAY['en', 'nl'] WHERE id = $1")
        .bind(w.f.job.job_id)
        .execute(&w.e.owner)
        .await
        .unwrap();
    let root = w.store("kb", "group", None, None).await;
    let leaf = w
        .store("policy", "leaf", Some(root), Some(Uuid::new_v4()))
        .await;
    sqlx::query(
        "INSERT INTO knowledge_upload (tenant_id, id, store_id, language, files, created_by) \
         VALUES ($1, $2, $3, 'en', '[{\"name\": \"returns.pdf\"}]', 't12')",
    )
    .bind(w.tenant())
    .bind(Uuid::new_v4())
    .bind(leaf)
    .execute(&w.e.owner)
    .await
    .unwrap();
    w.kind("returns", true, Some("kb/policy"), &[]).await;
    let a = w.assess(AreaKey::Knowledge, ReadOf::Draft, &[]).await;
    let g = find(&a.go_live, "knowledge_coverage_gap:t12:returns");
    assert_eq!(g.title, "returns has no documents in nl (T12 stage)");
    assert!(a.blocking.is_empty());
    let ev = w.engine(ReadOf::Draft, &[]).await;
    assert!(ev.missing.is_empty(), "{:?}", ev.missing);
    assert_eq!(
        find(&ev.go_live, "knowledge_coverage_gap:t12:returns").blocks,
        Blocks::GoLive
    );
    assert_eq!(w.gate(ReadOf::Draft, &[]).await, Ok(()));

    // a document in nl closes the gap
    sqlx::query(
        "INSERT INTO knowledge_upload (tenant_id, id, store_id, language, files, created_by) \
         VALUES ($1, $2, $3, 'nl', '[{\"name\": \"retour.pdf\"}]', 't12')",
    )
    .bind(w.tenant())
    .bind(Uuid::new_v4())
    .bind(leaf)
    .execute(&w.e.owner)
    .await
    .unwrap();
    let a = w.assess(AreaKey::Knowledge, ReadOf::Draft, &[]).await;
    assert!(
        !keys(&a.go_live).contains(&"knowledge_coverage_gap:t12:returns".to_string()),
        "{:?}",
        keys(&a.go_live)
    );
}

// ---- registration ---------------------------------------------------------------------------------------------------

#[tokio::test]
async fn contributors_are_registered_in_the_production_registry() {
    let areas = readiness::registry().areas();
    assert!(areas.contains(&AreaKey::Providers), "{areas:?}");
    assert!(areas.contains(&AreaKey::Knowledge), "{areas:?}");

    let w = world().await;
    let sub = format!("u-t12-obs-{}", w.tenant().simple());
    insert_user(&w.e.owner, &sub, w.tenant(), "tenant_user").await;
    let (s, b) = send(
        &w.e.app,
        get(
            &format!("/api/jobs/{}/readiness/board?of=draft", w.f.job.job_key),
            Some(&person(&sub, &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for key in ["providers", "knowledge"] {
        let a = b["areas"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["key"] == key)
            .unwrap_or_else(|| panic!("area {key}: {b}"))
            .clone();
        assert_ne!(a["summary"], "Not checked yet", "{a}");
        assert_ne!(a["status"], "todo", "{a}");
    }
}
