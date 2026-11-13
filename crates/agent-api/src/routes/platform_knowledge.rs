//! Platform knowledge curation (PK T8; plan PK-9, handoff item 6): a super_admin curates the strata platform owner's
//! stores, documents and packs, and references a published pack from a template layer. Every route is `SuperAdmin`
//! and acts on the platform plane (`AGENT_PLATFORM_TENANT_ID`, crate::knowledge::packs::curation::platform_plane):
//! unset → 503 `platform_tenant_unset`. The agent keeps no `knowledge_store` rows for platform stores (strata is the
//! source of truth). strata's errors pass through (409 `pack_published`, 422 licence refusals with strata's message,
//! 403 `platform_owner_required`, 503 `platform_knowledge_off`). No transaction is open across a strata call.
//!
//! Prefix `/api/platform/knowledge`:
//! - `GET /stores`, `POST /stores {name, kind?: leaf|router, parent_id?, description?}`,
//!   `DELETE /stores/{id}?recursive=`.
//! - `GET /stores/{id}/documents?limit=&cursor=&source_id=`, `DELETE /documents/{id}`.
//! - `POST /stores/{id}/documents {text, title?, source_id?, topic_keys, provenance {source_url, license,
//!   retrieved_at}}`: strata_rag JSON ingest (inline text).
//! - `POST /stores/{id}/ingest-jobs {urls, topic_keys, license, retrieved_at?}`: strata_ingest URL jobs (#15), each
//!   URL its own `provenance.source_url`; strata fetches, chunks and retries. A refused licence fails the job with
//!   `provenance_rejected`.
//! - `POST /stores/{id}/upload` (multipart: `topic_keys` = a JSON array, `provenance` = JSON `{source_url, license,
//!   retrieved_at?}`, then 1..=20 `file` parts of the tenant upload types, 50 MB in all): strata_ingest file jobs
//!   (#15), one per file. The agent checks every field (422 `invalid_topic_keys` / `invalid_provenance` /
//!   `no_file` / `too_many_files`, 413 `too_large`, 415 `unsupported_type`) before strata, and sends `topic_keys` and
//!   `provenance` as text fields BEFORE the first `file` part, whatever order the curator's form used.
//! - `GET /ingest-jobs/{id}`: the strata_ingest job view plus `curator_status` / `curator_message`
//!   (crate::knowledge::packs::curation::curator_job): a job strata failed with `provenance_rejected` reads
//!   `licence_refused` with what to do.
//! - `GET /packs` (the owner listing: drafts and retired included), `POST /packs {key, version, store_id}`,
//!   `POST /packs/{key}/{version}/publish`, `POST /packs/{key}/{version}/retire` (`{key}` percent-encoded; a draft is
//!   422 `pack_not_published`: strata drops a draft with its root store), `GET /packs/{key}/{version}/stores` (the
//!   pack root's subtree, strata `PackStores`, drafts and retired packs included; 404 `pack_not_found`).
//! - `POST /packs/{key}/{version}/references {layer, stage, kind}` → `{row, tag: "<family>@v<N>"}` (409
//!   `pack_unpublished` / `pack_retired`, 404 `pack_not_found`); `DELETE /packs/{key}/references?layer=&stage=&kind=`
//!   → a tombstone and a tag.
//!
//! Audited in the platform log (`platform.knowledge_store.*`, `platform.knowledge_document.*`,
//! `platform.knowledge_pack.*`) with ids and keys only, never document text.
use axum::extract::multipart::MultipartRejection;
use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::audit::{Actor, PlatformEvent};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::knowledge::packs::curation::{self, platform_plane, record};
use crate::routes::knowledge;
use crate::state::AppState;
use crate::strata::rag::{self, DocumentIn, DocumentsQuery, NewStore, Provenance, StoreKind};
use crate::strata::{Service, StrataStoreId, ingest};
use crate::tenancy::SuperAdmin;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/platform/knowledge/stores",
            get(list_stores).post(create_store),
        )
        .route("/api/platform/knowledge/stores/{id}", delete(delete_store))
        .route(
            "/api/platform/knowledge/stores/{id}/documents",
            get(list_documents).post(ingest_document),
        )
        .route(
            "/api/platform/knowledge/stores/{id}/ingest-jobs",
            post(ingest_urls),
        )
        .route(
            "/api/platform/knowledge/stores/{id}/upload",
            post(upload_files).layer(DefaultBodyLimit::max(knowledge::UPLOAD_BODY_LIMIT)),
        )
        .route("/api/platform/knowledge/ingest-jobs/{id}", get(ingest_job))
        .route(
            "/api/platform/knowledge/documents/{id}",
            delete(delete_document),
        )
        .route(
            "/api/platform/knowledge/packs",
            get(list_packs).post(register_pack),
        )
        .route(
            "/api/platform/knowledge/packs/{key}/{version}/publish",
            post(publish_pack),
        )
        .route(
            "/api/platform/knowledge/packs/{key}/{version}/retire",
            post(retire_pack),
        )
        .route(
            "/api/platform/knowledge/packs/{key}/{version}/stores",
            get(pack_stores),
        )
        .route(
            "/api/platform/knowledge/packs/{key}/{version}/references",
            post(reference_pack),
        )
        .route(
            "/api/platform/knowledge/packs/{key}/references",
            delete(unreference_pack),
        )
}

