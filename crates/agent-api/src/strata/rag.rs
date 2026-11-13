//! strata_rag client (pinned at `7c2f1ad`: #22 store providers + reembed-copy, #23 leaf-set queries, #24 platform
//! packs). Request and response shapes mirror strata_rag `src/api/handlers.rs`, `src/api/eval_api.rs`,
//! `src/api/platform.rs`, `src/query.rs`; response structs ignore fields they do not name, so later strata additions
//! do not break them.
//!
//! D91 / G7: a query always names its targets ([`QueryTarget`], serialised as exactly one of `root_id` / `root_ids` /
//! `leaf_ids`) and a tenant eval always sends `root_id`. A 409 `ambiguous_root` that still comes back is a bug: it is
//! passed through and counted (`agent_strata_ambiguous_root_total{op}`).
//!
//! R10/R11: a pack root the tenant may no longer read answers 404 `not_found` naming the store;
//! [`unreadable_store`] tells that apart so the caller (T10) can retry without that pack.
use axum::http::StatusCode;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use super::{Plane, StrataStoreId, enc, query_string};
use crate::error::{ApiError, ApiResult};
use crate::upstream::{LONG_TIMEOUT, Reply};

/// The counter of 409 `ambiguous_root` answers that reached agent (each one is a bug, G7).
pub const AMBIGUOUS_ROOT_TOTAL: &str = "agent_strata_ambiguous_root_total";

// ---------------------------------------------------------------------------------------------------------------
// Stores

/// strata's store kind; agent `group` ↔ strata `router`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    Router,
    Leaf,
}

/// `StoreView` (strata `src/engine.rs`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StoreView {
    pub id: StrataStoreId,
    #[serde(default)]
    pub parent_id: Option<StrataStoreId>,
    pub kind: StoreKind,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub token_count: i64,
    #[serde(default)]
    pub chunk_count: i64,
    #[serde(default)]
    pub children: Vec<StrataStoreId>,
}

#[derive(Deserialize)]
struct StoreList {
    stores: Vec<StoreView>,
}

/// `POST /api/stores` body (`CreateStoreBody`): strata `name` = agent store key, `description` = its label.
#[derive(Clone, Debug, Serialize)]
pub struct NewStore {
    pub parent_id: Option<StrataStoreId>,
    pub kind: StoreKind,
    pub name: String,
    pub description: String,
}

/// `GET /api/stores`: the tenant's whole tree (own stores only).
pub async fn stores(http: &reqwest::Client, p: &Plane<'_>) -> ApiResult<Vec<StoreView>> {
    let r = p
        .call("list_stores", Method::GET, "/api/stores")
        .send(http)
        .await?;
    Ok(p.decode::<StoreList>("list_stores", r.body)?.stores)
}

/// `GET /api/stores/{id}`.
pub async fn get_store(
    http: &reqwest::Client,
    p: &Plane<'_>,
    id: StrataStoreId,
) -> ApiResult<StoreView> {
    let path = format!("/api/stores/{id}");
    let r = p.call("get_store", Method::GET, &path).send(http).await?;
    p.decode("get_store", r.body)
}

/// `POST /api/stores` → 201 view. 409 `duplicate_name` / `pack_published`, 404 for an unknown parent.
pub async fn create_store(
    http: &reqwest::Client,
    p: &Plane<'_>,
    store: &NewStore,
) -> ApiResult<StoreView> {
    let body = serde_json::to_value(store).map_err(|e| ApiError::Internal(e.into()))?;
    let r = p
        .call("create_store", Method::POST, "/api/stores")
        .json(body)
        .send(http)
        .await?;
    p.decode("create_store", r.body)
}

/// `PATCH /api/stores/{id}` with the fields that are set (at least one).
pub async fn update_store(
    http: &reqwest::Client,
    p: &Plane<'_>,
    id: StrataStoreId,
    name: Option<&str>,
    description: Option<&str>,
) -> ApiResult<StoreView> {
    let mut body = Map::new();
    if let Some(n) = name {
        body.insert("name".into(), json!(n));
    }
    if let Some(d) = description {
        body.insert("description".into(), json!(d));
    }
    let path = format!("/api/stores/{id}");
    let r = p
        .call("update_store", Method::PATCH, &path)
        .json(Value::Object(body))
        .send(http)
        .await?;
    p.decode("update_store", r.body)
}

