//! Knowledge kinds of a job, per-language coverage and the platform packs (B3 T10; plan §5.5, §5.4 `knowledgeCoverage`,
//! §0 R10/R11; D54, D68). Shapes = `web/src/lib/api/types/ontology.ts` (`KnowledgeKindRow`, `KnowledgeStoreOption`)
//! and `types/tenant.ts` (`KnowledgeCoverage`), widened by R10 (`packs`); refusals = the fixture
//! (`web/src/lib/api/areas/ontology.ts` `bindKnowledgeKind`).
//!
//! - `GET /api/jobs/{job}/knowledge-kinds` (member) → `{rows, stores, not_applicable, inherited}` (`inherited`: every
//!   platform pack reference the job's template folds, `{key, stage, kind, pin, layer, applies}`): `not_applicable` (D107, KH
//!   T3) = the template pack references that match none of the job's kinds (`{key, stage, kind, pin, layer, code:
//!   "pack_not_applicable"}`, listed, never an error); each row carries `tenant_data` (D110). The job's kinds (working rows) with their
//!   pins `packs[{pin, listed, unreadable, newer, state, excluded_by, tombstoned, leaves}]` (T5), and the tenant's leaves by path with strata's document count
//!   (`limit=500`, at most 500) in the job's pinned DRAFT generation (T4a, as search and coverage). The platform-pack listing is refreshed into `platform_pack` on the way (R10). strata_rag
//!   unreadable → rows from the cache and counts 0, header `X-Agent-Stale: strata_rag`.
//! - `PUT /api/jobs/{job}/knowledge-kinds/{stage}/{kind}` `{store?, packs?}` (job_designer) → the row. A draft write
//!   (`design::track`, kind `knowledge`): `store` absent = keep, `null` = unbind the tenant leaf; `packs` absent =
//!   keep, `[]` = no packs. 404 `kind_not_found`; 422 `unknown_store` (not a leaf path of the tree), `invalid_pack`
//!   (not `key@version`, more than 16, a key twice), `unknown_pack` (a NEW pin not in the tenant's listing; a pin
//!   already on the row stays accepted, R10); `packs` given and strata_rag unreachable → its error (503
//!   `unavailable`). No lock is held across the listing call.
//! - `GET /api/knowledge/platform-packs` (member) → `[{pin, key, version, root_id, published_at}]`: the packs the
//!   tenant may pin (rag 404 = platform knowledge off = `[]`), refreshed into the cache; strata_rag down → 503.
//! - `GET /api/jobs/{job}/knowledge/coverage` (member) → `KnowledgeCoverage` over the job's DRAFT languages: the
//!   documents of each kind's bound leaf from strata_rag, read in the job's pinned DRAFT generation (T4a; language
//!   from the upload, then document metadata, then `und`), R10 `platform` cells for kinds with a listed pack; strata_rag unreadable → the uploaded files (local)
//!   and `X-Agent-Stale: strata_rag`.
use std::collections::{BTreeMap, HashMap};

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::design::{self, DesignOf, DraftWrite, JobRef, Label};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::knowledge::coverage::{self, CoverageDoc};
use crate::knowledge::generations;
use crate::knowledge::kinds::{self, CachedPack, KindRow, PackPin};
use crate::knowledge::packs::template as template_packs;
use crate::knowledge::stores::{self, StoreRow};
use crate::state::AppState;
use crate::strata::rag::{self, DocumentsQuery, PlatformPack};
use crate::strata::{self, Plane, Service, StrataStoreId};
use crate::tenancy::{JobCtx, JobMember, JobRequires, Member, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/knowledge-kinds", get(list))
        .route("/api/jobs/{job}/knowledge-kinds/{stage}/{kind}", put(bind))
        .route("/api/jobs/{job}/knowledge/coverage", get(coverage_view))
        .route("/api/knowledge/platform-packs", get(platform_packs))
}

/// Set when strata_rag could not be read (the answer is built from local state).
pub const STALE_HEADER: &str = "x-agent-stale";
/// Documents counted per leaf (one `GET …/documents?limit=500`).
const DOC_CAP: i64 = 500;
/// A document whose language no upload or metadata names.
const UNKNOWN_LANGUAGE: &str = "und";

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