/// The longest store name / title the agent passes on.
const MAX_NAME: usize = 200;

fn check_name(raw: &str, what: &'static str, code: &'static str) -> ApiResult<String> {
    let s = raw.trim();
    let n = s.chars().count();
    if n == 0 || n > MAX_NAME || s.chars().any(char::is_control) {
        return Err(ApiError::Unprocessable(
            code,
            format!("{what} must be one line of 1 to {MAX_NAME} characters."),
        ));
    }
    Ok(s.to_string())
}

// ---------------------------------------------------------------------------------------------------------------
// Stores

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreInput {
    name: String,
    #[serde(default)]
    kind: Option<StoreKind>,
    #[serde(default)]
    parent_id: Option<Uuid>,
    #[serde(default)]
    description: Option<String>,
}

#[derive(Deserialize)]
struct DeleteStoreQuery {
    #[serde(default)]
    recursive: Option<bool>,
}

async fn list_stores(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let p = platform_plane(&st, Service::Rag, &c.sub).await?;
    let stores = rag::stores(&st.http, &p).await?;
    Ok(Json(json!(stores)))
}

async fn create_store(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<StoreInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let name = check_name(&b.name, "name", "invalid_name")?;
    let description = b.description.unwrap_or_default().trim().to_string();
    let p = platform_plane(&st, Service::Rag, &c.sub).await?;
    let view = rag::create_store(
        &st.http,
        &p,
        &NewStore {
            parent_id: b.parent_id.map(StrataStoreId),
            kind: b.kind.unwrap_or(StoreKind::Leaf),
            name: name.clone(),
            description,
        },
    )
    .await?;
    let ev = PlatformEvent::new("platform.knowledge_store.create", name.as_str())
        .detail("store_id", view.id.to_string())
        .detail("name", name);
    record(&st, &Actor::person(&c), ev).await?;
    Ok((StatusCode::CREATED, Json(json!(view))))
}