/// `DELETE /api/stores/{id}[?recursive=true]` → 204. 409 `has_children` without `recursive`.
pub async fn delete_store(
    http: &reqwest::Client,
    p: &Plane<'_>,
    id: StrataStoreId,
    recursive: bool,
) -> ApiResult<()> {
    let q = query_string(&[("recursive", recursive.then(|| "true".to_string()))]);
    let path = format!("/api/stores/{id}{q}");
    p.call("delete_store", Method::DELETE, &path)
        .send(http)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Documents

/// `GET /api/stores/{id}/documents` parameters (strata: `limit` 1..=500, default 50; opaque `cursor`).
#[derive(Clone, Debug, Default)]
pub struct DocumentsQuery {
    pub limit: Option<i64>,
    pub cursor: Option<String>,
    pub source_id: Option<String>,
}

/// `DocumentListItem` (strata `src/db/documents.rs`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub id: Uuid,
    pub store_id: StrataStoreId,
    pub source: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content_hash: String,
    #[serde(default)]
    pub token_count: i64,
    #[serde(default)]
    pub chunk_count: i64,
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub source_run: Option<String>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
    pub created_at: String,
}

/// One page of documents, newest first; `next` is strata's opaque cursor (`null` on the last page).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DocumentPage {
    pub items: Vec<Document>,
    #[serde(default)]
    pub next: Option<String>,
}

/// `GET /api/stores/{id}/documents?limit=&cursor=&source_id=` (a router answers an empty page).
pub async fn documents(
    http: &reqwest::Client,
    p: &Plane<'_>,
    store: StrataStoreId,
    q: &DocumentsQuery,
) -> ApiResult<DocumentPage> {
    let qs = query_string(&[
        ("limit", q.limit.map(|l| l.to_string())),
        ("cursor", q.cursor.clone()),
        ("source_id", q.source_id.clone()),
    ]);
    let path = format!("/api/stores/{store}/documents{qs}");
    let r = p.call("documents", Method::GET, &path).send(http).await?;
    p.decode("documents", r.body)
}

/// `DELETE /api/documents/{id}` → 204 (404 for another tenant's document).
pub async fn delete_document(http: &reqwest::Client, p: &Plane<'_>, id: Uuid) -> ApiResult<()> {
    let path = format!("/api/documents/{id}");
    p.call("delete_document", Method::DELETE, &path)
        .send(http)
        .await?;
    Ok(())
}

/// Which of a source's documents a prune removes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PruneScope {
    /// `all=true`.
    All,
    /// `except_run=<run>`: everything but that run's documents (the empty-sync guard applies).
    ExceptRun(String),
}

/// `DELETE /api/sources/{source_id}/documents` parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prune {
    pub scope: PruneScope,
    /// One leaf only.
    pub store_id: Option<StrataStoreId>,
    pub allow_empty: bool,
}

/// `PruneReport` (strata `src/ingest.rs`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneReport {
    pub deleted: u64,
    pub kept: i64,
    #[serde(default)]
    pub stores: Vec<StrataStoreId>,
    #[serde(default)]
    pub refused_stores: Vec<StrataStoreId>,
}

/// `DELETE /api/sources/{source_id}/documents?all=|except_run=&store_id=&allow_empty=` → the report.
pub async fn prune_source(
    http: &reqwest::Client,
    p: &Plane<'_>,
    source_id: &str,
    prune: &Prune,
) -> ApiResult<PruneReport> {
    let (all, except_run) = match &prune.scope {
        PruneScope::All => (Some("true".to_string()), None),
        PruneScope::ExceptRun(run) => (None, Some(run.clone())),
    };
    let qs = query_string(&[
        ("all", all),
        ("except_run", except_run),
        ("store_id", prune.store_id.map(|s| s.to_string())),
        ("allow_empty", prune.allow_empty.then(|| "true".to_string())),
    ]);
    let path = format!("/api/sources/{}/documents{qs}", enc(source_id));
    let r = p
        .call("prune_source", Method::DELETE, &path)
        .send(http)
        .await?;
    p.decode("prune_source", r.body)
}

// ---------------------------------------------------------------------------------------------------------------
// Query

/// strata's `RouteMode`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RouteMode {
    Embed,
    Laya,
    Hybrid,
}

/// What a query searches (strata takes at most one; agent always names one, D91).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum QueryTarget {
    /// `root_id`: one tree, routed (the tenant-tree playground).
    Root(StrataStoreId),
    /// `root_ids`: each routed, selected leaves unioned (a kind-scoped leaf set: tenant leaf + pack roots, R10).
    Roots(Vec<StrataStoreId>),
    /// `leaf_ids`: exactly these leaves, no routing.
    Leaves(Vec<StrataStoreId>),
}

impl QueryTarget {
    /// The one body field and its value.
    pub fn field(&self) -> (&'static str, Value) {
        match self {
            QueryTarget::Root(id) => ("root_id", json!(id)),
            QueryTarget::Roots(ids) => ("root_ids", json!(ids)),
            QueryTarget::Leaves(ids) => ("leaf_ids", json!(ids)),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            QueryTarget::Root(_) => false,
            QueryTarget::Roots(ids) | QueryTarget::Leaves(ids) => ids.is_empty(),
        }
    }
}