fn stale(body: Value, is_stale: bool) -> Response {
    if is_stale {
        ([(STALE_HEADER, "strata_rag")], Json(body)).into_response()
    } else {
        Json(body).into_response()
    }
}

/// Serialise with the job's other design writes before reading what the write depends on (`design::track` takes the
/// same lock again).
async fn lock_job(c: &mut PgConnection, job: Uuid) -> ApiResult<()> {
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job)
        .execute(&mut *c)
        .await?;
    Ok(())
}

/// `(key, version)` of every pinnable pack of a listing.
fn listed_versions(listing: &[PlatformPack]) -> Vec<(String, i32)> {
    kinds::pinnable(listing)
        .map(|p| (p.key.clone(), p.version))
        .collect()
}

/// What a pin view reads besides the row (T5): the pack cache, the cached leaf counts and the job's
/// `knowledge_pack` refinement tombstones.
struct PinCtx {
    cache: Vec<CachedPack>,
    leaves: BTreeMap<String, i64>,
    tombstones: Vec<String>,
}

async fn pin_ctx(c: &mut PgConnection, job: Uuid) -> ApiResult<PinCtx> {
    Ok(PinCtx {
        cache: kinds::cached_packs(&mut *c).await?,
        leaves: kinds::cached_leaf_counts(&mut *c).await?,
        tombstones: template_packs::job_tombstones(&mut *c, job).await?,
    })
}

/// A job tombstone key (`stage|kind|pack_key`, stage and kind may be `*`) removes `pack_key` from `stage` / `kind`.
fn tombstoned(keys: &[String], stage: &str, kind: &str, pack_key: &str) -> bool {
    keys.iter().any(|k| {
        let mut parts = k.splitn(3, '|');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(s), Some(kd), Some(p)) => {
                p == pack_key && (s == "*" || s == stage) && (kd == "*" || kd == kind)
            }
            _ => false,
        }
    })
}

/// `KnowledgeKindRow` + R10 `packs` (`{pin, listed, unreadable, newer}`) + T5 `{state, excluded_by, tombstoned,
/// leaves}`: the owner's classification, the tenant exclusion hiding it, whether the job's refinements tombstone the
/// pack for this kind, and the number of cached leaves.
fn row_json(tenant: Uuid, job: &str, k: &KindRow, cx: &PinCtx, listed: &[(String, i32)]) -> Value {
    let packs: Vec<Value> = k
        .packs
        .iter()
        .map(|pin| {
            let c = cx.cache.iter().find(|c| c.pin() == *pin);
            let key = kinds::parse_pin(pin).map(|p| p.key).unwrap_or_default();
            json!({
                "pin": pin,
                "listed": c.is_some_and(|c| c.listed),
                "unreadable": c.is_some_and(|c| c.unreadable_at.is_some()),
                "newer": kinds::newer_of(pin, listed),
                "state": c.and_then(|c| c.state.clone()),
                "excluded_by": c.and_then(|c| c.excluded_by.clone()),
                "tombstoned": tombstoned(&cx.tombstones, &k.stage, &k.kind, &key),
                "leaves": cx.leaves.get(pin).copied().unwrap_or(0),
            })
        })
        .collect();
    json!({
        "tenant_id": tenant,
        "job": job,
        "stage": k.stage,
        "kind": k.kind,
        "required": k.required,
        "tenant_data": k.tenant_data,
        "description": k.description,
        "store": k.store_path,
        "origin": k.origin,
        "packs": packs,
        "packs_source": k.packs_source,
        "updated_at": k.updated_at,
    })
}

/// The cache's listed pinnable versions (used when no fresh listing is at hand).
fn cached_versions(cache: &[CachedPack]) -> Vec<(String, i32)> {
    cache
        .iter()
        .filter(|c| c.listed)
        .map(|c| (c.pack_key.clone(), c.version))
        .collect()
}

// ---------------------------------------------------------------------------------------------------------------
// knowledgeKinds

