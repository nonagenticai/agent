//! `/api/knowledge`: the tenant's store tree (live data, D69), its documents, the query playground and the model card
//! (B3 T5; plan §5.2, §7 G4/G5/G7, §0 R7/R10). Shapes = `web/src/lib/api/types/tenant.ts` (`KnowledgeStore`,
//! `KnowledgeDocument`, `KnowledgeQueryResult`) and `types/retrieval.ts` (`RetrievalModels`); refusals = the fixture
//! (`web/src/lib/api/areas/tenant.ts` T7, `areas/retrieval.ts`). Ported from care-bus-agents@96c608c
//! `routes/knowledge.rs`; the tree is served from agent's own table, not a strata pass-through.
//!
//! - `GET /api/knowledge/stores` (member) → `Paged<KnowledgeStore>`: the local tree with strata_rag's token/chunk
//!   counts and a document count per bound leaf (G4: `GET …/documents?limit=500`, at most 500, cached 60 s per
//!   store; a group sums its leaves). strata_rag down or not set up → the tree with counts 0 and
//!   `X-Agent-Stale: strata_rag`.
//! - `POST /api/knowledge/stores` (knowledge_manager) → 201 `KnowledgeStore` + outbox `strata_rag:store_ensure:<key>`.
//!   422 `invalid_key`, `invalid_store`, `parent_not_group`; 409 `store_exists`. `region` = the tenant's residency.
//! - `DELETE /api/knowledge/stores/{id}` (knowledge_manager) → 204. 404 `store_not_found`; 409 `store_not_empty`
//!   (local children, or strata `has_children`), `provisioning_in_flight`,
//!   `store_has_sources` (with `sources`: a source writes into it). A bound store is deleted in strata_rag
//!   first (no lock held across the call).
//! - `POST /api/knowledge/stores/ensure` (knowledge_manager) → `{enqueued, stores}`: one `store_ensure` per unbound
//!   store, parents first. 409 `provisioning_in_flight` while any store is being created.
//! - `GET /api/knowledge/stores/{id}/documents?before=` (member) → 25 per page (`before` = strata's opaque cursor,
//!   `next` passed back as is); a group or an unbound leaf answers an empty page. `language` (G5) from the upload
//!   (`upload:<id>` source ids), else document metadata, else `und`.
//! - `DELETE /api/knowledge/documents/{id}` (knowledge_manager) → 204; strata `not_found` → 404 `document_not_found`.
//! - `POST /api/knowledge/query` (member, stores nothing) → `KnowledgeQueryResult`. D91 / §0 R7: the root is resolved
//!   here, never by strata: the named store's root, else the tenant's only root; several roots and none named → 422
//!   `root_required` (no strata call). The request always carries `root_id`. PK T5: `{job, stage, kind}` search that
//!   kind's leaf set in the job's draft instead (`knowledge::kinds::search`, `leaf_ids`, no root and so never
//!   `root_required`; `degraded` lists the pack pins it ran without); naming only some of the three is 422
//!   `invalid_query`.
//! - `GET /api/knowledge/models` (member) → `RetrievalModels`: the working `embed` / `rerank` slots (`provider_slot`,
//!   T4), strata's `dims`, and the pinned chunking / query defaults (G6).
//! - `POST /api/knowledge/stores/{id}/upload` (knowledge_manager, multipart: `file` parts + optional `language`) →
//!   the first created `IngestJob` (T6, plan §5.2 `uploadToStore`): one strata_ingest job per file with
//!   `source_id = upload:<id>` and the upload's language (given, else the tenant's first job's first language), all
//!   recorded in `knowledge_upload`. Several files answer `source_label: "a.pdf + 2 more"`. 404 `store_not_found`; 422
//!   `not_a_leaf`, `no_file`, `too_many_files` (over 20), `invalid_language`; 409 `store_not_bound`; 413 `too_large`
//!   (over 50 MB in all); 415 `unsupported_type`. The files are read as they arrive and held at most up to that 50 MB
//!   cap (below strata_ingest's own per-file limit), then sent in one multipart request (30 s timeout). An optional
//!   `supersedes` field (PK-8, T6: a JSON array of platform topic keys, the source rules) is stored in
//!   `knowledge_upload.supersedes`, 422 `invalid_supersedes`; it is sent to strata_ingest (#15) as a `supersedes`
//!   text field before the first `file` part.
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use axum::extract::multipart::{MultipartError, MultipartRejection};
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{self, Event};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::knowledge::stores::{self, Counts, MAX_LABEL_CHARS, StoreRow};
use crate::providers::llm;
use crate::state::AppState;
use crate::strata::rag::{
    self, DocumentsQuery, Hit, Layer, QueryReq, QueryResp, QueryTarget, RouteMode,
};
use crate::strata::{self, Plane, Service, StrataStoreId, ingest};
use crate::tenancy::{self, Member, Requires, TenantCtx, role};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/knowledge/stores", get(list).post(create))
        // static, so it never reaches `/api/knowledge/stores/{id}` (which has no POST)
        .route("/api/knowledge/stores/ensure", post(ensure_missing))
        .route("/api/knowledge/stores/{id}", delete(remove))
        .route("/api/knowledge/stores/{id}/documents", get(documents))
        .route(
            "/api/knowledge/stores/{id}/upload",
            post(upload).layer(DefaultBodyLimit::max(UPLOAD_BODY_LIMIT)),
        )
        .route("/api/knowledge/documents/{id}", delete(delete_document))
        .route("/api/knowledge/query", post(query))
        .route("/api/knowledge/models", get(models))
}