/// `POST /api/query` parameters other than the target; unset fields take strata's defaults. The platform fields
/// exist for completeness and B3 never sets them (R10: precedence is strata's deployment default).
#[derive(Clone, Debug, Default, Serialize)]
pub struct QueryReq {
    pub query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_mode: Option<RouteMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beam: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_score: Option<f32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub expand: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ids: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_penalty: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub platform_min_score: Option<f32>,
}

/// A hit's layer (rag #24); absent (older strata) = `tenant`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layer {
    #[default]
    Tenant,
    Platform,
}

/// `Scores`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Scores {
    #[serde(default)]
    pub dense: Option<f32>,
    #[serde(default)]
    pub lexical: Option<f32>,
    #[serde(default)]
    pub rerank: Option<f32>,
}

/// `QueryResult`. For a platform hit `store_path` starts with the pack key and `pack` is `"<key>@<version>"`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hit {
    pub chunk_id: Uuid,
    pub document_id: Uuid,
    #[serde(default)]
    pub store_path: Vec<String>,
    #[serde(default)]
    pub layer: Layer,
    #[serde(default)]
    pub pack: Option<String>,
    #[serde(default)]
    pub ord: i32,
    pub text: String,
    #[serde(default)]
    pub page_start: Option<i32>,
    #[serde(default)]
    pub page_end: Option<i32>,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub scores: Scores,
    #[serde(default)]
    pub source_id: Option<String>,
    #[serde(default)]
    pub source_run: Option<String>,
    #[serde(default)]
    pub metadata: Map<String, Value>,
}

/// `RoutingInfo`; `mode_used` is `null` when nothing was routed (`leaf_ids`, rag #23).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Routing {
    #[serde(default)]
    pub mode_used: Option<RouteMode>,
    #[serde(default)]
    pub leaves: Vec<StrataStoreId>,
    #[serde(default)]
    pub laya_consulted: bool,
    #[serde(default)]
    pub laya_fallback: Option<String>,
}

/// `Timings` (ms). Fields strata adds later are ignored; `total` is the playground's `took_ms`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timings {
    #[serde(default)]
    pub embed: u64,
    #[serde(default)]
    pub route: u64,
    #[serde(default)]
    pub search: u64,
    #[serde(default)]
    pub rerank: u64,
    #[serde(default)]
    pub total: u64,
}

/// `QueryResponse`. `providers`, `timings_by_model` and `expansion` are kept as strata sent them.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct QueryResp {
    pub results: Vec<Hit>,
    #[serde(default)]
    pub routing: Routing,
    #[serde(default)]
    pub timings_ms: Timings,
    #[serde(default)]
    pub providers: Value,
    #[serde(default)]
    pub timings_by_model: Vec<Value>,
    #[serde(default)]
    pub expansion: Option<Value>,
}

/// Counts and logs a 409 `ambiguous_root` (G7); every error is returned unchanged.
fn note_ambiguous(op: &'static str, e: ApiError) -> ApiError {
    if e.code() == "ambiguous_root" {
        metrics::counter!(AMBIGUOUS_ROOT_TOTAL, "op" => op).increment(1);
        tracing::error!(
            op,
            "strata_rag answered ambiguous_root although agent named the root (bug)"
        );
    }
    e
}

/// `POST /api/query` on `target` (timeout [`LONG_TIMEOUT`]). An empty `Roots` / `Leaves` makes no call and answers an
/// empty result (strata would refuse it 422). Errors as `upstream`; see [`unreadable_store`] for a 404.
pub async fn query(
    http: &reqwest::Client,
    p: &Plane<'_>,
    target: &QueryTarget,
    req: &QueryReq,
) -> ApiResult<QueryResp> {
    if target.is_empty() {
        return Ok(QueryResp::default());
    }
    let mut body = serde_json::to_value(req).map_err(|e| ApiError::Internal(e.into()))?;
    if let Value::Object(m) = &mut body {
        let (k, v) = target.field();
        m.insert(k.to_string(), v);
    }
    let r = p
        .call("query", Method::POST, "/api/query")
        .json(body)
        .timeout(LONG_TIMEOUT)
        .send(http)
        .await
        .map_err(|e| note_ambiguous("query", e))?;
    p.decode("query", r.body)
}

/// The store a 404 `not_found` names (`"not found: store <uuid>"`, strata `src/query.rs`): a target the tenant may
/// not read, e.g. a pinned pack root after an exclusion or a curator delete (R10/R11). `None` for any other error.
pub fn unreadable_store(e: &ApiError) -> Option<StrataStoreId> {
    if e.status_and_code() != (StatusCode::NOT_FOUND, "not_found") {
        return None;
    }
    // crate::upstream::refusal keeps the id as the extra field `store` (the message is agent's own)
    let ApiError::Detailed { extra, .. } = e else {
        return None;
    };
    let id = extra.get("store")?.as_str()?;
    Uuid::parse_str(id).ok().map(StrataStoreId)
}