/// strata's document count per leaf (agent store id) in `leaves` (agent store id, its strata store in the job's
/// pinned generation); `true` = something could not be read.
async fn leaf_counts(
    st: &AppState,
    p: &Plane<'_>,
    leaves: &[(Uuid, StrataStoreId)],
) -> (HashMap<Uuid, i64>, bool) {
    let mut out = HashMap::new();
    let mut failed = false;
    let q = DocumentsQuery {
        limit: Some(DOC_CAP),
        cursor: None,
        source_id: None,
    };
    for &(store_id, sid) in leaves {
        match rag::documents(&st.http, p, sid, &q).await {
            Ok(page) => {
                let n = i64::try_from(page.items.len())
                    .unwrap_or(DOC_CAP)
                    .min(DOC_CAP);
                out.insert(store_id, n);
            }
            Err(_) => failed = true,
        }
    }
    (out, failed)
}

/// Every leaf of the tree with its strata store in the job's pinned DRAFT generation (T4a): what the draft kinds
/// search and coverage read. A leaf unbound there is left out.
async fn pinned_leaves(
    c: &mut PgConnection,
    job: &JobRef,
    all: &[StoreRow],
) -> ApiResult<Vec<(Uuid, StrataStoreId)>> {
    let mut out = Vec::new();
    for s in all.iter().filter(|s| s.is_leaf()) {
        if let Some(sid) = generations::strata_for(&mut *c, job, s.id, &DesignOf::Draft).await? {
            out.push((s.id, sid));
        }
    }
    Ok(out)
}

/// `knowledgeKinds`.
async fn list(JobMember(j): JobMember, State(st): State<AppState>) -> ApiResult<Response> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    let rows = kinds::rows(&mut tx, jr.id).await?;
    // D107: template packs that reach none of the job's kinds — listed, never an error. `inherited`: every platform
    // pack the job's template gives it, with the layer that set it (base → industry → department).
    let knowledge = kinds::template_knowledge(&mut tx, &jr)
        .await?
        .unwrap_or_default();
    let not_applicable: Vec<Value> = knowledge
        .not_applicable
        .iter()
        .map(|p| {
            json!({
                "key": p.key(),
                "stage": p.stage,
                "kind": p.kind,
                "pin": p.pin(),
                "layer": p.layer,
                "code": "pack_not_applicable",
            })
        })
        .collect();
    let inherited: Vec<Value> = knowledge
        .packs
        .iter()
        .map(|p| {
            let applies = !knowledge
                .not_applicable
                .iter()
                .any(|n| n.key() == p.key() && n.layer == p.layer);
            json!({
                "key": p.key(),
                "stage": p.stage,
                "kind": p.kind,
                "pin": p.pin(),
                "layer": p.layer,
                "applies": applies,
            })
        })
        .collect();
    let all = stores::all(&mut tx).await?;
    let leaves = pinned_leaves(&mut tx, &jr, &all).await?;
    tx.commit().await?;
    // config, not the provisioning gate: a store being created must not hide everything else
    let plane = strata::plane(&st, j.t.tenant, Service::Rag).await;
    let (counts, listing, is_stale) = match &plane {
        Ok(p) => {
            let (counts, failed) = leaf_counts(&st, p, &leaves).await;
            match rag::platform_packs(&st.http, p).await {
                Ok(l) => (counts, Some(l), failed),
                Err(_) => (counts, None, true),
            }
        }
        Err(_) => (HashMap::new(), None, true),
    };
    let mut tx = j.begin(&st.db).await?;
    if let Some(l) = &listing {
        kinds::record_listing(&mut tx, j.t.tenant, l).await?;
    }
    let cx = pin_ctx(&mut tx, jr.id).await?;
    tx.commit().await?;
    let listed = match &listing {
        Some(l) => listed_versions(l),
        None => cached_versions(&cx.cache),
    };
    let items: Vec<Value> = rows
        .iter()
        .map(|k| row_json(j.t.tenant, &jr.key, k, &cx, &listed))
        .collect();
    // leaves by path (paths are unique: keys are unique per tenant)
    let options: BTreeMap<String, Value> = all
        .iter()
        .filter(|s| s.is_leaf())
        .map(|s| {
            let path = stores::path_of(&all, s.id).join("/");
            let v = json!({
                "path": path,
                "label": s.label,
                "documents": counts.get(&s.id).copied().unwrap_or(0),
                "updated_at": s.updated_at,
            });
            (path, v)
        })
        .collect();
    let stores: Vec<Value> = options.into_values().collect();
    Ok(stale(
        json!({"rows": items, "stores": stores, "not_applicable": not_applicable, "inherited": inherited}),
        is_stale,
    ))
}