/// Set when the counts could not be read from strata_rag (the list is the local tree with counts 0).
pub const STALE_HEADER: &str = "x-agent-stale";
/// Documents per page (fixture `paged(docs, {limit: 25})`).
const PAGE: i64 = 25;
/// G4: documents counted per leaf (one `GET …/documents?limit=500`); more shows as 500.
const DOC_COUNT_CAP: i64 = 500;
/// G4: how long one leaf's document count is reused.
const DOC_COUNT_TTL: Duration = Duration::from_secs(60);
/// A document whose language no upload or metadata names (BCP-47 "undetermined").
const UNKNOWN_LANGUAGE: &str = "und";
/// G6: strata_rag's defaults at `7c2f1ad` (`src/config.rs` STRATA_CHUNK_TARGET / _OVERLAP / _RERANK_CANDIDATES,
/// `src/query.rs` d_top_k / d_beam / d_min_score, `RouteMode::default()`); re-pin when strata changes them.
const CHUNK_TARGET_TOKENS: i64 = 384;
const CHUNK_OVERLAP_TOKENS: i64 = 48;
const RERANK_CANDIDATES: i64 = 8;
const DEFAULT_TOP_K: i64 = 8;
const DEFAULT_BEAM: i64 = 3;
const DEFAULT_MIN_SCORE: f64 = 0.35;
/// T6 direct upload limits (fixture `UPLOAD_MAX_FILES`, `UPLOAD_MAX_BYTES`, `UPLOAD_TYPES`).
pub(crate) const UPLOAD_MAX_FILES: usize = 20;
pub(crate) const UPLOAD_MAX_BYTES: usize = 50 * 1024 * 1024;
pub(crate) const UPLOAD_TYPES: [&str; 7] = ["pdf", "docx", "txt", "md", "html", "csv", "xlsx"];
/// axum's cap on the whole upload body: the files plus room for the small fields.
pub(crate) const UPLOAD_BODY_LIMIT: usize = UPLOAD_MAX_BYTES + 1024 * 1024;
/// An upload's language when none is given and the tenant has no job yet.
const DEFAULT_LANGUAGE: &str = "en";

// ---------------------------------------------------------------------------------------------------------------
// Inputs and refusals

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreInput {
    key: String,
    label: String,
    #[serde(default)]
    parent_id: Option<Uuid>,
    kind: String,
}

#[derive(Deserialize)]
struct DocsQuery {
    #[serde(default)]
    before: Option<String>,
}