// ---------------------------------------------------------------------------------------------------------------
// Providers (tenant: embed / rerank / chat; per store, D93)

/// `GET /api/tenant/providers` → `ProvidersView` (`{embed, rerank, chat}`, never a key).
pub async fn providers_get(http: &reqwest::Client, p: &Plane<'_>) -> ApiResult<Value> {
    let r = p
        .call("providers_get", Method::GET, "/api/tenant/providers")
        .send(http)
        .await?;
    Ok(r.body)
}

/// `POST /api/tenant/providers/test` (strata_rag #25 `db99e9c`, T0t / 06 §3 A3): the PUT's validation and live probe of one
/// provider (`capability` embed | rerank | chat), storing nothing → `{ok, capability, model, latency_ms, detail, dims?, error_class?}`; a failing provider is `ok: false`,
/// never a 5xx. 422 refusals map through the upstream table (`unknown_capability`, `secret_required_for_new_host`,
/// `provider_host_not_allowed`, the PUT's codes). `Ok(None)` = the route is not deployed (404).
pub async fn providers_test(
    http: &reqwest::Client,
    p: &Plane<'_>,
    body: Value,
) -> ApiResult<Option<Value>> {
    let r = p
        .call("providers_test", Method::POST, "/api/tenant/providers/test")
        .json(body)
        .timeout(LONG_TIMEOUT)
        .send_any(http)
        .await?;
    if r.status == axum::http::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !r.status.is_success() {
        return Err(crate::upstream::refusal(
            p.service().name(),
            r.status,
            &r.body,
        ));
    }
    Ok(Some(r.body))
}

/// `PUT /api/tenant/providers` (whole-set replace, live probe; `api_key` write-only) → the stored view.
pub async fn providers_put(http: &reqwest::Client, p: &Plane<'_>, body: Value) -> ApiResult<Value> {
    let r = p
        .call("providers_put", Method::PUT, "/api/tenant/providers")
        .json(body)
        .timeout(LONG_TIMEOUT)
        .send(http)
        .await?;
    Ok(r.body)
}

/// `GET /api/stores/{id}/providers` → `StoreProvidersView` (`{store_id, embed_model, embed, rerank, chat}`).
pub async fn store_providers_get(
    http: &reqwest::Client,
    p: &Plane<'_>,
    store: StrataStoreId,
) -> ApiResult<Value> {
    let path = format!("/api/stores/{store}/providers");
    let r = p
        .call("store_providers_get", Method::GET, &path)
        .send(http)
        .await?;
    Ok(r.body)
}

/// `PUT /api/stores/{id}/providers` (409 `embed_model_mismatch` / `pack_published`) → the view.
pub async fn store_providers_put(
    http: &reqwest::Client,
    p: &Plane<'_>,
    store: StrataStoreId,
    body: Value,
) -> ApiResult<Value> {
    let path = format!("/api/stores/{store}/providers");
    let r = p
        .call("store_providers_put", Method::PUT, &path)
        .json(body)
        .timeout(LONG_TIMEOUT)
        .send(http)
        .await?;
    Ok(r.body)
}

/// `DELETE /api/stores/{id}/providers` (the store inherits again) → the view.
pub async fn store_providers_delete(
    http: &reqwest::Client,
    p: &Plane<'_>,
    store: StrataStoreId,
) -> ApiResult<Value> {
    let path = format!("/api/stores/{store}/providers");
    let r = p
        .call("store_providers_delete", Method::DELETE, &path)
        .send(http)
        .await?;
    Ok(r.body)
}

// ---------------------------------------------------------------------------------------------------------------
// Reembed-copy (D83 / D92)

/// `ReembedCopyJob` (strata `src/reembed_copy.rs`); `state` is `running` | `done` | `failed`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReembedCopyJob {
    pub job: Uuid,
    pub source_store: StrataStoreId,
    pub target_store: StrataStoreId,
    #[serde(default)]
    pub embed_model: String,
    #[serde(default)]
    pub copy_vectors: bool,
    pub state: String,
    #[serde(default)]
    pub chunks_done: i64,
    #[serde(default)]
    pub chunks_total: i64,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub finished_at: Option<String>,
}

/// `POST /api/stores/{source}/reembed-copy {target_store, copy_vectors}` → 202 with the running job.
pub async fn reembed_copy_start(
    http: &reqwest::Client,
    p: &Plane<'_>,
    source: StrataStoreId,
    target: StrataStoreId,
    copy_vectors: bool,
) -> ApiResult<ReembedCopyJob> {
    let path = format!("/api/stores/{source}/reembed-copy");
    let r = p
        .call("reembed_copy_start", Method::POST, &path)
        .json(json!({"target_store": target, "copy_vectors": copy_vectors}))
        .send(http)
        .await?;
    p.decode("reembed_copy_start", r.body)
}

