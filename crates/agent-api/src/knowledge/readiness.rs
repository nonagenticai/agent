//! B3's readiness contributors (plan B3 T12, D67/D68/D93, §0 R9/R10/R11): the `providers` and `knowledge` areas of
//! the J3 board (crate::readiness, registered in `readiness::registry()`).
//!
//! **Providers** ([`ProvidersReadiness`]):
//! - blocking `slot_not_deployed` (a [`MissingItem`] `{dependency: strata_rag|strata_ingest, slot}`) for each W5 slot
//!   the checked bundle requires (`cx.requires.slots`) that has nothing deployed behind it. `transcribe` is optional
//!   (`Slot::required`) and never blocks; nonagentic's own tenant (D70, `tenants.platform_owned`) never blocks.
//! - D93: the job's model for a slot is its PINNED generation's (crate::knowledge::generations, per `cx.design`: the
//!   draft pin for `of=draft`, the snapshot's for a release / live) for every root store its knowledge kinds bind;
//!   only a job that binds no generation reads the tenant slot's design value (`provider_slot`, same `cx.design`).
//!   A design value (`source='default'` included, T4b) or a pinned model counts as deployed. A slot with no model of
//!   the tenant's runs strata's deployment default: it is NOT deployed only when strata last reported it
//!   `unconfigured` (`provider_slot_remote.observed`, kept by the slots view and every push). No observation yet =
//!   the deployment default is assumed (a fresh tenant is not blocked by a fact nobody has read).
//! - go-live gaps: `key_required` (`qualify`
//!   bound to a provider without a key: every turn answers `TURN_FAILED`) and one gap per binding
//!   [`llm::resolve`] would refuse (`provider_deleted`, a kind that cannot serve the use; [`llm::problems`]);
//!   `embed_push_withheld` (review I4) while the newest push withheld the published embed model.
//!
//! **Knowledge** ([`KnowledgeReadiness`]):
//! - blocking `knowledge_unbound` (D110/D115, KH T8; a [`MissingItem`] `{dependency: knowledge, slot:
//!   <department>:<stage>|<kind>}`, see [`knowledge_item`]) for each department of the selection checked and each
//!   required kind it has (its own, an archetype's, or inherited from base / industry / job: the template fold of the
//!   design's pins, `CompiledKnowledge::kinds_of_department`, plus any required working kind the fold does not have)
//!   with zero resolvable leaves in the design checked ([`kinds::leaf_set`]): a tenant leaf (the bound path is a leaf
//!   of the tree) or a frozen pack pin with leaves (`PackLeaves::live`). A `tenant_data` kind counts only a FED tenant
//!   leaf — an upload, or a source / connector with a completed run that produced documents — whatever languages the
//!   job serves (packs alone never satisfy it). A stage `*` binding serves every stage (D107): a kind is resolvable when its
//!   own binding or `*|<kind>`'s is. Every department must be satisfied; the item names the department (no
//!   department selected: `job`, next to the hierarchy item). A kind a layer upgrade introduces is in the fold of the
//!   new pins, so the apply of the release cut from them is refused until it is bound (the cut lists it as the
//!   informational upgrade finding `required_kind_introduced`, crate::release::conflicts);
//! - blocking primary-language coverage (D110, slot `<department>:<stage>|<kind>:<lang>`): a resolvable required kind
//!   with nothing in the job's primary language (`languages[0]`): no live pack (non-`tenant_data` kinds), and no
//!   upload or source run with documents in that language in its tenant leaf ([`coverage::leaf_languages`]);
//!
//! and the go-live gaps (D68: a warning, never blocking):
//! - `store_unbound`: a kind bound to a path that is no leaf of the tree, or to a leaf with no strata store under the
//!   job's pinned generation;
//! - `knowledge_pack_unavailable` (R10/R11): a pinned `pack_key@version` with no `platform_pack` row, excluded
//!   (`excluded_by`), `unreadable_at` set, classified missing / draft / retired, or `listed = false`, the text naming
//!   the reason: searches run without it (degraded), so the next release should re-pin;
//! - `knowledge_coverage_gap`: a required kind bound to a store with no document in a served language other than the
//!   primary one (crate::knowledge::coverage over the locally known documents; a pack-only cell is `platform`, not a
//!   gap).
//!
//! LOCAL STATE ONLY: no upstream call. Design is read through the read API in `cx.design`.
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use agent_model::compile::Selection;
use agent_model::kinds::stage_knowledge::ANY_STAGE;

use super::coverage::{self, CoverageKind};
use super::generations::{self, ModelSet};
use super::kinds::{self, CachedPack};
use super::packs::template as template_packs;
use super::stores;
use crate::design::{self, DesignOf, DraftRef, JobRef};
use crate::error::ApiResult;
use crate::platform::templates::TemplatePin;
use crate::providers::kinds::Slot;
use crate::providers::llm::{self, Use};
use crate::providers::slots::{self, SlotDesign};
use crate::providers::targets::{slot_ref, use_ref};
use crate::readiness::board::{area_shell, job_path, step};
use crate::readiness::{
    AreaKey, AreaStatus, Assessment, Blocks, Enforced, Fixer, KNOWLEDGE_DEPENDENCY, MissingItem,
    Readiness, ReadinessContributor, ReadinessCx, ReadinessGap, ReadinessStatus,
};