async fn delete_store(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(q): ApiQuery<DeleteStoreQuery>,
) -> ApiResult<StatusCode> {
    let recursive = q.recursive.unwrap_or(false);
    let p = platform_plane(&st, Service::Rag, &c.sub).await?;
    rag::delete_store(&st.http, &p, StrataStoreId(id), recursive).await?;
    let ev = PlatformEvent::new("platform.knowledge_store.delete", id.to_string())
        .detail("store_id", id.to_string())
        .detail("recursive", recursive);
    record(&st, &Actor::person(&c), ev).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------------------------
// Documents

#[derive(Deserialize)]
struct DocumentsParams {
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    source_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentInput {
    text: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    source_id: Option<String>,
    #[serde(default)]
    topic_keys: Vec<String>,
    provenance: Provenance,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UrlJobsInput {
    urls: Vec<String>,
    #[serde(default)]
    topic_keys: Vec<String>,
    license: String,
    #[serde(default)]
    retrieved_at: Option<String>,
}

async fn list_documents(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiQuery(q): ApiQuery<DocumentsParams>,
) -> ApiResult<Json<Value>> {
    let p = platform_plane(&st, Service::Rag, &c.sub).await?;
    let page = rag::documents(
        &st.http,
        &p,
        StrataStoreId(id),
        &DocumentsQuery {
            limit: q.limit,
            cursor: q.cursor,
            source_id: q.source_id,
        },
    )
    .await?;
    Ok(Json(json!(page)))
}

async fn ingest_document(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(b): ApiJson<DocumentInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if b.text.trim().is_empty() {
        return Err(ApiError::Unprocessable(
            "invalid_text",
            "A platform document needs text.".to_string(),
        ));
    }
    let topic_keys = curation::check_topic_keys(&b.topic_keys)?;
    let provenance = curation::check_provenance(&b.provenance)?;
    let title = match b.title.as_deref() {
        Some(t) if !t.trim().is_empty() => Some(check_name(t, "title", "invalid_title")?),
        _ => None,
    };
    let source_id = match b.source_id.as_deref() {
        Some(s) if !s.trim().is_empty() => Some(check_name(s, "source_id", "invalid_source_id")?),
        _ => None,
    };
    let p = platform_plane(&st, Service::Rag, &c.sub).await?;
    let doc = DocumentIn {
        text: b.text,
        title,
        source_id,
        topic_keys: topic_keys.clone(),
        provenance: Some(provenance.clone()),
        ..DocumentIn::default()
    };
    let out = rag::ingest_document_json(&st.http, &p, StrataStoreId(id), &doc).await?;
    let doc_id = out["id"].as_str().unwrap_or_default().to_string();
    let created = out["status"] != "existing";
    let ev = PlatformEvent::new("platform.knowledge_document.ingest", doc_id.as_str())
        .detail("store_id", id.to_string())
        .detail("document_id", doc_id)
        .detail("topic_keys", topic_keys)
        .detail("license", provenance.license)
        .detail("created", created);
    record(&st, &Actor::person(&c), ev).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(out)))
}

async fn ingest_urls(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    ApiJson(b): ApiJson<UrlJobsInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if b.urls.is_empty() || b.urls.len() > curation::MAX_URLS {
        return Err(ApiError::Unprocessable(
            "invalid_urls",
            format!("Send 1 to {} URLs.", curation::MAX_URLS),
        ));
    }
    let urls = b
        .urls
        .iter()
        .map(|u| curation::check_public_url(u, "invalid_urls"))
        .collect::<ApiResult<Vec<String>>>()?;
    let topic_keys = curation::check_topic_keys(&b.topic_keys)?;
    let license = curation::check_license(&b.license)?;
    let retrieved_at = match b.retrieved_at.as_deref() {
        Some(r) => curation::check_retrieved_at(r)?,
        None => chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    let items: Vec<Value> = urls
        .iter()
        .map(|u| {
            json!({"url": u, "provenance": {"source_url": u, "license": license, "retrieved_at": retrieved_at}})
        })
        .collect();
    let body = json!({"store_id": id, "topic_keys": topic_keys, "urls": items});
    let p = platform_plane(&st, Service::Ingest, &c.sub).await?;
    let created = ingest::create_json(&st.http, &p, body).await?;
    let jobs: Vec<String> = created.jobs.iter().map(|j| j.job_id.to_string()).collect();
    let ev = PlatformEvent::new("platform.knowledge_document.ingest_urls", id.to_string())
        .detail("store_id", id.to_string())
        .detail("jobs", jobs)
        .detail("topic_keys", topic_keys)
        .detail("license", license);
    record(&st, &Actor::person(&c), ev).await?;
    Ok((StatusCode::ACCEPTED, Json(json!(created))))
}

/// A curator upload's provenance field: `retrieved_at` defaults to now.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadProvenance {
    source_url: String,
    license: String,
    #[serde(default)]
    retrieved_at: Option<String>,
}

/// One file of a curator upload.
struct CuratorFile {
    name: String,
    content_type: Option<String>,
    data: Vec<u8>,
}

/// A curator upload as read: the files and the two raw JSON text fields.
struct CuratorForm {
    files: Vec<CuratorFile>,
    topic_keys: Option<String>,
    provenance: Option<String>,
}

/// The form's `file` parts (the tenant upload's limits and types), `topic_keys` and `provenance`; any other field
/// is skipped. Refused as soon as a limit is crossed, so nothing past it is read.
async fn read_curator_form(mut form: Multipart) -> ApiResult<CuratorForm> {
    let mut out = CuratorForm {
        files: Vec::new(),
        topic_keys: None,
        provenance: None,
    };
    let mut total = 0usize;
    while let Some(mut field) = form.next_field().await.map_err(knowledge::upload_error)? {
        let field_name = field.name().unwrap_or_default().to_string();
        match field_name.as_str() {
            "file" => {
                if out.files.len() == knowledge::UPLOAD_MAX_FILES {
                    return Err(ApiError::Unprocessable(
                        "too_many_files",
                        format!(
                            "One upload takes at most {} files.",
                            knowledge::UPLOAD_MAX_FILES
                        ),
                    ));
                }
                let name = field.file_name().unwrap_or_default().trim().to_string();
                if !knowledge::UPLOAD_TYPES.contains(&knowledge::extension(&name).as_str()) {
                    return Err(ApiError::Unsupported(
                        "unsupported_type",
                        format!(
                            "{name}: not a supported type ({}).",
                            knowledge::UPLOAD_TYPES.join(", ")
                        ),
                    ));
                }
                let content_type = field.content_type().map(str::to_string);
                let mut data = Vec::new();
                while let Some(chunk) = field.chunk().await.map_err(knowledge::upload_error)? {
                    total += chunk.len();
                    if total > knowledge::UPLOAD_MAX_BYTES {
                        return Err(knowledge::too_large());
                    }
                    data.extend_from_slice(&chunk);
                }
                out.files.push(CuratorFile {
                    name,
                    content_type,
                    data,
                });
            }
            "topic_keys" => {
                out.topic_keys = Some(field.text().await.map_err(knowledge::upload_error)?);
            }
            "provenance" => {
                out.provenance = Some(field.text().await.map_err(knowledge::upload_error)?);
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The `topic_keys` field: a JSON array of strings, then [`curation::check_topic_keys`].
fn upload_topic_keys(raw: Option<&str>) -> ApiResult<Vec<String>> {
    let keys: Vec<String> = raw
        .and_then(|r| serde_json::from_str(r).ok())
        .ok_or_else(|| {
            ApiError::Unprocessable(
                "invalid_topic_keys",
                "topic_keys must be a JSON array of topic keys, such as [\"hr/leave\"]."
                    .to_string(),
            )
        })?;
    curation::check_topic_keys(&keys)
}

/// The `provenance` field: JSON `{source_url, license, retrieved_at?}`, then [`curation::check_provenance`].
fn upload_provenance(raw: Option<&str>) -> ApiResult<Provenance> {
    let p: UploadProvenance = raw
        .and_then(|r| serde_json::from_str(r).ok())
        .ok_or_else(|| {
            ApiError::Unprocessable(
                "invalid_provenance",
                "provenance must be JSON {source_url, license, retrieved_at?}.".to_string(),
            )
        })?;
    curation::check_provenance(&Provenance {
        source_url: p.source_url,
        license: p.license,
        retrieved_at: p.retrieved_at.unwrap_or_else(|| {
            chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        }),
    })
}

/// Curator file upload into a platform store (strata_ingest #15 multipart): every field is checked before strata;
/// `topic_keys` and `provenance` go first, then one `file` part per file. A licence strata refuses fails each job
/// with `provenance_rejected` (read it with `GET /ingest-jobs/{id}`).
async fn upload_files(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
    form: Result<Multipart, MultipartRejection>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    if st.cfg.platform_tenant_id.is_none() {
        return Err(curation::platform_tenant_unset());
    }
    let form = form.map_err(|_| {
        ApiError::BadRequest(
            "invalid_body",
            "the body must be multipart/form-data".into(),
        )
    })?;
    let CuratorForm {
        files,
        topic_keys,
        provenance,
    } = read_curator_form(form).await?;
    let topic_keys = upload_topic_keys(topic_keys.as_deref())?;
    let provenance = upload_provenance(provenance.as_deref())?;
    if files.is_empty() {
        return Err(ApiError::Unprocessable(
            "no_file",
            "Pick at least one file.".into(),
        ));
    }
    let mut req = reqwest::multipart::Form::new()
        .text("store_id", id.to_string())
        .text("topic_keys", json!(topic_keys).to_string())
        .text("provenance", json!(provenance).to_string());
    let mut names: Vec<String> = Vec::with_capacity(files.len());
    for f in files {
        let mut part = reqwest::multipart::Part::bytes(f.data).file_name(f.name.clone());
        if let Some(ct) = &f.content_type {
            part = part.mime_str(ct).map_err(|_| {
                ApiError::BadRequest(
                    "invalid_body",
                    "An uploaded file has an invalid content type.".into(),
                )
            })?;
        }
        names.push(f.name);
        req = req.part("file", part);
    }
    let p = platform_plane(&st, Service::Ingest, &c.sub).await?;
    let created = ingest::create_multipart(&st.http, &p, req).await?;
    let jobs: Vec<String> = created.jobs.iter().map(|j| j.job_id.to_string()).collect();
    let ev = PlatformEvent::new("platform.knowledge_document.upload", id.to_string())
        .detail("store_id", id.to_string())
        .detail("jobs", jobs)
        .detail("files", names)
        .detail("topic_keys", topic_keys)
        .detail("license", provenance.license);
    record(&st, &Actor::person(&c), ev).await?;
    Ok((StatusCode::ACCEPTED, Json(json!(created))))
}

/// One strata_ingest job of the platform owner, with the curator status ([`curation::curator_job`]).
async fn ingest_job(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<Json<Value>> {
    let p = platform_plane(&st, Service::Ingest, &c.sub).await?;
    let job = ingest::get(&st.http, &p, id).await?;
    Ok(Json(curation::curator_job(job)))
}

async fn delete_document(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<Uuid>,
) -> ApiResult<StatusCode> {
    let p = platform_plane(&st, Service::Rag, &c.sub).await?;
    rag::delete_document(&st.http, &p, id).await?;
    let ev = PlatformEvent::new("platform.knowledge_document.delete", id.to_string())
        .detail("document_id", id.to_string());
    record(&st, &Actor::person(&c), ev).await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------------------------------------------
// Packs

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterInput {
    key: String,
    version: i64,
    store_id: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReferenceInput {
    layer: String,
    stage: String,
    kind: String,
}

#[derive(Deserialize)]
struct UnreferenceQuery {
    layer: String,
    stage: String,
    kind: String,
}

async fn list_packs(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let p = platform_plane(&st, Service::Rag, &c.sub).await?;
    let packs = rag::platform_packs(&st.http, &p).await?;
    Ok(Json(json!(packs)))
}

async fn register_pack(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<RegisterInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let key = curation::check_pack_key(&b.key)?;
    let version = curation::check_pack_version(b.version)?;
    let p = platform_plane(&st, Service::Rag, &c.sub).await?;
    let pack = rag::pack_register(&st.http, &p, &key, version, StrataStoreId(b.store_id)).await?;
    let ev = PlatformEvent::new("platform.knowledge_pack.register", key.as_str())
        .detail("key", key.as_str())
        .detail("version", version)
        .detail("root_id", b.store_id.to_string());
    record(&st, &Actor::person(&c), ev).await?;
    Ok((StatusCode::CREATED, Json(json!(pack))))
}

/// The `{key}` / `{version}` path pair, checked.
fn pack_ref(key: &str, version: i64) -> ApiResult<(String, i32)> {
    Ok((
        curation::check_pack_key(key)?,
        curation::check_pack_version(version)?,
    ))
}

async fn publish_pack(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath((key, version)): ApiPath<(String, i64)>,
) -> ApiResult<Json<Value>> {
    let (key, version) = pack_ref(&key, version)?;
    let p = platform_plane(&st, Service::Rag, &c.sub).await?;
    let pack = rag::pack_publish(&st.http, &p, &key, version).await?;
    let ev = PlatformEvent::new("platform.knowledge_pack.publish", key.as_str())
        .detail("key", key.as_str())
        .detail("version", version);
    record(&st, &Actor::person(&c), ev).await?;
    Ok(Json(json!(pack)))
}

async fn retire_pack(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath((key, version)): ApiPath<(String, i64)>,
) -> ApiResult<Json<Value>> {
    let (key, version) = pack_ref(&key, version)?;
    let p = platform_plane(&st, Service::Rag, &c.sub).await?;
    let pack = rag::pack_retire(&st.http, &p, &key, version).await?;
    let ev = PlatformEvent::new("platform.knowledge_pack.retire", key.as_str())
        .detail("key", key.as_str())
        .detail("version", version);
    record(&st, &Actor::person(&c), ev).await?;
    Ok(Json(json!(pack)))
}

/// The pack root's subtree on the platform plane (the owner reads every pack: drafts and retired ones too). A read:
/// not audited. strata's 404 (no such pack, or platform knowledge off) is 404 `pack_not_found`.
async fn pack_stores(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath((key, version)): ApiPath<(String, i64)>,
) -> ApiResult<Json<Value>> {
    let (key, version) = pack_ref(&key, version)?;
    let p = platform_plane(&st, Service::Rag, &c.sub).await?;
    match rag::pack_stores(&st.http, &p, &key, version).await? {
        Some(stores) => Ok(Json(json!(stores))),
        None => Err(ApiError::Missing(
            "pack_not_found",
            format!("No platform pack {key}@{version}."),
        )),
    }
}

/// PK-9: the pack must be published and not retired in the owner listing (read before the transaction), then one
/// platform transaction writes the `knowledge_pack` row and tags the layer's family.
async fn reference_pack(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath((key, version)): ApiPath<(String, i64)>,
    ApiJson(b): ApiJson<ReferenceInput>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let (key, version) = pack_ref(&key, version)?;
    let p = platform_plane(&st, Service::Rag, &c.sub).await?;
    let owner_packs = rag::platform_packs(&st.http, &p).await?;
    curation::check_referenceable(&owner_packs, &key, version)?;
    let row_key = curation::reference_key(&b.stage, &b.kind, &key);
    let done = curation::write_reference(
        &st,
        &Actor::person(&c),
        st.generator_guard.as_ref(),
        b.layer.trim(),
        &row_key,
        Some(version),
        &c.sub,
    )
    .await?;
    Ok((StatusCode::CREATED, Json(done.json())))
}

/// A tombstone of `<stage>|<kind>|<key>` on `layer` and a tag (no strata call: removing a reference never needs the
/// pack to exist).
async fn unreference_pack(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(key): ApiPath<String>,
    ApiQuery(q): ApiQuery<UnreferenceQuery>,
) -> ApiResult<Json<Value>> {
    let key = curation::check_pack_key(&key)?;
    if st.cfg.platform_tenant_id.is_none() {
        return Err(curation::platform_tenant_unset());
    }
    let row_key = curation::reference_key(&q.stage, &q.kind, &key);
    let done = curation::write_reference(
        &st,
        &Actor::person(&c),
        st.generator_guard.as_ref(),
        q.layer.trim(),
        &row_key,
        None,
        &c.sub,
    )
    .await?;
    Ok(Json(done.json()))
}