// ---------------------------------------------------------------------------------------------------------------
// bindKnowledgeKind

/// `Some(v)` whenever the field is present (JSON `null` included); absent → `None` (via `#[serde(default)]`).
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindInput {
    #[serde(default, deserialize_with = "present")]
    store: Option<Value>,
    #[serde(default)]
    packs: Option<Vec<String>>,
}

/// `bindKnowledgeKind`.
async fn bind(
    JobRequires(j, _): JobRequires<role::JobDesigner>,
    State(st): State<AppState>,
    ApiPath((_job, stage, kind)): ApiPath<(String, String, String)>,
    ApiJson(input): ApiJson<BindInput>,
) -> ApiResult<Json<Value>> {
    let store: Option<Option<String>> = match input.store {
        None => None,
        Some(Value::Null) => Some(None),
        Some(Value::String(s)) => Some(Some(s.trim().to_string())),
        Some(_) => {
            return Err(ApiError::BadRequest(
                "invalid_body",
                "store is a store path or null.".into(),
            ));
        }
    };
    let pins: Option<Vec<String>> = input
        .packs
        .as_deref()
        .map(kinds::check_pins)
        .transpose()?
        .map(|ps| ps.iter().map(PackPin::pin).collect());
    let jr = job_ref(&j);
    let tenant = j.t.tenant;
    // the tenant's listing, read before any lock is taken (never held across the call)
    let listing = match &pins {
        None => None,
        Some(_) => {
            let mut tx = j.begin(&st.db).await?;
            kinds::row(&mut tx, jr.id, &stage, &kind)
                .await?
                .ok_or_else(|| kinds::kind_not_found(&stage, &kind))?;
            let p = strata::gate(&st, &mut tx, tenant, Service::Rag).await?;
            tx.commit().await?;
            Some(rag::platform_packs(&st.http, &p).await?)
        }
    };
    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, &jr).await?;
    lock_job(&mut tx, jr.id).await?;
    if let Some(l) = &listing {
        kinds::record_listing(&mut tx, tenant, l).await?;
    }
    let current = kinds::row(&mut tx, jr.id, &stage, &kind)
        .await?
        .ok_or_else(|| kinds::kind_not_found(&stage, &kind))?;
    if let Some(Some(path)) = &store {
        let all = stores::all(&mut tx).await?;
        if kinds::leaf_by_path(&all, path).is_none() {
            return Err(ApiError::Unprocessable(
                "unknown_store",
                format!("No knowledge store {path}."),
            ));
        }
    }
    if let (Some(pins), Some(l)) = (&pins, &listing) {
        let listed = listed_versions(l);
        for pin in pins {
            let ok = current.packs.contains(pin)
                || kinds::parse_pin(pin)
                    .is_some_and(|p| listed.iter().any(|(k, v)| *k == p.key && *v == p.version));
            if !ok {
                return Err(ApiError::Unprocessable(
                    "unknown_pack",
                    format!("No platform pack {pin} you can pin."),
                ));
            }
        }
    }
    let after = kinds::kind_value(
        store.clone().unwrap_or(current.store_path).as_deref(),
        pins.as_deref().unwrap_or(&current.packs),
    );
    let text = kinds::value_text(&Some(after));
    let label = if text == "unbound" {
        format!("{kind} ({stage}) unbound")
    } else {
        format!("{kind} ({stage}) bound to {text}")
    };
    let (job_id, stg, kd, by) = (jr.id, stage.clone(), kind.clone(), j.t.caller.sub.clone());
    let w = DraftWrite {
        area: "knowledge",
        page: "J10",
        kind: kinds::KIND,
        r#ref: kinds::kind_ref(&stage, &kind),
        label: Label::Now(label),
    };
    design::track::<(), _>(&mut tx, &st.design, &j, w, move |c| {
        Box::pin(async move { kinds::bind_row(c, job_id, &stg, &kd, store, pins, &by).await })
    })
    .await?;
    let row = kinds::row(&mut tx, jr.id, &stage, &kind)
        .await?
        .ok_or_else(|| kinds::kind_not_found(&stage, &kind))?;
    let cx = pin_ctx(&mut tx, jr.id).await?;
    tx.commit().await?;
    let listed = match &listing {
        Some(l) => listed_versions(l),
        None => cached_versions(&cx.cache),
    };
    Ok(Json(row_json(tenant, &jr.key, &row, &cx, &listed)))
}