#[derive(Deserialize)]
struct QueryInput {
    query: String,
    mode: RouteMode,
    #[serde(default)]
    top_k: Option<usize>,
    #[serde(default)]
    min_score: Option<f32>,
    /// Laya branches; not sent in embed mode.
    #[serde(default)]
    beam: Option<usize>,
    /// Search the tree of this store (agent id); absent = the tenant's only root.
    #[serde(default)]
    store_id: Option<Uuid>,
    /// T4a (§0 R7): search as this job's Test environment does, in the generation its draft pins.
    #[serde(default)]
    job: Option<String>,
    /// PK T5: with `job` and `kind`, search that knowledge kind's leaf set in the job's draft (`kinds::search`: its
    /// tenant leaf plus pinned pack leaves, `leaf_ids`; no root, so never `root_required`).
    #[serde(default)]
    stage: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

fn invalid_store(msg: &str) -> ApiError {
    ApiError::Unprocessable("invalid_store", msg.into())
}

fn store_not_empty(key: &str) -> ApiError {
    ApiError::Conflict(
        "store_not_empty",
        format!("{key} still holds stores; delete those first."),
    )
}

fn in_flight(key: &str) -> ApiError {
    ApiError::Conflict(
        "provisioning_in_flight",
        format!("{key} is being created in strata_rag; wait for it to finish."),
    )
}

// ---------------------------------------------------------------------------------------------------------------
// G4: per-leaf document counts, cached per (tenant, strata store)

type CountCache = HashMap<(Uuid, StrataStoreId), (Instant, i64)>;

static DOC_COUNTS: LazyLock<Mutex<CountCache>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn cached_count(tenant: Uuid, sid: StrataStoreId) -> Option<i64> {
    let g = DOC_COUNTS.lock().unwrap_or_else(PoisonError::into_inner);
    g.get(&(tenant, sid))
        .filter(|(at, _)| at.elapsed() < DOC_COUNT_TTL)
        .map(|(_, n)| *n)
}

/// Drops the tenant's cached counts (after a document delete).
fn forget_counts(tenant: Uuid) {
    DOC_COUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .retain(|(t, _), _| *t != tenant);
}

async fn doc_count(
    st: &AppState,
    p: &Plane<'_>,
    tenant: Uuid,
    sid: StrataStoreId,
) -> ApiResult<i64> {
    if let Some(n) = cached_count(tenant, sid) {
        return Ok(n);
    }
    let q = DocumentsQuery {
        limit: Some(DOC_COUNT_CAP),
        cursor: None,
        source_id: None,
    };
    let page = rag::documents(&st.http, p, sid, &q).await?;
    let n = i64::try_from(page.items.len())
        .unwrap_or(DOC_COUNT_CAP)
        .min(DOC_COUNT_CAP);
    DOC_COUNTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert((tenant, sid), (Instant::now(), n));
    Ok(n)
}

/// strata's counts per agent store id; `true` = something could not be read.
async fn counts(
    st: &AppState,
    p: &Plane<'_>,
    tenant: Uuid,
    all: &[StoreRow],
) -> (HashMap<Uuid, Counts>, bool) {
    let mut out: HashMap<Uuid, Counts> = HashMap::new();
    let views = match rag::stores(&st.http, p).await {
        Ok(v) => v,
        Err(_) => return (out, true),
    };
    let by_sid: HashMap<StrataStoreId, &rag::StoreView> = views.iter().map(|v| (v.id, v)).collect();
    let by_id: HashMap<Uuid, &StoreRow> = all.iter().map(|s| (s.id, s)).collect();
    let mut stale = false;
    let mut leaf_docs: Vec<(Uuid, i64)> = Vec::new();
    for s in all {
        let Some(v) = s.strata().and_then(|sid| by_sid.get(&sid)) else {
            continue;
        };
        out.insert(
            s.id,
            Counts {
                documents: 0,
                chunks: v.chunk_count,
                tokens: v.token_count,
            },
        );
        if s.is_leaf() {
            match doc_count(st, p, tenant, v.id).await {
                Ok(n) => leaf_docs.push((s.id, n)),
                Err(_) => stale = true,
            }
        }
    }
    // a leaf's documents count for it and every group above it
    for (leaf, n) in leaf_docs {
        let mut cur = Some(leaf);
        let mut hops = 0;
        while let Some(id) = cur {
            out.entry(id).or_default().documents += n;
            cur = by_id.get(&id).and_then(|s| s.parent_id);
            hops += 1;
            if hops > all.len() {
                break;
            }
        }
    }
    (out, stale)
}

// ---------------------------------------------------------------------------------------------------------------
// Stores

/// `knowledgeStores`.
async fn list(Member(t): Member, State(st): State<AppState>) -> ApiResult<Response> {
    let mut tx = t.begin(&st.db).await?;
    let all = stores::all(&mut tx).await?;
    tx.commit().await?;
    // counts only: a store being created (an open outbox row) must not hide the others' counts, so config, not the
    // provisioning gate
    let plane = if all.iter().any(|s| s.strata_store_id.is_some()) {
        Some(strata::plane(&st, t.tenant, Service::Rag).await)
    } else {
        None
    };
    let (by_store, stale) = match plane {
        None => (HashMap::new(), false),
        Some(Ok(p)) => counts(&st, &p, t.tenant, &all).await,
        Some(Err(_)) => (HashMap::new(), true),
    };
    let items: Vec<Value> = all
        .iter()
        .map(|s| s.json(by_store.get(&s.id).copied().unwrap_or_default()))
        .collect();
    let body = Json(json!({"items": items, "next": null}));
    if stale {
        Ok(([(STALE_HEADER, "strata_rag")], body).into_response())
    } else {
        Ok(body.into_response())
    }
}

/// `createKnowledgeStore`: the row (region = the tenant's residency) and its `store_ensure`, in one transaction.
async fn create(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiJson(input): ApiJson<StoreInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let key = input.key.trim().to_string();
    if !stores::valid_key(&key) {
        return Err(ApiError::Unprocessable(
            "invalid_key",
            "A store key is lowercase letters, digits, dots, dashes or underscores, e.g. policy.gift_cards."
                .into(),
        ));
    }
    let label = input.label.trim().to_string();
    if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
        return Err(invalid_store(
            "Give the store a label of at most 200 characters.",
        ));
    }
    if !matches!(input.kind.as_str(), "group" | "leaf") {
        return Err(invalid_store("A store is a group or a leaf."));
    }
    let mut tx = t.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, t.tenant).await?;
    let taken: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM knowledge_store WHERE key = $1)")
            .bind(&key)
            .fetch_one(&mut *tx)
            .await?;
    if taken {
        return Err(store_exists(&key));
    }
    if let Some(pid) = input.parent_id {
        match stores::by_id(&mut tx, pid, true).await? {
            Some(p) if p.kind == "group" => {}
            _ => {
                return Err(ApiError::Unprocessable(
                    "parent_not_group",
                    "Pick a group store as the parent.".into(),
                ));
            }
        }
    }
    let region: String = sqlx::query_scalar("SELECT residency FROM tenants WHERE id = $1")
        .bind(t.tenant)
        .fetch_one(&mut *tx)
        .await?;
    let id = Uuid::new_v4();
    let inserted = sqlx::query(
        "INSERT INTO knowledge_store (id, key, label, parent_id, kind, region, updated_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(id)
    .bind(&key)
    .bind(&label)
    .bind(input.parent_id)
    .bind(&input.kind)
    .bind(&region)
    .bind(&t.caller.actor)
    .execute(&mut *tx)
    .await;
    if let Err(e) = inserted {
        let dup = e
            .as_database_error()
            .and_then(|d| d.code())
            .is_some_and(|c| c == "23505");
        return Err(if dup {
            store_exists(&key)
        } else {
            ApiError::from(e)
        });
    }
    stores::enqueue_ensure(&mut tx, t.tenant, id, &key, &t.actor()).await?;
    if input.parent_id.is_none() {
        // T4a (D83): a root store is born with its first generation, the tenant's current model set
        crate::knowledge::generations::ensure_initial_all(&mut tx, t.tenant, &t.caller.sub).await?;
    }
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("knowledge_store.create", "knowledge_store", key.clone())
            .detail("kind", input.kind.clone())
            .detail("parent", input.parent_id.map(|p| p.to_string())),
    )
    .await?;
    let row = stores::by_id(&mut tx, id, false)
        .await?
        .ok_or_else(stores::store_not_found)?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(row.json(Counts::default()))))
}

fn store_exists(key: &str) -> ApiError {
    ApiError::Conflict(
        "store_exists",
        format!("A store with the key {key} already exists."),
    )
}

/// The store `id` locked for delete, refused while it holds stores or is being created.
async fn deletable(c: &mut sqlx::PgConnection, id: Uuid) -> ApiResult<StoreRow> {
    let s = stores::by_id(c, id, true)
        .await?
        .ok_or_else(stores::store_not_found)?;
    let children: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM knowledge_store WHERE parent_id = $1)")
            .bind(id)
            .fetch_one(&mut *c)
            .await?;
    if children {
        return Err(store_not_empty(&s.key));
    }
    // a source of any job that writes into the store holds it (FK `source_store`): refused here, not a 500 (T8)
    let sources: i64 = sqlx::query_scalar("SELECT count(*) FROM source WHERE target_store_id = $1")
        .bind(id)
        .fetch_one(&mut *c)
        .await?;
    if sources > 0 {
        return Err(ApiError::Conflict(
            "store_has_sources",
            format!(
                "{} is the target of {sources} knowledge source(s); delete those sources or point them at \
                 another store first.",
                s.key
            ),
        )
        .with("sources", sources));
    }
    if s.ensuring {
        return Err(in_flight(&s.key));
    }
    Ok(s)
}