/// Register B3's contributors (crate::readiness::registry).
pub fn register(r: &mut Readiness) {
    r.register(Arc::new(ProvidersReadiness));
    r.register(Arc::new(KnowledgeReadiness));
}

/// The `providers` area.
pub struct ProvidersReadiness;

impl ReadinessContributor for ProvidersReadiness {
    fn area(&self) -> AreaKey {
        AreaKey::Providers
    }

    fn assess<'a>(
        &'a self,
        c: &'a mut PgConnection,
        cx: &'a ReadinessCx<'a>,
    ) -> BoxFuture<'a, ApiResult<Assessment>> {
        Box::pin(assess_providers(c, cx))
    }
}

/// The `knowledge` area.
pub struct KnowledgeReadiness;

impl ReadinessContributor for KnowledgeReadiness {
    fn area(&self) -> AreaKey {
        AreaKey::Knowledge
    }

    fn assess<'a>(
        &'a self,
        c: &'a mut PgConnection,
        cx: &'a ReadinessCx<'a>,
    ) -> BoxFuture<'a, ApiResult<Assessment>> {
        Box::pin(assess_knowledge(c, cx))
    }
}

/// What a pack pin's cache state says when it cannot be searched.
const REPIN: &str = "retired, excluded or deleted — re-pin";