/// `GET /api/stores/{source}/reembed-copy/{job}`.
pub async fn reembed_copy_get(
    http: &reqwest::Client,
    p: &Plane<'_>,
    source: StrataStoreId,
    job: Uuid,
) -> ApiResult<ReembedCopyJob> {
    let path = format!("/api/stores/{source}/reembed-copy/{job}");
    let r = p
        .call("reembed_copy_get", Method::GET, &path)
        .send(http)
        .await?;
    p.decode("reembed_copy_get", r.body)
}

// ---------------------------------------------------------------------------------------------------------------
// Eval (tenant questions + stored async runs, rag #21)

/// `GET /api/eval/questions` → `{items: [Question], next: null}`.
pub async fn eval_questions(http: &reqwest::Client, p: &Plane<'_>) -> ApiResult<Value> {
    let r = p
        .call("eval_questions", Method::GET, "/api/eval/questions")
        .send(http)
        .await?;
    Ok(r.body)
}

/// `POST /api/eval/questions` (`QuestionInput`) → 201 `Question`; 409 `question_exists` / `too_many_questions`.
pub async fn eval_question_create(
    http: &reqwest::Client,
    p: &Plane<'_>,
    question: Value,
) -> ApiResult<Value> {
    let r = p
        .call("eval_question_create", Method::POST, "/api/eval/questions")
        .json(question)
        .send(http)
        .await?;
    Ok(r.body)
}

/// `PATCH /api/eval/questions/{id}` (`QuestionPatch`) → `Question`.
pub async fn eval_question_update(
    http: &reqwest::Client,
    p: &Plane<'_>,
    id: Uuid,
    patch: Value,
) -> ApiResult<Value> {
    let path = format!("/api/eval/questions/{id}");
    let r = p
        .call("eval_question_update", Method::PATCH, &path)
        .json(patch)
        .send(http)
        .await?;
    Ok(r.body)
}

/// `DELETE /api/eval/questions/{id}` → 204.
pub async fn eval_question_delete(
    http: &reqwest::Client,
    p: &Plane<'_>,
    id: Uuid,
) -> ApiResult<()> {
    let path = format!("/api/eval/questions/{id}");
    p.call("eval_question_delete", Method::DELETE, &path)
        .send(http)
        .await?;
    Ok(())
}

/// `POST /api/eval/questions/import` as JSON `{questions}` (all or nothing) → 201 `{added, total}`. CSV is parsed
/// agent-side and sent as JSON.
pub async fn eval_questions_import(
    http: &reqwest::Client,
    p: &Plane<'_>,
    questions: Vec<Value>,
) -> ApiResult<Value> {
    let r = p
        .call(
            "eval_questions_import",
            Method::POST,
            "/api/eval/questions/import",
        )
        .json(json!({ "questions": questions }))
        .send(http)
        .await?;
    Ok(r.body)
}

/// A tenant eval's options (`EvalBody` minus `source` and `root_id`, which [`eval_run`] sets).
#[derive(Clone, Debug, Default, Serialize)]
pub struct EvalReq {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub modes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub beam: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question_ids: Option<Vec<Uuid>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub questions: Option<Vec<Value>>,
}

/// `POST /api/eval {source: "tenant", root_id, …}` → 202 with the `running` `EvalRunSummary` (D91: always `root_id`).
/// 409 `eval_running` / `eval_not_ready`, 404 for an unknown root.
pub async fn eval_run(
    http: &reqwest::Client,
    p: &Plane<'_>,
    root: StrataStoreId,
    req: &EvalReq,
) -> ApiResult<Value> {
    let mut body = serde_json::to_value(req).map_err(|e| ApiError::Internal(e.into()))?;
    if let Value::Object(m) = &mut body {
        m.insert("source".into(), json!("tenant"));
        m.insert("root_id".into(), json!(root));
    }
    let r = p
        .call("eval_run", Method::POST, "/api/eval")
        .json(body)
        .send(http)
        .await
        .map_err(|e| note_ambiguous("eval_run", e))?;
    Ok(r.body)
}

/// `GET /api/eval/runs?limit=&cursor=` → `{items: [EvalRunSummary], next}`.
pub async fn eval_runs(
    http: &reqwest::Client,
    p: &Plane<'_>,
    limit: Option<i64>,
    cursor: Option<&str>,
) -> ApiResult<Value> {
    let qs = query_string(&[
        ("limit", limit.map(|l| l.to_string())),
        ("cursor", cursor.map(str::to_string)),
    ]);
    let path = format!("/api/eval/runs{qs}");
    let r = p.call("eval_runs", Method::GET, &path).send(http).await?;
    Ok(r.body)
}