/// `deleteKnowledgeStore`: checked, deleted in strata_rag when bound, then deleted here, in ONE transaction that holds
/// the store row `FOR UPDATE` across the strata call. A source bound to the store meanwhile (`sources::validate`
/// takes it `FOR SHARE`) or a store added below it (its FK takes `FOR KEY SHARE`) waits for the delete and then finds
/// the store gone; one committed first is seen by the check, which refuses before strata is called. So a strata store
/// is never deleted while a local row still binds it. Only this row is held, never the tenant.
async fn remove(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = t.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, t.tenant).await?;
    let s = deletable(&mut tx, id).await?;
    if let Some(sid) = s.strata() {
        // the store exists in strata, so strata is set up for this tenant: config only, not the provisioning gate
        let p = strata::plane_in(&st, &mut tx, t.tenant, Service::Rag)
            .await?
            .with_actor(&t.caller.sub);
        match rag::delete_store(&st.http, &p, sid, false).await {
            Ok(()) => {}
            Err(e) if e.code() == "has_children" => return Err(store_not_empty(&s.key)),
            // already gone there
            Err(e) if e.code() == "not_found" => {}
            Err(e) => return Err(e),
        }
    }
    sqlx::query("DELETE FROM knowledge_store WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("knowledge_store.delete", "knowledge_store", s.key.clone())
            .detail("kind", s.kind.clone())
            .detail("bound", s.strata_store_id.is_some()),
    )
    .await?;
    tx.commit().await?;
    forget_counts(t.tenant);
    Ok(StatusCode::NO_CONTENT)
}

/// `ensureMissingStores`.
async fn ensure_missing(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    tenancy::lock_tenant_row(&mut tx, t.tenant).await?;
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM provision_outbox \
                         WHERE op = 'store_ensure' AND status IN ('pending', 'in_flight'))",
    )
    .fetch_one(&mut *tx)
    .await?;
    if busy {
        return Err(ApiError::Conflict(
            "provisioning_in_flight",
            "Stores are already being created in strata_rag. Check the Knowledge row on the overview.".into(),
        ));
    }
    let keys = stores::ensure_unbound(&mut tx, t.tenant, &t.actor()).await?;
    tx.commit().await?;
    Ok(Json(json!({"enqueued": keys.len(), "stores": keys})))
}

// ---------------------------------------------------------------------------------------------------------------
// Documents

/// What a document's `upload:<id>` source id links to (G5).
#[derive(sqlx::FromRow)]
struct UploadLink {
    id: Uuid,
    language: String,
    files: Value,
    ingest_job_ids: Vec<String>,
}

fn upload_of(d: &rag::Document) -> Option<Uuid> {
    d.source_id
        .as_deref()?
        .strip_prefix("upload:")
        .and_then(|u| Uuid::parse_str(u).ok())
}

async fn upload_links(
    st: &AppState,
    t: &TenantCtx,
    docs: &[rag::Document],
) -> ApiResult<HashMap<Uuid, UploadLink>> {
    let mut ids: Vec<Uuid> = docs.iter().filter_map(upload_of).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut tx = t.begin(&st.db).await?;
    let rows: Vec<UploadLink> = sqlx::query_as(
        "SELECT id, language, files, ingest_job_ids FROM knowledge_upload WHERE id = ANY($1)",
    )
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(rows.into_iter().map(|u| (u.id, u)).collect())
}