/// `support` → `Support` (fixture `stageLabel`); `*` (D107: every stage) → `Every`.
fn stage_label(s: &str) -> String {
    if s == ANY_STAGE {
        return "Every".to_string();
    }
    let mut chars = s.chars();
    match chars.next() {
        Some(f) => f.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A go-live gap of `area` (fixer the tenant admin, enforced from M4; `blocks` is forced by the engine).
fn gap(
    area: AreaKey,
    key: String,
    subject: String,
    title: String,
    why: String,
    fix_label: String,
    href: String,
) -> ReadinessGap {
    ReadinessGap {
        key,
        area,
        subject,
        title,
        why,
        cause: None,
        blocks: Blocks::GoLive,
        fixer: Fixer::TenantAdmin,
        fix_label,
        href,
        enforced_from: Enforced::M4,
    }
}

// ---- shared reads ---------------------------------------------------------------------------------------------------

/// D70: the job's tenant is nonagentic's own (`tenants.platform_owned`, reconciled at boot).
async fn platform_owned(c: &mut PgConnection, job: &JobRef) -> ApiResult<bool> {
    let owned: Option<bool> = sqlx::query_scalar(
        "SELECT t.platform_owned FROM jobs j JOIN tenants t ON t.id = j.tenant_id WHERE j.id = $1",
    )
    .bind(job.id)
    .fetch_optional(&mut *c)
    .await?;
    Ok(owned.unwrap_or(false))
}

/// One knowledge kind as the checked design holds it.
struct KindView {
    stage: String,
    kind: String,
    required: bool,
    tenant_data: bool,
    description: String,
    store: Option<String>,
    packs: Vec<String>,
}

impl KindView {
    fn label(&self) -> String {
        format!("{} ({})", self.kind, stage_label(&self.stage))
    }
}

fn ref_text(r: &DraftRef, f: &str) -> String {
    r.get(f).and_then(Value::as_str).unwrap_or("").to_string()
}

/// The knowledge kinds of `cx.design` (their bindings), with `required` / `description` from the job's kind rows
/// (template facts, not design); a place without a value is left out.
async fn kind_views(c: &mut PgConnection, cx: &ReadinessCx<'_>) -> ApiResult<Vec<KindView>> {
    let rows = kinds::rows(&mut *c, cx.job.id).await?;
    let placed = design::values_of_kind(&mut *c, cx.reg, cx.job, &cx.design, kinds::KIND).await?;
    let mut out = Vec::with_capacity(placed.len());
    for p in placed {
        let Some(v) = p.snap.as_ref() else {
            continue;
        };
        let (store, packs) = kinds::value_parts(v)?;
        let stage = ref_text(&p.r#ref, "stage");
        let kind = ref_text(&p.r#ref, "kind");
        let row = rows.iter().find(|r| r.stage == stage && r.kind == kind);
        out.push(KindView {
            required: row.is_some_and(|r| r.required),
            tenant_data: row.is_some_and(|r| r.tenant_data),
            description: row.map(|r| r.description.clone()).unwrap_or_default(),
            stage,
            kind,
            store,
            packs,
        });
    }
    out.sort_by_key(|k| (k.stage.clone(), k.kind.clone()));
    Ok(out)
}

// ---- providers ------------------------------------------------------------------------------------------------------

/// The model sets the job runs under `cx.design`: the pinned generation of each root store its kinds bind (D93).
/// Empty = the job binds no generation.
async fn pinned_sets(
    c: &mut PgConnection,
    cx: &ReadinessCx<'_>,
    views: &[KindView],
) -> ApiResult<Vec<ModelSet>> {
    let all = stores::all(&mut *c).await?;
    let mut roots: Vec<Uuid> = Vec::new();
    for k in views {
        let Some(path) = k.store.as_deref() else {
            continue;
        };
        if let Some(leaf) = kinds::leaf_by_path(&all, path)
            && let Some(root) = stores::root_of(&all, leaf.id)
            && !roots.contains(&root)
        {
            roots.push(root);
        }
    }
    let mut sets = Vec::with_capacity(roots.len());
    for id in roots {
        let Some(root) = all.iter().find(|s| s.id == id) else {
            continue;
        };
        if let Some(g) = generations::pinned(&mut *c, cx.job, root, &cx.design).await? {
            sets.push(g.models());
        }
    }
    Ok(sets)
}

/// What strata last reported per slot (`provider_slot_remote.observed ->> 'source'`).
async fn observed_sources(c: &mut PgConnection) -> ApiResult<BTreeMap<String, Option<String>>> {
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT slot, observed ->> 'source' FROM provider_slot_remote")
            .fetch_all(&mut *c)
            .await?;
    Ok(rows.into_iter().collect())
}

/// `strata_rag.embed` → the slot, when the dependency is the service that holds it.
fn parse_required(s: &str) -> Option<(&str, &str, Slot)> {
    let (dep, name) = s.split_once('.')?;
    let slot = Slot::parse(name)?;
    (slots::target_of(slot).as_str() == dep).then_some((dep, name, slot))
}

/// `slot_not_deployed` for `required` (`<dependency>.<slot>`).
fn missing_slot(required: &str) -> MissingItem {
    let (dependency, slot) = required.split_once('.').unwrap_or((required, required));
    let tab = if dependency == "strata_ingest" {
        "ingest"
    } else {
        "knowledge"
    };
    MissingItem {
        dependency: dependency.to_string(),
        slot: slot.to_string(),
        detail: format!("W5 slot {required} has no deployment"),
        href: format!("/admin/providers?tab={tab}"),
    }
}

/// The required slots of `cx` with nothing deployed behind them (module doc), in `requires` order.
async fn undeployed(c: &mut PgConnection, cx: &ReadinessCx<'_>) -> ApiResult<Vec<MissingItem>> {
    if cx.requires.slots.is_empty() || platform_owned(&mut *c, cx.job).await? {
        return Ok(Vec::new());
    }
    let views = kind_views(&mut *c, cx).await?;
    let sets = pinned_sets(&mut *c, cx, &views).await?;
    let observed = observed_sources(&mut *c).await?;
    let mut out = Vec::new();
    for required in &cx.requires.slots {
        let Some((_, name, slot)) = parse_required(required) else {
            // a slot no service holds can never be deployed: fail loud
            out.push(missing_slot(required));
            continue;
        };
        if !slot.required() {
            continue;
        }
        let has_model = if sets.is_empty() {
            let v = design::value(
                &mut *c,
                cx.reg,
                cx.job,
                &cx.design,
                "provider_slot",
                &slot_ref(slot),
            )
            .await?;
            SlotDesign::from_value(v.as_ref())?.is_some()
        } else {
            sets.iter().all(|m| m.get(slot).is_some())
        };
        let unconfigured = observed
            .get(name)
            .and_then(|s| s.as_deref())
            .is_some_and(|s| s == "unconfigured");
        if !has_model && unconfigured {
            out.push(missing_slot(required));
        }
    }
    Ok(out)
}

/// The LLM go-live gaps of `cx.design`: `key_required` (bound to a provider without a key) and the bindings `resolve`
/// would refuse. An unbound `qualify` is no gap: it runs on the inference-router under the tenant's own licence.
async fn llm_gaps(c: &mut PgConnection, cx: &ReadinessCx<'_>) -> ApiResult<Vec<ReadinessGap>> {
    if platform_owned(&mut *c, cx.job).await? {
        // D70: the internal pgx vLLM serves the platform tenant's uses
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let q = design::value(
        &mut *c,
        cx.reg,
        cx.job,
        &cx.design,
        "llm_use",
        &use_ref(Use::Qualify.as_str()),
    )
    .await?;
    let bound = q.as_ref().and_then(|v| {
        let pid = v.get("provider_id").and_then(Value::as_str)?;
        v.get("model").and_then(Value::as_str)?;
        Some(pid)
    });
    // bound to a provider without a key: `qualify::model` builds no model and every turn answers `TURN_FAILED`
    // . A deleted or unknown provider is `llm::problems`' gap.
    let keyless = match bound {
        Some(pid) => match Uuid::parse_str(pid) {
            Ok(id) => llm::provider(&mut *c, id, llm::Lock::None)
                .await?
                .filter(|p| p.secret_sealed.is_none()),
            Err(_) => None,
        },
        None => None,
    };
    if let Some(p) = keyless {
        out.push(gap(
            AreaKey::Providers,
            "key_required:qualify".to_string(),
            Use::Qualify.as_str().to_string(),
            "The LLM provider has no key".to_string(),
            format!(
                "{}: key required. Until a key is stored, turns cannot be answered, every customer message gets \
                 \"the assistant is unavailable\".",
                p.name
            ),
            "Store the provider\u{2019}s key".to_string(),
            "/admin/providers".to_string(),
        ));
    }
    for p in llm::problems(&mut *c, cx.reg, cx.job, &cx.design).await? {
        let label = Use::parse(p.use_name).map_or(p.use_name, Use::label);
        out.push(gap(
            AreaKey::Providers,
            format!("{}:llm-use:{}", p.code, p.use_name),
            p.use_name.to_string(),
            format!("{label} cannot use its provider"),
            p.message,
            "Bind another provider".to_string(),
            "/admin/providers".to_string(),
        ));
    }
    Ok(out)
}

/// Review I4: the published embed model strata does not run yet (the newest push withheld it while a job still reads a
/// root on the deployment default; crate::providers::slots::withheld). A warning: the job keeps searching what it
/// searched.
async fn withheld_gap(
    c: &mut PgConnection,
    cx: &ReadinessCx<'_>,
) -> ApiResult<Option<ReadinessGap>> {
    let tenant: Option<Uuid> = sqlx::query_scalar("SELECT tenant_id FROM jobs WHERE id = $1")
        .bind(cx.job.id)
        .fetch_optional(&mut *c)
        .await?;
    let Some(tenant) = tenant else {
        return Ok(None);
    };
    let Some(w) = slots::withheld(&mut *c, tenant).await? else {
        return Ok(None);
    };
    let model = w
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("the deployment default");
    let roots: Vec<&str> = w
        .get("roots")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let held = if roots.is_empty() {
        "knowledge on the deployment default".to_string()
    } else {
        roots.join(", ")
    };
    Ok(Some(gap(
        AreaKey::Providers,
        "embed_push_withheld:embed".to_string(),
        Slot::Embed.as_str().to_string(),
        "The new embedding model is not live yet".to_string(),
        format!(
            "The published embedding model {model} is withheld: {held} still holds content a job reads on the old \
             model. Rebuild it through its model change and publish; the model then goes out (retry from the \
             Providers page)."
        ),
        "Open the model changes".to_string(),
        "/admin/providers?tab=knowledge".to_string(),
    )))
}

async fn assess_providers(c: &mut PgConnection, cx: &ReadinessCx<'_>) -> ApiResult<Assessment> {
    let job = cx.job.key.as_str();
    let blocking = undeployed(&mut *c, cx).await?;
    let mut go_live = llm_gaps(&mut *c, cx).await?;
    go_live.extend(withheld_gap(&mut *c, cx).await?);
    let n = cx.requires.slots.len();
    let names: Vec<String> = blocking
        .iter()
        .map(|m| format!("{}.{}", m.dependency, m.slot))
        .collect();

    let mut area = area_shell(AreaKey::Providers, job);
    area.template_part = format!("{n} W5 slots, from the features you selected");
    area.ready = count(n.saturating_sub(blocking.len()));
    area.total = count(n);
    area.status = if !blocking.is_empty() {
        AreaStatus::Gap
    } else if !go_live.is_empty() {
        AreaStatus::Warn
    } else {
        AreaStatus::Ok
    };
    area.summary = if !blocking.is_empty() {
        format!("Missing: {}", names.join(", "))
    } else if !go_live.is_empty() {
        let titles: Vec<&str> = go_live.iter().map(|g| g.title.as_str()).collect();
        format!("Every slot has a deployment · {}", titles.join("; "))
    } else {
        "Every slot has a deployment".to_string()
    };
    let dor = if blocking.is_empty() {
        step(
            job,
            "dor_slots",
            ReadinessStatus::Done,
            format!("{n} slots deployed"),
        )
    } else {
        step(
            job,
            "dor_slots",
            ReadinessStatus::Missing,
            format!("Missing: {}", names.join(", ")),
        )
    };
    Ok(Assessment {
        area,
        blocking,
        testing: Vec::new(),
        go_live,
        steps: vec![dor],
    })
}

// ---- knowledge ------------------------------------------------------------------------------------------------------

/// Why a pinned pack cannot be searched, from the local cache, naming the reason (PK-5/PK-10: excluded / retired /
/// missing / unpublished); `None` = listed and readable. The gap stays a warning (R11: a search degrades, it never
/// fails).
fn pack_state(cache: &BTreeMap<String, CachedPack>, pin: &str) -> Option<String> {
    let Some(p) = cache.get(pin) else {
        return Some("was never listed for this tenant".into());
    };
    if let Some(prefix) = &p.excluded_by {
        return Some(format!(
            "is excluded for this tenant (platform exclusion {prefix})"
        ));
    }
    if p.unreadable_at.is_some() {
        return Some("could not be read".into());
    }
    match p.state.as_deref() {
        Some("missing") => Some("is missing from the platform".into()),
        Some("draft") => Some("is not published yet".into()),
        Some("retired") if !p.listed => Some("is retired".into()),
        _ if !p.listed => Some("is no longer listed".into()),
        _ => None,
    }
}

async fn assess_knowledge(c: &mut PgConnection, cx: &ReadinessCx<'_>) -> ApiResult<Assessment> {
    let job = cx.job.key.as_str();
    let href = job_path(job, "/knowledge-kinds");
    let views = kind_views(&mut *c, cx).await?;
    let all = stores::all(&mut *c).await?;
    let cache: BTreeMap<String, CachedPack> = kinds::cached_packs(&mut *c)
        .await?
        .into_iter()
        .map(|p| (p.pin(), p))
        .collect();

    let mut go_live: Vec<ReadinessGap> = Vec::new();
    // D110/D115: required knowledge per department — blocking
    let languages: Vec<String> = design::typed::<Vec<String>>(
        &mut *c,
        cx.reg,
        cx.job,
        &cx.design,
        "languages",
        &DraftRef::new(),
    )
    .await?
    .unwrap_or_default();
    let required = required_kinds(&mut *c, cx, &views).await?;
    let primary = languages.first().map(String::as_str);
    let mut states: BTreeMap<(String, String), KindState> = BTreeMap::new();
    for k in &required {
        let st = kind_state(&mut *c, cx, &views, k, primary).await?;
        states.insert((k.stage.clone(), k.kind.clone()), st);
    }
    let departments: Vec<Option<&str>> = match cx.selection {
        Some(sel) if !sel.departments.is_empty() => {
            sel.departments.iter().map(|d| Some(d.as_str())).collect()
        }
        _ => vec![None],
    };
    let mut blocking: Vec<MissingItem> = Vec::new();
    for dept in &departments {
        for k in required.iter().filter(|k| k.of(*dept)) {
            let st = &states[&(k.stage.clone(), k.kind.clone())];
            if let Some(item) = knowledge_item(job, *dept, k, st, primary) {
                blocking.push(item);
            }
        }
    }
    // store_unbound: the bound leaf has no strata store under the job's pinned generation
    let mut store_gaps = 0usize;
    for k in &views {
        let Some(path) = k.store.as_deref() else {
            continue;
        };
        let bound = match kinds::leaf_by_path(&all, path) {
            Some(leaf) => generations::strata_for(&mut *c, cx.job, leaf.id, &cx.design)
                .await?
                .is_some(),
            None => false,
        };
        if bound {
            continue;
        }
        store_gaps += 1;
        go_live.push(gap(
            AreaKey::Knowledge,
            format!("store_unbound:{}:{}", k.stage, k.kind),
            k.kind.clone(),
            format!(
                "{} is bound to {path}, which has no knowledge store ({} stage)",
                k.kind,
                stage_label(&k.stage)
            ),
            format!(
                "{path} is not a leaf of your store tree or is not created in the knowledge service yet, so a search of {} finds nothing there.",
                k.kind
            ),
            format!("Check {path}"),
            href.clone(),
        ));
    }
    // knowledge_pack_unavailable (R10/R11): one gap per pin, naming the kinds that pin it
    let mut pins: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for k in &views {
        for pin in &k.packs {
            pins.entry(pin.as_str()).or_default().push(k.label());
        }
    }
    let mut pack_gaps = 0usize;
    for (pin, used) in &pins {
        let Some(state) = pack_state(&cache, pin) else {
            continue;
        };
        pack_gaps += 1;
        let key = pin.rsplit_once('@').map_or(*pin, |(k, _)| k);
        go_live.push(gap(
            AreaKey::Knowledge,
            format!("knowledge_pack_unavailable:{pin}"),
            pin.to_string(),
            format!("Platform pack {pin} is unavailable"),
            format!(
                "{pin} {state} ({REPIN}). Searches of {} run without it and say so (degraded).",
                used.join(", ")
            ),
            format!("Re-pin {key}"),
            href.clone(),
        ));
    }
    // knowledge_coverage_gap (D68): a warning per required, store-bound kind missing a served language other than the
    // primary one (D110: the primary language blocks, above)
    // a pack covers a cell only when it is readable, not excluded and has leaves (T5)
    let leaves = kinds::cached_leaf_counts(&mut *c).await?;
    let readable: HashSet<&str> = cache
        .iter()
        .filter(|(pin, p)| {
            p.listed
                && p.unreadable_at.is_none()
                && p.excluded_by.is_none()
                && leaves.get(pin.as_str()).is_some_and(|n| *n > 0)
        })
        .map(|(pin, _)| pin.as_str())
        .collect();
    let ckinds: Vec<CoverageKind> = views
        .iter()
        .map(|k| CoverageKind {
            stage: k.stage.clone(),
            kind: k.kind.clone(),
            required: k.required,
            store: k.store.clone(),
            packs: k.packs.iter().any(|p| readable.contains(p.as_str())),
        })
        .collect();
    let docs = coverage::upload_docs(&mut *c).await?;
    let cov = coverage::coverage_of(job, &languages, &ckinds, &all, &docs);
    for r in cov.rows.iter().filter(|r| r.required && r.store.is_some()) {
        let missing: Vec<&str> = r
            .missing
            .iter()
            .map(String::as_str)
            .filter(|l| Some(*l) != primary)
            .collect();
        if missing.is_empty() {
            continue;
        }
        let langs = missing.join(", ");
        go_live.push(gap(
            AreaKey::Knowledge,
            format!("knowledge_coverage_gap:{}:{}", r.stage, r.kind),
            r.kind.clone(),
            format!(
                "{} has no documents in {langs} ({} stage)",
                r.kind,
                stage_label(&r.stage)
            ),
            format!(
                "The job serves {langs}, but the store bound to {} has no document in it, so the agent cannot quote it there. A warning: publishing is not blocked.",
                r.kind
            ),
            format!("Add {} documents", r.kind),
            href.clone(),
        ));
    }

    let total = states.len();
    let unready: Vec<String> = required
        .iter()
        .filter(|k| !states[&(k.stage.clone(), k.kind.clone())].ready(k.tenant_data, primary))
        .map(RequiredKind::label)
        .collect();
    let mut area = area_shell(AreaKey::Knowledge, job);
    area.template_part = format!("{total} required kinds across the journey stages");
    area.ready = count(total - unready.len());
    area.total = count(total);
    area.status = if !blocking.is_empty() {
        AreaStatus::Gap
    } else if go_live.is_empty() {
        AreaStatus::Ok
    } else {
        AreaStatus::Warn
    };
    area.summary = if !unready.is_empty() {
        format!("Not ready to go live: {}", unready.join(", "))
    } else if !go_live.is_empty() {
        let n = go_live.len();
        format!(
            "Every required kind is bound · {n} warning{}",
            if n == 1 { "" } else { "s" }
        )
    } else {
        "Every required kind is bound".to_string()
    };
    let dor = if !unready.is_empty() {
        step(
            job,
            "dor_knowledge",
            ReadinessStatus::Missing,
            format!("Not ready to go live: {}", unready.join(", ")),
        )
    } else if store_gaps + pack_gaps > 0 {
        step(
            job,
            "dor_knowledge",
            ReadinessStatus::Missing,
            format!(
                "Bound, but {} binding{} cannot be searched",
                store_gaps + pack_gaps,
                if store_gaps + pack_gaps == 1 { "" } else { "s" }
            ),
        )
    } else {
        step(job, "dor_knowledge", ReadinessStatus::Done, "Bound")
    };
    Ok(Assessment {
        area,
        blocking,
        testing: Vec::new(),
        go_live,
        steps: vec![dor],
    })
}

// ---- required knowledge (D110/D115, KH T8) --------------------------------------------------------------------------

/// One required knowledge kind of the design checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredKind {
    /// A stage, or `*` (every stage).
    pub stage: String,
    pub kind: String,
    /// D110: only the tenant's own leaf satisfies it.
    pub tenant_data: bool,
    pub description: String,
    /// D115: the departments that have it (empty = every department: base, industry and job kinds).
    pub departments: Vec<String>,
}

impl RequiredKind {
    fn label(&self) -> String {
        format!("{} ({})", self.kind, stage_label(&self.stage))
    }

    /// Whether department `dept` has the kind (`None` = the job has no department: every kind is the job's).
    fn of(&self, dept: Option<&str>) -> bool {
        match dept {
            Some(d) => self.departments.is_empty() || self.departments.iter().any(|x| x == d),
            None => true,
        }
    }
}

/// What resolves a required kind in the design checked (its own binding or `*|<kind>`'s).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KindState {
    /// The bound path is a leaf of the tenant's tree.
    pub tenant: bool,
    /// At least one pinned pack has leaves to search (`PackLeaves::live`).
    pub packs: bool,
    /// The tenant leaf is fed: an upload, or a source (connector) with a completed run that produced documents.
    pub tenant_fed: bool,
    /// The tenant leaf holds documents in the job's primary language ([`coverage::leaf_languages`]).
    pub tenant_primary: bool,
}

impl KindState {
    /// At least one resolvable leaf (D110): the tenant leaf, or — unless `tenant_data` — a frozen pack's. A
    /// `tenant_data` kind needs its tenant leaf fed (an upload, or a source / connector run with documents), whatever
    /// languages the job serves.
    pub fn resolvable(&self, tenant_data: bool) -> bool {
        if tenant_data {
            self.tenant && self.tenant_fed
        } else {
            self.tenant || self.packs
        }
    }

    /// Something in the primary language: a live pack covers every language (R10 `platform` cell), except for a
    /// `tenant_data` kind.
    pub fn primary_covered(&self, tenant_data: bool) -> bool {
        self.tenant_primary || (!tenant_data && self.packs)
    }

    /// Nothing of it blocks: resolvable, and the primary language (when the job serves one) covered.
    fn ready(&self, tenant_data: bool, primary: Option<&str>) -> bool {
        self.resolvable(tenant_data) && (primary.is_none() || self.primary_covered(tenant_data))
    }
}

/// The blocking item of required kind `k` of department `dept` (`None` = the job has no department) in state `st`,
/// if any: `knowledge_unbound` with slot `<dept|job>:<stage>|<kind>` when nothing resolves, else
/// `<dept|job>:<stage>|<kind>:<lang>` when the primary language `primary` is not covered.
pub fn knowledge_item(
    job: &str,
    dept: Option<&str>,
    k: &RequiredKind,
    st: &KindState,
    primary: Option<&str>,
) -> Option<MissingItem> {
    let scope = dept.unwrap_or("job");
    let of = match dept {
        Some(d) => format!("department {d}"),
        None => "the job".to_string(),
    };
    let href = job_path(job, "/knowledge-kinds");
    let what = format!(
        "Required knowledge kind {} ({} stage) of {of}",
        k.kind,
        stage_label(&k.stage)
    );
    if !st.resolvable(k.tenant_data) {
        let detail = if k.tenant_data {
            format!(
                "{what} is your own data and nothing of yours feeds it: bind it to a store your source or \
                 connector fills, or upload to it (platform packs alone do not count)."
            )
        } else {
            format!("{what} has no knowledge: bind it to a store or pin a platform pack.")
        };
        return Some(MissingItem {
            dependency: KNOWLEDGE_DEPENDENCY.to_string(),
            slot: format!("{scope}:{}|{}", k.stage, k.kind),
            detail,
            href,
        });
    }
    let lang = primary?;
    if st.primary_covered(k.tenant_data) {
        return None;
    }
    Some(MissingItem {
        dependency: KNOWLEDGE_DEPENDENCY.to_string(),
        slot: format!("{scope}:{}|{}:{lang}", k.stage, k.kind),
        detail: format!("{what} has no documents in {lang}, the job's primary language."),
        href,
    })
}

/// The pins the design checked was built from: a release's own (`job_release.pins`), live = the current release's
/// (the job's pins before any release), the draft = the job's (`jobs.pins`).
async fn design_pins(
    c: &mut PgConnection,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<Vec<TemplatePin>> {
    let release: Option<Value> = match of {
        DesignOf::Release(id) => {
            sqlx::query_scalar("SELECT pins FROM job_release WHERE job_id = $1 AND release_id = $2")
                .bind(job.id)
                .bind(id)
                .fetch_optional(&mut *c)
                .await?
        }
        DesignOf::Live => sqlx::query_scalar(
            "SELECT r.pins FROM jobs j JOIN job_release r ON r.job_id = j.id AND r.release_id = j.current_release_id \
              WHERE j.id = $1",
        )
        .bind(job.id)
        .fetch_optional(&mut *c)
        .await?,
        DesignOf::Draft => None,
    };
    let pins = match release {
        Some(p) => p,
        None => {
            sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1")
                .bind(job.id)
                .fetch_one(&mut *c)
                .await?
        }
    };
    serde_json::from_value(pins).map_err(|e| {
        crate::error::ApiError::Internal(anyhow::anyhow!(
            "the pins of {} are unreadable: {e}",
            job.key
        ))
    })
}

/// The required kinds of the design checked, by `(stage, kind)`: the template fold of its pins under the selection
/// checked (D107/D115, with each kind's departments), plus every required working kind the fold does not have
/// (inherited by every department). `required` / `tenant_data` are the fold's, or the working row's.
async fn required_kinds(
    c: &mut PgConnection,
    cx: &ReadinessCx<'_>,
    views: &[KindView],
) -> ApiResult<Vec<RequiredKind>> {
    let folded = match cx.selection {
        Some(sel) if hierarchical(sel) => {
            let pins = design_pins(&mut *c, cx.job, &cx.design).await?;
            template_packs::knowledge(&mut *c, &pins, sel).await?.kinds
        }
        _ => Vec::new(),
    };
    let mut out: BTreeMap<(String, String), RequiredKind> = BTreeMap::new();
    for f in &folded {
        let row = views
            .iter()
            .find(|v| v.stage == f.stage && v.kind == f.kind);
        if !(f.required || row.is_some_and(|v| v.required)) {
            continue;
        }
        out.insert(
            (f.stage.clone(), f.kind.clone()),
            RequiredKind {
                stage: f.stage.clone(),
                kind: f.kind.clone(),
                tenant_data: f.tenant_data,
                description: f.description.clone(),
                departments: f.departments.clone(),
            },
        );
    }
    for v in views.iter().filter(|v| v.required) {
        out.entry((v.stage.clone(), v.kind.clone()))
            .or_insert_with(|| RequiredKind {
                stage: v.stage.clone(),
                kind: v.kind.clone(),
                tenant_data: v.tenant_data,
                description: v.description.clone(),
                departments: Vec::new(),
            });
    }
    Ok(out.into_values().collect())
}

/// A selection the template fold can run on: an industry and at least one department (else the hierarchy item
/// blocks, crate::readiness::hierarchy_missing).
fn hierarchical(sel: &Selection) -> bool {
    !sel.industry.is_empty() && !sel.departments.is_empty()
}

/// What resolves `k` in the design checked: its own binding and (D107) the `*|<kind>` binding, each through
/// [`kinds::leaf_set`]; a place without a value resolves nothing.
async fn kind_state(
    c: &mut PgConnection,
    cx: &ReadinessCx<'_>,
    views: &[KindView],
    k: &RequiredKind,
    primary: Option<&str>,
) -> ApiResult<KindState> {
    let mut st = KindState::default();
    let mut stages = vec![k.stage.as_str()];
    if k.stage != ANY_STAGE {
        stages.push(ANY_STAGE);
    }
    for stage in stages {
        if !views.iter().any(|v| v.stage == stage && v.kind == k.kind) {
            continue;
        }
        let ls = kinds::leaf_set(&mut *c, cx.reg, cx.job, stage, &k.kind, &cx.design).await?;
        st.packs |= ls.packs.iter().any(|p| p.live());
        if let Some(leaf) = ls.tenant_store {
            st.tenant = true;
            let langs = coverage::leaf_languages(&mut *c, leaf).await?;
            st.tenant_fed |= !langs.is_empty();
            st.tenant_primary |= primary.is_some_and(|l| langs.contains(l));
        }
    }
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_slot_parses_only_its_own_service() {
        assert_eq!(
            parse_required("strata_rag.embed"),
            Some(("strata_rag", "embed", Slot::Embed))
        );
        assert_eq!(
            parse_required("strata_ingest.vision").map(|x| x.2),
            Some(Slot::Vision)
        );
        assert!(parse_required("strata_ingest.embed").is_none());
        assert!(parse_required("strata_rag.nope").is_none());
        assert!(parse_required("embed").is_none());
    }

    #[test]
    fn missing_slot_links_the_service_tab() {
        let m = missing_slot("strata_rag.embed");
        assert_eq!(
            (m.dependency.as_str(), m.slot.as_str()),
            ("strata_rag", "embed")
        );
        assert_eq!(m.href, "/admin/providers?tab=knowledge");
        assert_eq!(m.detail, "W5 slot strata_rag.embed has no deployment");
        assert_eq!(
            missing_slot("strata_ingest.ocr").href,
            "/admin/providers?tab=ingest"
        );
    }

    #[test]
    fn stage_label_capitalises() {
        assert_eq!(stage_label("support"), "Support");
        assert_eq!(stage_label(""), "");
    }

    #[test]
    fn pack_state_names_the_reason() {
        let base = kinds::CachedPack {
            pack_key: "industry/retail".into(),
            version: 1,
            root_id: uuid::Uuid::nil(),
            published_at: None,
            listed: true,
            checked_at: chrono::Utc::now(),
            unreadable_at: None,
            state: Some("published".into()),
            excluded_by: None,
        };
        let state = |f: &dyn Fn(&mut kinds::CachedPack)| {
            let mut p = base.clone();
            f(&mut p);
            let cache: BTreeMap<String, CachedPack> = [(p.pin(), p)].into_iter().collect();
            pack_state(&cache, "industry/retail@1")
        };
        assert_eq!(state(&|_| {}), None);
        assert_eq!(
            state(&|p| p.excluded_by = Some("industry".into())).as_deref(),
            Some("is excluded for this tenant (platform exclusion industry)")
        );
        assert_eq!(
            state(&|p| p.unreadable_at = Some(chrono::Utc::now())).as_deref(),
            Some("could not be read")
        );
        assert_eq!(
            state(&|p| p.state = Some("missing".into())).as_deref(),
            Some("is missing from the platform")
        );
        assert_eq!(
            state(&|p| p.state = Some("draft".into())).as_deref(),
            Some("is not published yet")
        );
        assert_eq!(
            state(&|p| {
                p.state = Some("retired".into());
                p.listed = false;
            })
            .as_deref(),
            Some("is retired")
        );
        assert_eq!(
            state(&|p| p.listed = false).as_deref(),
            Some("is no longer listed")
        );
        assert_eq!(
            pack_state(&BTreeMap::new(), "base@1").as_deref(),
            Some("was never listed for this tenant")
        );
    }
}