/// `GET /api/eval/runs/{id}` → `EvalRun` (with `results`).
pub async fn eval_run_get(http: &reqwest::Client, p: &Plane<'_>, id: Uuid) -> ApiResult<Value> {
    let path = format!("/api/eval/runs/{id}");
    let r = p
        .call("eval_run_get", Method::GET, &path)
        .send(http)
        .await?;
    Ok(r.body)
}

// ---------------------------------------------------------------------------------------------------------------
// Platform packs (R10 reads; PK-5 owner listing; PK-9 curation; PK-10 exclusions)
//
// strata_rag `src/api/platform.rs`. Every `/api/platform/*` route is 404 `not_found` ("platform knowledge is not
// enabled") while strata's `STRATA_PLATFORM_OWNER` is unset. The reads map that to "nothing" (an empty list,
// `None`); the writes map it to 503 `platform_knowledge_off` ([`platform_knowledge_off`]). Pack writes are the
// platform owner's only: on any other tenant's plane strata answers 403 `forbidden`, which these clients map to 403
// `platform_owner_required` (crate::upstream::refusal would make every 403 a 503, since a 403 there means our own
// credentials). Every other refusal goes through crate::upstream::refusal (R3 allow-list: `pack_published`,
// `duplicate_name`, `invalid_target`, `invalid_input`, `not_found` keep their status).

/// `PlatformPack` (strata `src/db/platform.rs`). On a tenant's plane the listing holds the packs the tenant may
/// read, without retired ones: published and not excluded. On the platform owner's plane (the
/// `AGENT_PLATFORM_TENANT_ID` pair) it holds every pack, drafts (`published_at: null`) and retired ones included
/// (PK-5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformPack {
    pub key: String,
    pub version: i32,
    pub root_id: StrataStoreId,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub retired_at: Option<String>,
}

#[derive(Deserialize)]
struct PackList {
    packs: Vec<PlatformPack>,
}

/// `GET /api/platform/packs`; 404 `not_found` (platform knowledge off) is an empty list. With the platform owner's
/// plane this is the owner listing: drafts and retired packs included (see [`PlatformPack`]).
pub async fn platform_packs(http: &reqwest::Client, p: &Plane<'_>) -> ApiResult<Vec<PlatformPack>> {
    match p
        .call("platform_packs", Method::GET, "/api/platform/packs")
        .send(http)
        .await
    {
        Ok(r) => Ok(p.decode::<PackList>("platform_packs", r.body)?.packs),
        Err(e) if e.status_and_code() == (StatusCode::NOT_FOUND, "not_found") => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// One store of a pack's subtree (strata `db::platform::PackStore`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackStore {
    pub id: StrataStoreId,
    #[serde(default)]
    pub parent_id: Option<StrataStoreId>,
    pub kind: StoreKind,
    pub name: String,
    /// Store names from the pack root (included) down to this store.
    #[serde(default)]
    pub path: Vec<String>,
    /// `"<model>@<dims>"`: platform leaves are embedded with the pack's own model (handoff item 4).
    #[serde(default)]
    pub embed_model: String,
    #[serde(default)]
    pub chunk_count: i64,
}

/// `GET /api/platform/packs/{key}/{version}/stores` (strata `api::platform::PackStores`): the pack root's subtree,
/// root first, ordered by name path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackStores {
    /// `"<key>@<version>"`, as query hits label platform results.
    pub pack: String,
    pub root_id: StrataStoreId,
    pub stores: Vec<PackStore>,
}

impl PackStores {
    /// The subtree's leaves, in strata's order: what a query names in `leaf_ids` (PK-7).
    pub fn leaf_ids(&self) -> Vec<StrataStoreId> {
        self.stores
            .iter()
            .filter(|s| s.kind == StoreKind::Leaf)
            .map(|s| s.id)
            .collect()
    }
}

/// `/api/platform/packs/{key}/{version}` + `tail`, the key percent-encoded as one segment (`industry%2Fretail`).
fn pack_path(key: &str, version: i32, tail: &str) -> String {
    format!("/api/platform/packs/{}/{version}{tail}", enc(key))
}

/// `GET /api/platform/packs/{key}/{version}/stores`. `Ok(None)` for any 404: the pack is not readable by this
/// plane's tenant (excluded, a draft, missing) or platform knowledge is off. A retired pack stays readable (spec §3).
pub async fn pack_stores(
    http: &reqwest::Client,
    p: &Plane<'_>,
    key: &str,
    version: i32,
) -> ApiResult<Option<PackStores>> {
    let path = pack_path(key, version, "/stores");
    match p.call("pack_stores", Method::GET, &path).send(http).await {
        Ok(r) => p.decode("pack_stores", r.body).map(Some),
        Err(e) if e.status_and_code() == (StatusCode::NOT_FOUND, "not_found") => Ok(None),
        Err(e) => Err(e),
    }
}