/// The ingest job of an uploaded document: the upload's only job, else the job of the file named like the document.
fn ingest_job_of(d: &rag::Document, u: &UploadLink) -> Option<String> {
    if let [only] = u.ingest_job_ids.as_slice() {
        return Some(only.clone());
    }
    u.files.as_array()?.iter().find_map(|f| {
        let name = f.get("name").and_then(Value::as_str)?;
        if name != d.source && d.title.as_deref() != Some(name) {
            return None;
        }
        f.get("ingest_job_id")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

/// `KnowledgeDocument`.
fn doc_json(d: &rag::Document, store: Uuid, uploads: &HashMap<Uuid, UploadLink>) -> Value {
    let up = upload_of(d).and_then(|u| uploads.get(&u));
    let language = up
        .map(|u| u.language.clone())
        .or_else(|| {
            d.metadata
                .get("language")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| UNKNOWN_LANGUAGE.to_string());
    json!({
        "id": d.id,
        "store_id": store,
        "title": d.title.clone().unwrap_or_else(|| d.source.clone()),
        "source": d.source,
        "tokens": d.token_count,
        "chunks": d.chunk_count,
        "added_at": d.created_at,
        "ingest_job_id": up.and_then(|u| ingest_job_of(d, u)),
        "language": language,
    })
}

/// `knowledgeDocuments`.
async fn documents(
    Member(t): Member,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(q): ApiQuery<DocsQuery>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let s = stores::by_id(&mut tx, id, false)
        .await?
        .ok_or_else(stores::store_not_found)?;
    let (Some(sid), true) = (s.strata(), s.is_leaf()) else {
        tx.commit().await?;
        return Ok(Json(json!({"items": [], "next": null})));
    };
    let p = strata::gate(&st, &mut tx, t.tenant, Service::Rag)
        .await?
        .with_actor(&t.caller.sub);
    tx.commit().await?;
    let dq = DocumentsQuery {
        limit: Some(PAGE),
        cursor: q.before.filter(|b| !b.is_empty()),
        source_id: None,
    };
    let page = rag::documents(&st.http, &p, sid, &dq).await?;
    let uploads = upload_links(&st, &t, &page.items).await?;
    let items: Vec<Value> = page
        .items
        .iter()
        .map(|d| doc_json(d, s.id, &uploads))
        .collect();
    Ok(Json(json!({"items": items, "next": page.next})))
}

/// `deleteKnowledgeDocument`: strata decides whose document it is (another tenant's is its 404).
async fn delete_document(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    let mut tx = t.begin(&st.db).await?;
    let p = strata::gate(&st, &mut tx, t.tenant, Service::Rag)
        .await?
        .with_actor(&t.caller.sub);
    tx.commit().await?;
    match rag::delete_document(&st.http, &p, id).await {
        Ok(()) => {}
        Err(e) if e.status_and_code() == (StatusCode::NOT_FOUND, "not_found") => {
            return Err(ApiError::Missing(
                "document_not_found",
                "No document with that id in this tenant.".into(),
            ));
        }
        Err(e) => return Err(e),
    }
    forget_counts(t.tenant);
    let mut tx = t.begin(&st.db).await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new(
            "knowledge_document.delete",
            "knowledge_document",
            id.to_string(),
        ),
    )
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------------------------
// Query playground

/// The root store the playground searches (§0 R7): the named store's root, else the tenant's only root; `None` when
/// the tenant has no store. Several roots and none named → 422 `root_required`.
fn resolve_root(all: &[StoreRow], store: Option<Uuid>) -> ApiResult<Option<&StoreRow>> {
    let root = match store {
        Some(id) => Some(stores::root_of(all, id).ok_or_else(stores::store_not_found)?),
        None => {
            let roots: Vec<&StoreRow> = all.iter().filter(|s| s.parent_id.is_none()).collect();
            match roots.as_slice() {
                [] => None,
                [one] => Some(one.id),
                _ => {
                    let keys: Vec<String> = roots.iter().map(|s| s.key.clone()).collect();
                    return Err(ApiError::Unprocessable(
                        "root_required",
                        "This tenant has several store trees; pick the store to search.".into(),
                    )
                    .with("roots", keys));
                }
            }
        }
    };
    Ok(root.and_then(|r| all.iter().find(|s| s.id == r)))
}

/// One `KnowledgeHit`. A tenant hit's `store_path` is the local path of its leaf (strata's names are agent keys);
/// a platform hit (rag #24) is `platform:<key>@<v>` as both its store and its path, with `metadata.provenance` kept.
fn hit_json(h: &Hit, all: &[StoreRow], by_key: &HashMap<&str, &StoreRow>) -> Value {
    let (store_id, store_path) = match h.layer {
        Layer::Platform => {
            let pin = h
                .pack
                .clone()
                .or_else(|| h.store_path.first().cloned())
                .unwrap_or_default();
            let label = format!("platform:{pin}");
            (json!(label), json!([label]))
        }
        Layer::Tenant => match h.store_path.last().and_then(|k| by_key.get(k.as_str())) {
            Some(s) => (json!(s.id), json!(stores::path_of(all, s.id))),
            None => (Value::Null, json!(h.store_path)),
        },
    };
    let score = h
        .scores
        .rerank
        .or(h.scores.dense)
        .or(h.scores.lexical)
        .unwrap_or(0.0);
    let metadata = match h.metadata.get("provenance") {
        Some(p) => json!({ "provenance": p }),
        None => json!({}),
    };
    json!({
        "store_id": store_id,
        "store_path": store_path,
        "document": h.title.clone().unwrap_or_else(|| h.source.clone()),
        "text": h.text,
        "page_start": h.page_start,
        "page_end": h.page_end,
        "score": score,
        "chunk_id": h.chunk_id,
        "ord": h.ord,
        "scores": {"dense": h.scores.dense, "lexical": h.scores.lexical, "rerank": h.scores.rerank},
        "layer": h.layer,
        "pack": h.pack,
        "metadata": metadata,
    })
}

/// `KnowledgeQueryResult` of strata's response.
fn result_json(
    input: &QueryInput,
    all: &[StoreRow],
    searched: Vec<String>,
    skipped: Vec<String>,
    resp: &QueryResp,
    degraded: Vec<String>,
) -> Value {
    let by_key: HashMap<&str, &StoreRow> = all.iter().map(|s| (s.key.as_str(), s)).collect();
    let by_sid: HashMap<Uuid, &str> = all
        .iter()
        .filter_map(|s| s.strata_store_id.map(|sid| (sid, s.key.as_str())))
        .collect();
    let leaves: Vec<String> = resp
        .routing
        .leaves
        .iter()
        .map(|sid| {
            by_sid
                .get(&sid.0)
                .map_or_else(|| sid.to_string(), |k| (*k).to_string())
        })
        .collect();
    let hits: Vec<Value> = resp
        .results
        .iter()
        .map(|h| hit_json(h, all, &by_key))
        .collect();
    let t = resp.timings_ms;
    json!({
        "query": input.query,
        "mode": input.mode,
        "searched": searched,
        "skipped_unbound": skipped,
        "hits": hits,
        "took_ms": t.total,
        "routing": {
            "mode_used": resp.routing.mode_used,
            "leaves": leaves,
            "laya_consulted": resp.routing.laya_consulted,
            "laya_fallback": resp.routing.laya_fallback,
        },
        "timings_ms": {"embed": t.embed, "route": t.route, "search": t.search, "rerank": t.rerank, "total": t.total},
        "degraded": degraded,
    })
}

/// `knowledgeQuery`: the tenant-tree playground, or (PK T5, `job` + `stage` + `kind`) one knowledge kind's leaf set
/// in the job's draft through `knowledge::kinds::search` (R10/PK-7: `leaf_ids`, D91 relaxed: no root is resolved,
/// so several trees never answer `root_required`; unreadable packs are listed in `degraded`).
async fn query(
    Member(t): Member,
    State(st): State<AppState>,
    ApiJson(input): ApiJson<QueryInput>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let p = strata::gate(&st, &mut tx, t.tenant, Service::Rag)
        .await?
        .with_actor(&t.caller.sub);
    if !input.query.chars().any(char::is_alphanumeric) {
        return Err(ApiError::Unprocessable(
            "empty_query",
            "Ask a question with at least one meaningful word.".into(),
        ));
    }
    let all = stores::all(&mut tx).await?;
    tx.commit().await?;
    match (&input.job, &input.stage, &input.kind) {
        (_, None, None) => {}
        (Some(job), Some(stage), Some(kind)) => {
            return kind_query(&st, &t, &input, &all, job, stage, kind).await;
        }
        _ => {
            return Err(ApiError::Unprocessable(
                "invalid_query",
                "A knowledge kind search names job, stage and kind together.".into(),
            ));
        }
    }
    let Some(root) = resolve_root(&all, input.store_id)? else {
        let empty = QueryResp::default();
        return Ok(Json(result_json(
            &input,
            &all,
            Vec::new(),
            Vec::new(),
            &empty,
            Vec::new(),
        )));
    };
    let tree = stores::subtree(&all, root.id);
    let leaves = tree.iter().filter(|s| s.is_leaf());
    let searched: Vec<String> = leaves
        .clone()
        .filter(|s| s.strata_store_id.is_some())
        .map(|s| s.key.clone())
        .collect();
    let skipped: Vec<String> = leaves
        .filter(|s| s.strata_store_id.is_none())
        .map(|s| s.key.clone())
        .collect();
    // T4a (§0 R7): a job's search names the strata root of the generation its draft pins
    let sid = match &input.job {
        Some(key) => {
            let mut tx = t.begin(&st.db).await?;
            let jr = crate::knowledge::generations::job_ref(&mut tx, key).await?;
            let sid = crate::knowledge::generations::root_for(
                &mut tx,
                &jr,
                root,
                &crate::design::DesignOf::Draft,
            )
            .await?;
            tx.commit().await?;
            sid
        }
        None => root.strata(),
    };
    let resp = match sid {
        Some(sid) => {
            let req = QueryReq {
                query: input.query.clone(),
                top_k: input.top_k,
                route_mode: Some(input.mode),
                beam: input.beam.filter(|_| input.mode != RouteMode::Embed),
                min_score: input.min_score,
                ..QueryReq::default()
            };
            rag::query(&st.http, &p, &QueryTarget::Root(sid), &req).await?
        }
        None => QueryResp::default(),
    };
    Ok(Json(result_json(
        &input,
        &all,
        searched,
        skipped,
        &resp,
        Vec::new(),
    )))
}

/// The kind-scoped playground (PK T5): `kinds::search` over `(stage, kind)` of `job`'s draft. `searched` /
/// `skipped_unbound` name the kind's tenant leaf (bound in the job's pinned generation or not); route mode and beam
/// are not sent (a leaf set is never routed).
async fn kind_query(
    st: &AppState,
    t: &TenantCtx,
    input: &QueryInput,
    all: &[StoreRow],
    job: &str,
    stage: &str,
    kind: &str,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let jr = crate::knowledge::generations::job_ref(&mut tx, job).await?;
    let set = crate::knowledge::kinds::leaf_set(
        &mut tx,
        &st.design,
        &jr,
        stage,
        kind,
        &crate::design::DesignOf::Draft,
    )
    .await?;
    tx.commit().await?;
    let leaf_key = set
        .tenant_store
        .and_then(|id| all.iter().find(|s| s.id == id))
        .map(|s| s.key.clone());
    let (searched, skipped): (Vec<String>, Vec<String>) = match (leaf_key, set.tenant) {
        (Some(k), Some(_)) => (vec![k], Vec::new()),
        (Some(k), None) => (Vec::new(), vec![k]),
        (None, _) => (Vec::new(), Vec::new()),
    };
    let req = QueryReq {
        query: input.query.clone(),
        top_k: input.top_k,
        min_score: input.min_score,
        ..QueryReq::default()
    };
    let answer = crate::knowledge::kinds::search(
        st,
        t.tenant,
        &jr,
        stage,
        kind,
        &crate::design::DesignOf::Draft,
        &req,
    )
    .await?;
    Ok(Json(result_json(
        input,
        all,
        searched,
        skipped,
        &answer.resp,
        answer.degraded,
    )))
}

// ---------------------------------------------------------------------------------------------------------------
// Model card

/// A working `provider_slot` row (T4, 0051).
#[derive(sqlx::FromRow)]
struct SlotRow {
    slot: String,
    configured: bool,
    kind: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    region: Option<String>,
    version: i32,
}

/// One `ModelBinding`: the internal tenant (D70) runs on mlx_api with strata's own model; every other tenant on its
/// working slot, or, while it registers none of its own (no row, not configured, or reset to the deployment default),
/// on the platform model strata serves it (`source: "default"`, the tenant's knowledge under its own licence).
/// `platform` = the model is the platform's, not a provider of the tenant's.
fn binding(slot: &str, internal: bool, rows: &[SlotRow], remote: &Value) -> Value {
    if internal {
        return json!({
            "slot": slot, "configured": true, "kind": "mlx_api",
            "model": remote[slot]["model"].as_str(), "base_url": null, "region": null,
            "version": 0, "internal": true, "platform": true,
        });
    }
    let row = rows.iter().find(|r| r.slot == slot);
    match row {
        Some(r) if r.configured && r.model.is_some() => json!({
            "slot": slot, "configured": true, "kind": r.kind, "model": r.model,
            "base_url": r.base_url, "region": r.region, "version": r.version,
            "internal": r.kind.as_deref().is_some_and(|k| matches!(k, "mlx_api" | "pgx_vllm")),
            "platform": false,
        }),
        r if remote[slot]["source"] == "default" => json!({
            "slot": slot, "configured": true, "kind": null, "model": remote[slot]["model"].as_str(),
            "base_url": null, "region": null, "version": r.map_or(0, |r| r.version),
            "internal": false, "platform": true,
        }),
        r => json!({
            "slot": slot, "configured": false, "kind": null, "model": null, "base_url": null,
            "region": null, "version": r.map_or(0, |r| r.version), "internal": false, "platform": false,
        }),
    }
}

/// `retrievalModels`.
async fn models(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let internal = llm::platform_owned(&mut tx, &st, t.tenant).await?;
    let rows: Vec<SlotRow> = if internal {
        Vec::new()
    } else {
        sqlx::query_as(
            "SELECT slot, configured, kind, base_url, model, region, version FROM provider_slot \
              WHERE slot IN ('embed', 'rerank')",
        )
        .fetch_all(&mut *tx)
        .await?
    };
    let plane = strata::gate(&st, &mut tx, t.tenant, Service::Rag)
        .await
        .ok();
    tx.commit().await?;
    // strata's view of the tenant's providers (`dims`); best effort, never a 5xx for the card
    let remote = match &plane {
        Some(p) => rag::providers_get(&st.http, p).await.unwrap_or(Value::Null),
        None => Value::Null,
    };
    let mut embedding = binding("embed", internal, &rows, &remote);
    let dims = if embedding["model"].is_null() {
        Value::Null
    } else {
        json!(remote["embed"]["dims"].as_i64())
    };
    embedding["dimensions"] = dims;
    let mut reranker = binding("rerank", internal, &rows, &remote);
    reranker["candidates"] = json!(RERANK_CANDIDATES);
    Ok(Json(json!({
        "embedding": embedding,
        "reranker": reranker,
        "chunking": {"target_tokens": CHUNK_TARGET_TOKENS, "overlap_tokens": CHUNK_OVERLAP_TOKENS},
        "query_defaults": {
            "mode": "hybrid", "top_k": DEFAULT_TOP_K, "beam": DEFAULT_BEAM, "min_score": DEFAULT_MIN_SCORE,
        },
    })))
}

// ---------------------------------------------------------------------------------------------------------------
// Direct upload (T6)

/// One file of an upload, read up to the upload cap.
struct UploadedFile {
    name: String,
    content_type: Option<String>,
    data: Vec<u8>,
}

pub(crate) fn too_large() -> ApiError {
    ApiError::TooLarge("too_large", "One upload takes at most 50 MB in all.".into())
}

/// A body over the route's limit is too large; any other parse failure is a malformed body.
pub(crate) fn upload_error(e: MultipartError) -> ApiError {
    if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
        too_large()
    } else {
        ApiError::BadRequest("invalid_body", "the multipart body is not valid".into())
    }
}

/// The lowercase extension of a file name (`""` when it has none).
pub(crate) fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// The `knowledge_upload.language` check: `^[a-z]{2,3}(-[A-Za-z0-9]{2,8})*$`.
fn valid_language(l: &str) -> bool {
    let mut parts = l.split('-');
    let head = parts.next().unwrap_or_default();
    (2..=3).contains(&head.len())
        && head.bytes().all(|b| b.is_ascii_lowercase())
        && parts.all(|p| (2..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// What an upload form carries.
struct UploadForm {
    files: Vec<UploadedFile>,
    language: Option<String>,
    /// The raw `supersedes` field (a JSON array string), `None` when absent or blank.
    supersedes: Option<String>,
}

/// 422 `invalid_supersedes`.
fn invalid_supersedes(msg: impl Into<String>) -> ApiError {
    ApiError::Unprocessable("invalid_supersedes", msg.into())
}

/// The upload's `supersedes` field read and normalised (`[]` when absent), as a source's (PK-8).
fn upload_supersedes(raw: Option<&str>) -> ApiResult<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    let v: Value = serde_json::from_str(raw).map_err(|_| {
        invalid_supersedes("supersedes must be a JSON array of topic keys, such as [\"returns\"].")
    })?;
    // not a credential, but stored and audited: refused when secret- or email-shaped, as a source body is
    if crate::audit::scrub::check(&v).is_err() {
        return Err(invalid_supersedes(
            "A supersedes topic key looks like a secret or an email address.",
        ));
    }
    crate::knowledge::sources::normalize_supersedes(&v).map_err(invalid_supersedes)
}

/// The form's `file` parts, `language` and `supersedes`; any other field is skipped. Refused as soon as a limit is
/// crossed, so nothing past it is read.
async fn read_upload(mut form: Multipart) -> ApiResult<UploadForm> {
    let mut files: Vec<UploadedFile> = Vec::new();
    let mut total = 0usize;
    let mut language = None;
    let mut supersedes = None;
    while let Some(mut field) = form.next_field().await.map_err(upload_error)? {
        let field_name = field.name().unwrap_or_default().to_string();
        match field_name.as_str() {
            "file" => {
                if files.len() == UPLOAD_MAX_FILES {
                    return Err(ApiError::Unprocessable(
                        "too_many_files",
                        format!("One upload takes at most {UPLOAD_MAX_FILES} files."),
                    ));
                }
                let name = field.file_name().unwrap_or_default().trim().to_string();
                if !UPLOAD_TYPES.contains(&extension(&name).as_str()) {
                    return Err(ApiError::Unsupported(
                        "unsupported_type",
                        format!(
                            "{name}: not a supported type ({}).",
                            UPLOAD_TYPES.join(", ")
                        ),
                    ));
                }
                let content_type = field.content_type().map(str::to_string);
                let mut data = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(upload_error)? {
                    total += chunk.len();
                    if total > UPLOAD_MAX_BYTES {
                        return Err(too_large());
                    }
                    data.extend_from_slice(&chunk);
                }
                files.push(UploadedFile {
                    name,
                    content_type,
                    data,
                });
            }
            "language" => {
                let v = field.text().await.map_err(upload_error)?;
                let v = v.trim();
                if !v.is_empty() {
                    language = Some(v.to_string());
                }
            }
            "supersedes" => {
                let v = field.text().await.map_err(upload_error)?;
                let v = v.trim();
                if !v.is_empty() {
                    supersedes = Some(v.to_string());
                }
            }
            _ => {}
        }
    }
    if files.is_empty() {
        return Err(ApiError::Unprocessable(
            "no_file",
            "Pick at least one file.".into(),
        ));
    }
    Ok(UploadForm {
        files,
        language,
        supersedes,
    })
}

/// `uploadToStore`: checked, read, sent to strata_ingest as one multipart request (no transaction is open while the
/// body is read or strata is called), then recorded in `knowledge_upload`.
async fn upload(
    Requires(t, _): Requires<role::KnowledgeManager>,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    form: Result<Multipart, MultipartRejection>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let p = strata::gate(&st, &mut tx, t.tenant, Service::Ingest).await?;
    let s = stores::by_id(&mut tx, id, false)
        .await?
        .ok_or_else(stores::store_not_found)?;
    if !s.is_leaf() {
        return Err(ApiError::Unprocessable(
            "not_a_leaf",
            format!(
                "{} is a group; upload to one of the stores below it.",
                s.key
            ),
        ));
    }
    // T4a: the pinned generation's strata store replaces the store's own binding here.
    let Some(sid) = s.strata() else {
        return Err(ApiError::Conflict(
            "store_not_bound",
            format!(
                "{} is not created in strata_rag yet; create missing stores first.",
                s.key
            ),
        ));
    };
    let first_language: Option<Option<String>> =
        sqlx::query_scalar("SELECT languages[1] FROM jobs ORDER BY created_at, id LIMIT 1")
            .fetch_optional(&mut *tx)
            .await?;
    tx.commit().await?;
    let form = form.map_err(|_| {
        ApiError::BadRequest(
            "invalid_body",
            "the body must be multipart/form-data".into(),
        )
    })?;
    let UploadForm {
        files,
        language: given,
        supersedes,
    } = read_upload(form).await?;
    let supersedes = upload_supersedes(supersedes.as_deref())?;
    let language = match given {
        Some(l) if valid_language(&l) => l,
        Some(_) => {
            return Err(ApiError::Unprocessable(
                "invalid_language",
                "The language is a language tag such as en or nl-BE.".into(),
            ));
        }
        None => first_language
            .flatten()
            .filter(|l| valid_language(l))
            .unwrap_or_else(|| DEFAULT_LANGUAGE.to_string()),
    };

    let upload_id = Uuid::new_v4();
    let source_id = format!("upload:{upload_id}");
    let req = reqwest::multipart::Form::new()
        .text("store_id", sid.to_string())
        .text("source_id", source_id.clone())
        .text("options", json!({"language": language}).to_string());
    // strata_ingest #15: a text field before the first file part, applied to every file of the upload
    let mut req = crate::knowledge::sources::supersedes_field(req, &supersedes);
    let mut records: Vec<Value> = Vec::with_capacity(files.len());
    let mut bytes = 0usize;
    for f in files {
        bytes += f.data.len();
        records.push(json!({
            "name": f.name, "bytes": f.data.len(), "content_type": f.content_type,
            "ingest_job_id": null, "error": null,
        }));
        req = req.part(
            "file",
            reqwest::multipart::Part::bytes(f.data).file_name(f.name),
        );
    }
    let created = ingest::create_multipart(&st.http, &p, req).await?;
    for (r, j) in records.iter_mut().zip(&created.jobs) {
        r["ingest_job_id"] = json!(j.job_id);
        if j.status != "queued" {
            r["error"] = json!(j.status);
        }
    }
    let job_ids: Vec<String> = created.jobs.iter().map(|j| j.job_id.to_string()).collect();
    let first_name = records
        .first()
        .and_then(|r| r["name"].as_str())
        .unwrap_or_default()
        .to_string();
    let file_count = records.len();
    let Some(first) = created.jobs.first() else {
        return Err(crate::upstream::unavailable(Service::Ingest.name()));
    };

    let mut tx = t.begin(&st.db).await?;
    sqlx::query(
        "INSERT INTO knowledge_upload (id, store_id, language, files, ingest_job_ids, created_by, supersedes) \
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(upload_id)
    .bind(s.id)
    .bind(&language)
    .bind(Value::Array(records))
    .bind(&job_ids)
    .bind(&t.caller.actor)
    .bind(&supersedes)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("ingest.upload", "ingest_job", first.job_id.to_string())
            .detail("store", s.key.clone())
            .detail("upload", upload_id.to_string())
            .detail("files", file_count)
            .detail("bytes", bytes)
            .detail("language", language.clone())
            .detail("supersedes", supersedes.clone()),
    )
    .await?;
    tx.commit().await?;

    // the answer is the first job as strata now shows it; the upload stands even when that read fails
    let raw = match ingest::get(&st.http, &p, first.job_id).await {
        Ok(v) => v,
        Err(_) => json!({
            "id": first.job_id, "store_id": sid, "status": first.status, "meta": {"source_id": source_id},
            "source": {"kind": "upload", "filename": first_name}, "steps": [], "document_ids": [],
        }),
    };
    let mut view = ingest::ui_view(&raw, Some(s.id), &t.caller.actor);
    if file_count > 1 {
        view["source_label"] = json!(format!("{first_name} + {} more", file_count - 1));
    }
    forget_counts(t.tenant);
    Ok(Json(view))
}