// ---------------------------------------------------------------------------------------------------------------
// platformPacks

/// `platformPacks` (R10).
async fn platform_packs(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let listing = kinds::refresh_packs(&st, t.tenant).await?;
    let items: Vec<Value> = kinds::pinnable(&listing)
        .map(|p| {
            json!({
                "pin": format!("{}@{}", p.key, p.version),
                "key": p.key,
                "version": p.version,
                "root_id": p.root_id,
                "published_at": p.published_at,
            })
        })
        .collect();
    Ok(Json(Value::Array(items)))
}

// ---------------------------------------------------------------------------------------------------------------
// knowledgeCoverage

/// The language of a strata document: its upload's, else its metadata's, else `und` (G5, as `knowledgeDocuments`).
fn doc_language(d: &rag::Document, uploads: &HashMap<Uuid, String>) -> String {
    let upload = d
        .source_id
        .as_deref()
        .and_then(|s| s.strip_prefix("upload:"))
        .and_then(|u| Uuid::parse_str(u).ok())
        .and_then(|u| uploads.get(&u));
    match upload {
        Some(l) => l.clone(),
        None => d
            .metadata
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or(UNKNOWN_LANGUAGE)
            .to_string(),
    }
}

/// strata's documents of `leaves` (agent store id, its strata store in the job's pinned generation); `None` when
/// any could not be read.
async fn strata_docs(
    st: &AppState,
    p: &Plane<'_>,
    leaves: &[(Uuid, StrataStoreId)],
    uploads: &HashMap<Uuid, String>,
) -> Option<Vec<CoverageDoc>> {
    let q = DocumentsQuery {
        limit: Some(DOC_CAP),
        cursor: None,
        source_id: None,
    };
    let mut out = Vec::new();
    for &(store_id, sid) in leaves {
        let page = rag::documents(&st.http, p, sid, &q).await.ok()?;
        out.extend(page.items.iter().map(|d| CoverageDoc {
            id: d.id.to_string(),
            store_id,
            language: doc_language(d, uploads),
        }));
    }
    Some(out)
}

/// `knowledgeCoverage`.
async fn coverage_view(JobMember(j): JobMember, State(st): State<AppState>) -> ApiResult<Response> {
    let jr = job_ref(&j);
    let mut tx = j.begin(&st.db).await?;
    let languages: Vec<String> = sqlx::query_scalar("SELECT languages FROM jobs WHERE id = $1")
        .bind(jr.id)
        .fetch_one(&mut *tx)
        .await?;
    let kinds_in = coverage::kinds_of(&mut tx, jr.id).await?;
    let all = stores::all(&mut tx).await?;
    let local = coverage::upload_docs(&mut tx).await?;
    let uploads: HashMap<Uuid, String> =
        sqlx::query_as::<_, (Uuid, String)>("SELECT id, language FROM knowledge_upload")
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .collect();
    // each bound leaf's strata store in the job's pinned DRAFT generation (T4a), as the draft kinds search it
    let mut leaves: Vec<(Uuid, StrataStoreId)> = Vec::new();
    for k in &kinds_in {
        let Some(id) = k
            .store
            .as_deref()
            .and_then(|p| kinds::leaf_by_path(&all, p))
            .map(|s| s.id)
        else {
            continue;
        };
        if leaves.iter().any(|(l, _)| *l == id) {
            continue;
        }
        if let Some(sid) = generations::strata_for(&mut tx, &jr, id, &DesignOf::Draft).await? {
            leaves.push((id, sid));
        }
    }
    tx.commit().await?;
    let (docs, is_stale) = if leaves.is_empty() {
        (local, false)
    } else {
        let fetched = match strata::plane(&st, j.t.tenant, Service::Rag).await {
            Ok(p) => strata_docs(&st, &p, &leaves, &uploads).await,
            Err(_) => None,
        };
        match fetched {
            Some(d) => (d, false),
            None => (local, true),
        }
    };
    let cov = coverage::coverage_of(&jr.key, &languages, &kinds_in, &all, &docs);
    Ok(stale(cov.json(), is_stale))
}