/// 503 `platform_knowledge_off`: a platform write while strata's platform knowledge is off.
pub fn platform_knowledge_off() -> ApiError {
    ApiError::Unavailable(
        "platform_knowledge_off",
        "Platform knowledge is not enabled in this deployment.".to_string(),
        None,
    )
}

/// strata's 404 for every `/api/platform/*` route while `STRATA_PLATFORM_OWNER` is unset.
fn is_platform_off(r: &Reply) -> bool {
    r.status == StatusCode::NOT_FOUND
        && r.body["error"]["code"] == "not_found"
        && r.body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("platform knowledge is not enabled"))
}

/// A platform route's answer: the 2xx body, 503 [`platform_knowledge_off`], 403 `platform_owner_required` for
/// strata's `forbidden`, else crate::upstream::refusal.
fn platform_answer(p: &Plane<'_>, r: Reply) -> ApiResult<Value> {
    if r.status.is_success() {
        return Ok(r.body);
    }
    if is_platform_off(&r) {
        return Err(platform_knowledge_off());
    }
    if r.status == StatusCode::FORBIDDEN && r.body["error"]["code"] == "forbidden" {
        return Err(ApiError::Forbidden(
            "platform_owner_required",
            "Only the platform owner manages platform packs; check AGENT_PLATFORM_TENANT_ID."
                .to_string(),
        ));
    }
    Err(crate::upstream::refusal(
        p.service().name(),
        r.status,
        &r.body,
    ))
}

/// strata's 422 `invalid_target` on a pack route is about the pack, not an address: crate::upstream::refusal would
/// give it the connector-URL message ("must be a public https address"), so the pack routes map it to their own
/// `code` and `message` (T10).
fn pack_target(r: &Reply, code: &'static str, message: String) -> Option<ApiError> {
    if r.status == StatusCode::UNPROCESSABLE_ENTITY && r.body["error"]["code"] == "invalid_target" {
        Some(ApiError::Unprocessable(code, message))
    } else {
        None
    }
}

/// `POST /api/platform/packs {key, version, root_id}` → 201 draft (platform owner only). 404 for a root that is not
/// the owner's, 422 `invalid_pack_root` for a non-root (strata's `invalid_target`), 409 `duplicate_name` when
/// `key@version` exists or the store already roots a pack.
pub async fn pack_register(
    http: &reqwest::Client,
    p: &Plane<'_>,
    key: &str,
    version: i32,
    root_id: StrataStoreId,
) -> ApiResult<PlatformPack> {
    let r = p
        .call("pack_register", Method::POST, "/api/platform/packs")
        .json(json!({"key": key, "version": version, "root_id": root_id}))
        .send_any(http)
        .await?;
    if let Some(e) = pack_target(
        &r,
        "invalid_pack_root",
        format!("Store {root_id} has a parent: a pack's root must be a top-level store."),
    ) {
        return Err(e);
    }
    p.decode("pack_register", platform_answer(p, r)?)
}

/// `POST /api/platform/packs/{key}/{version}/publish`: freezes the pack (idempotent; platform owner only). Writes
/// into its subtree then answer 409 `pack_published`.
pub async fn pack_publish(
    http: &reqwest::Client,
    p: &Plane<'_>,
    key: &str,
    version: i32,
) -> ApiResult<PlatformPack> {
    let path = pack_path(key, version, "/publish");
    let r = p
        .call("pack_publish", Method::POST, &path)
        .send_any(http)
        .await?;
    p.decode("pack_publish", platform_answer(p, r)?)
}

/// `POST /api/platform/packs/{key}/{version}/retire`: hides a published pack from tenant listings (idempotent;
/// platform owner only). Pins keep reading it. A draft is strata's 422 `invalid_target`, answered as 422
/// `pack_not_published`: strata has no draft delete; a draft goes with its root store (store delete).
pub async fn pack_retire(
    http: &reqwest::Client,
    p: &Plane<'_>,
    key: &str,
    version: i32,
) -> ApiResult<PlatformPack> {
    let path = pack_path(key, version, "/retire");
    let r = p
        .call("pack_retire", Method::POST, &path)
        .send_any(http)
        .await?;
    if let Some(e) = pack_target(
        &r,
        "pack_not_published",
        format!(
            "{key}@{version} is a draft: only a published pack can be retired. To drop the draft, delete its root \
             store."
        ),
    ) {
        return Err(e);
    }
    p.decode("pack_retire", platform_answer(p, r)?)
}

/// `PlatformExclusion`: the tenant is excluded from `key_prefix` and every key below it, segment-wise
/// (`industry/retail` covers `industry/retail/hr`, not `industry/retailx`), in every version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Exclusion {
    pub key_prefix: String,
    pub reason: String,
    #[serde(default)]
    pub created_at: String,
}

#[derive(Deserialize)]
struct ExclusionList {
    exclusions: Vec<Exclusion>,
}

/// `GET /api/platform/exclusions`: this plane's tenant's own exclusions; platform knowledge off is an empty list.
pub async fn exclusions(http: &reqwest::Client, p: &Plane<'_>) -> ApiResult<Vec<Exclusion>> {
    let r = p
        .call("exclusions", Method::GET, "/api/platform/exclusions")
        .send_any(http)
        .await?;
    if is_platform_off(&r) {
        return Ok(Vec::new());
    }
    Ok(
        p.decode::<ExclusionList>("exclusions", platform_answer(p, r)?)?
            .exclusions,
    )
}

/// `PUT /api/platform/exclusions {key_prefix, reason}` (upsert) → the stored exclusion. 422 `invalid_input` for a
/// prefix that is not a pack key or a blank reason; 503 [`platform_knowledge_off`].
pub async fn exclusion_put(
    http: &reqwest::Client,
    p: &Plane<'_>,
    key_prefix: &str,
    reason: &str,
) -> ApiResult<Exclusion> {
    let r = p
        .call("exclusion_put", Method::PUT, "/api/platform/exclusions")
        .json(json!({"key_prefix": key_prefix, "reason": reason}))
        .send_any(http)
        .await?;
    p.decode("exclusion_put", platform_answer(p, r)?)
}

/// `DELETE /api/platform/exclusions?key_prefix=` (percent-encoded) → 204. 404 `not_found` when the tenant has no
/// exclusion with exactly that prefix; 503 [`platform_knowledge_off`].
pub async fn exclusion_delete(
    http: &reqwest::Client,
    p: &Plane<'_>,
    key_prefix: &str,
) -> ApiResult<()> {
    let qs = query_string(&[("key_prefix", Some(key_prefix.to_string()))]);
    let path = format!("/api/platform/exclusions{qs}");
    let r = p
        .call("exclusion_delete", Method::DELETE, &path)
        .send_any(http)
        .await?;
    platform_answer(p, r).map(|_| ())
}

// ---------------------------------------------------------------------------------------------------------------
// JSON document ingest (strata_rag `POST /api/stores/{id}/documents`; PK-9 curation)

/// `provenance` (strata `src/ingest.rs` `Provenance`): required, with an allow-listed licence, in a platform store.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub source_url: String,
    /// SPDX-style licence id (`CC-BY-4.0`, `public-domain`, ...).
    pub license: String,
    /// RFC 3339 timestamp of retrieval.
    pub retrieved_at: String,
}

/// The JSON ingest body (strata `IngestJson`); unset fields are left out.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DocumentIn {
    pub text: String,
    /// The citation label (strata default `api`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_run: Option<String>,
    /// A JSON object.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Topic keys the document covers (platform documents).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub topic_keys: Vec<String>,
    /// Topic keys the document overrides (tenant documents shadowing platform ones).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub supersedes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Provenance>,
}

/// Longest strata message a 422 ingest refusal carries.
const INGEST_MESSAGE_CHARS: usize = 300;

/// `POST /api/stores/{id}/documents` as JSON (timeout [`LONG_TIMEOUT`]) → strata's `IngestOutcome` (`status:
/// created | existing` plus the document row). A 422 `invalid_input` keeps **strata's** message (cut to 300 chars),
/// unlike every other refusal: the curator (super_admin) must read "provenance is required" / "licence `x` is not
/// allowed". 409 `pack_published` and the rest go through crate::upstream::refusal.
pub async fn ingest_document_json(
    http: &reqwest::Client,
    p: &Plane<'_>,
    store: StrataStoreId,
    doc: &DocumentIn,
) -> ApiResult<Value> {
    let body = serde_json::to_value(doc).map_err(|e| ApiError::Internal(e.into()))?;
    let path = format!("/api/stores/{store}/documents");
    let r = p
        .call("ingest_document_json", Method::POST, &path)
        .json(body)
        .timeout(LONG_TIMEOUT)
        .send_any(http)
        .await?;
    if r.status == StatusCode::UNPROCESSABLE_ENTITY && r.body["error"]["code"] == "invalid_input" {
        let message: String = r.body["error"]["message"]
            .as_str()
            .unwrap_or("The knowledge service could not accept that document.")
            .chars()
            .take(INGEST_MESSAGE_CHARS)
            .collect();
        return Err(ApiError::Unprocessable("invalid_input", message));
    }
    platform_answer(p, r)
}
