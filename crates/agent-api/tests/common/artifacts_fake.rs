//! In-process artifact_service (axum on 127.0.0.1:0) for the SW-9 attachment store, mirroring the routes and error
//! shapes of artifact_service origin/main + fix d175304 (`src/routes/artifacts.py`, `src/auth/middleware.py`):
//!
//! - `POST /projects/{p}/artifacts/upload` (multipart `file`, `filename`, `content_type`, `kind`, `sensitivity`,
//!   `tags`), `GET …/artifacts/{id}/download`, `DELETE …/artifacts/{id}` (204; unknown → 404).
//! - `GET /projects/{p}/artifacts?tags=<tag>&limit=<n>` (artifact_service `list_artifacts`): a JSON array of the
//!   stored artifacts carrying that tag, each `{"id", "tags", ...}` (the sweep of upload intents finds orphans by
//!   `att:<id>`); counted in [`Artifacts::lists`].
//! - Auth: `Authorization: Bearer <the pinned licence>` AND `X-Tenant-Id: <the pinned tenant uuid>`; anything else is
//!   401 (bad token) / 403 (another tenant) and counted in [`Artifacts::bad_auth`] (a test asserts it is 0 on the happy paths). The project must be the
//!   fixed attachments project.
//! - [`Mode`]: how the next uploads answer (infected 422 `detail.code=malware`, scanner down 503
//!   `detail.code=scan_unavailable`, a bare 422 / 503 / 500).
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::json;
use uuid::Uuid;

/// The project every attachment lives in (crate::attachments::artifacts::PROJECT_ID).
pub const PROJECT: &str = "1ccdade5-c58a-5505-bd5d-20eea03d49eb";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Ok,
    /// 422 `{"detail": {"code": "malware"}, "error": {...}}`.
    Malware,
    /// 503 `{"detail": {"code": "scan_unavailable"}}`.
    ScanDown,
    /// 422 without a code (a validation error is not a conviction).
    Plain422,
    /// 503 without a code (an outage is not a scanner outage).
    Plain503,
    /// 500 `{"detail": "Failed to upload file"}`.
    Fail500,
}

/// One accepted upload as the fake saw it.
#[derive(Clone, Debug, Default)]
pub struct Upload {
    pub artifact: Uuid,
    pub filename: String,
    pub content_type: String,
    pub kind: String,
    pub sensitivity: String,
    pub tags: String,
    pub visibility: String,
    pub bytes: Vec<u8>,
}

#[derive(Default)]
struct Inner {
    /// artifact id → (content type, bytes)
    store: HashMap<Uuid, (String, Vec<u8>)>,
    uploads: Vec<Upload>,
    deletes: Vec<Uuid>,
    /// The (tenant, licence token) every request must carry.
    pin: Option<(Uuid, String)>,
}

#[derive(Clone, Default)]
pub struct Artifacts {
    pub url: String,
    inner: Arc<Mutex<Inner>>,
    mode: Arc<Mutex<Mode>>,
    /// Requests refused for their credentials (or project).
    pub bad_auth: Arc<AtomicUsize>,
    /// Every upload attempt, whatever its answer.
    pub upload_attempts: Arc<AtomicUsize>,
    /// Answer `DELETE` with 500.
    pub delete_fails: Arc<AtomicBool>,
    /// Answer the list with 500.
    pub list_fails: Arc<AtomicBool>,
    /// Every list request that passed authentication.
    pub lists: Arc<AtomicUsize>,
}

impl Artifacts {
    pub fn pin(&self, tenant: Uuid, token: &str) {
        self.inner.lock().unwrap().pin = Some((tenant, token.to_string()));
    }
    pub fn set_mode(&self, m: Mode) {
        *self.mode.lock().unwrap() = m;
    }
    pub fn uploads(&self) -> Vec<Upload> {
        self.inner.lock().unwrap().uploads.clone()
    }
    pub fn deletes(&self) -> Vec<Uuid> {
        self.inner.lock().unwrap().deletes.clone()
    }
    pub fn bytes_of(&self, artifact: Uuid) -> Option<Vec<u8>> {
        self.inner
            .lock()
            .unwrap()
            .store
            .get(&artifact)
            .map(|(_, b)| b.clone())
    }
    pub fn holds(&self, artifact: Uuid) -> bool {
        self.inner.lock().unwrap().store.contains_key(&artifact)
    }
    /// Put bytes in as if an earlier upload had stored them.
    pub fn put(&self, artifact: Uuid, content_type: &str, bytes: &[u8]) {
        self.inner
            .lock()
            .unwrap()
            .store
            .insert(artifact, (content_type.to_string(), bytes.to_vec()));
    }
    /// Put bytes in as if an earlier upload had stored them, tagged like the real one (`tags` comma-separated).
    pub fn put_tagged(&self, artifact: Uuid, content_type: &str, bytes: &[u8], tags: &str) {
        let mut g = self.inner.lock().unwrap();
        g.store
            .insert(artifact, (content_type.to_string(), bytes.to_vec()));
        g.uploads.push(Upload {
            artifact,
            content_type: content_type.to_string(),
            tags: tags.to_string(),
            bytes: bytes.to_vec(),
            ..Upload::default()
        });
    }
    pub fn stored(&self) -> usize {
        self.inner.lock().unwrap().store.len()
    }

    fn authorised(&self, h: &HeaderMap, project: &str) -> bool {
        let ok = {
            let g = self.inner.lock().unwrap();
            match &g.pin {
                Some((tenant, token)) => {
                    h.get("authorization").and_then(|v| v.to_str().ok())
                        == Some(format!("Bearer {token}").as_str())
                        && h.get("x-tenant-id").and_then(|v| v.to_str().ok())
                            == Some(tenant.to_string().as_str())
                }
                None => false,
            }
        } && project == PROJECT;
        if !ok {
            self.bad_auth.fetch_add(1, Ordering::SeqCst);
        }
        ok
    }
}

/// 401 `TENANT_PROOF_REQUIRED` for a bad (or missing) token, 403 `TENANT_PROOF_MISMATCH` for another tenant.
fn forbidden(a: &Artifacts, h: &HeaderMap) -> Response {
    let token_ok = {
        let g = a.inner.lock().unwrap();
        match &g.pin {
            Some((_, token)) => {
                h.get("authorization").and_then(|v| v.to_str().ok())
                    == Some(format!("Bearer {token}").as_str())
            }
            None => false,
        }
    };
    if token_ok {
        (
            StatusCode::FORBIDDEN,
            axum::Json(json!({"error": {"code": "TENANT_PROOF_MISMATCH", "status": 403}})),
        )
            .into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": {"code": "TENANT_PROOF_REQUIRED", "status": 401}})),
        )
            .into_response()
    }
}

async fn upload(
    State(a): State<Artifacts>,
    Path(project): Path<String>,
    h: HeaderMap,
    mut mp: Multipart,
) -> Response {
    a.upload_attempts.fetch_add(1, Ordering::SeqCst);
    if !a.authorised(&h, &project) {
        return forbidden(&a, &h);
    }
    let mut up = Upload {
        artifact: Uuid::new_v4(),
        visibility: h
            .get("x-visibility")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("tenant")
            .to_string(),
        ..Upload::default()
    };
    while let Ok(Some(field)) = mp.next_field().await {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            up.bytes = field.bytes().await.unwrap_or_default().to_vec();
            continue;
        }
        let text = field.text().await.unwrap_or_default();
        match name.as_str() {
            "filename" => up.filename = text,
            "content_type" => up.content_type = text,
            "kind" => up.kind = text,
            "sensitivity" => up.sensitivity = text,
            "tags" => up.tags = text,
            _ => {}
        }
    }
    let mode = *a.mode.lock().unwrap();
    match mode {
        Mode::Malware => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                axum::Json(json!({
                    "detail": {"code": "malware", "message": "File rejected: malware detected"},
                    "error": {"code": "malware", "message": "File rejected: malware detected", "status": 422},
                })),
            )
                .into_response();
        }
        Mode::ScanDown => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({
                    "detail": {"code": "scan_unavailable", "message": "Virus scan unavailable"},
                    "error": {"code": "scan_unavailable", "message": "Virus scan unavailable", "status": 503},
                })),
            )
                .into_response();
        }
        Mode::Plain422 => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                axum::Json(json!({"detail": [{"type": "missing", "loc": ["body", "kind"]}]})),
            )
                .into_response();
        }
        Mode::Plain503 => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                axum::Json(json!({"detail": "upstream connect error"})),
            )
                .into_response();
        }
        Mode::Fail500 => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                axum::Json(json!({"detail": "Failed to upload file"})),
            )
                .into_response();
        }
        Mode::Ok => {}
    }
    let id = up.artifact;
    {
        let mut g = a.inner.lock().unwrap();
        g.store
            .insert(id, (up.content_type.clone(), up.bytes.clone()));
        g.uploads.push(up.clone());
    }
    axum::Json(json!({
        "artifact_id": id, "version": 1, "storage_key": format!("k/{id}"), "size": up.bytes.len(),
        "sha256": "x", "origin_kind": "tenant", "publish_status": "draft",
    }))
    .into_response()
}

async fn download(
    State(a): State<Artifacts>,
    Path((project, id)): Path<(String, Uuid)>,
    h: HeaderMap,
) -> Response {
    if !a.authorised(&h, &project) {
        return forbidden(&a, &h);
    }
    let found = a.inner.lock().unwrap().store.get(&id).cloned();
    match found {
        Some((ct, bytes)) => ([(header::CONTENT_TYPE, ct)], bytes).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"detail": "Artifact not found"})),
        )
            .into_response(),
    }
}

async fn list(
    State(a): State<Artifacts>,
    Path(project): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    h: HeaderMap,
) -> Response {
    if !a.authorised(&h, &project) {
        return forbidden(&a, &h);
    }
    a.lists.fetch_add(1, Ordering::SeqCst);
    if a.list_fails.load(Ordering::SeqCst) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let want = q.get("tags").cloned().unwrap_or_default();
    let g = a.inner.lock().unwrap();
    let found: Vec<serde_json::Value> = g
        .uploads
        .iter()
        .filter(|u| g.store.contains_key(&u.artifact))
        .filter(|u| want.is_empty() || u.tags.split(',').any(|t| t.trim() == want))
        .map(|u| {
            let tags: Vec<&str> = u.tags.split(',').map(str::trim).collect();
            json!({"id": u.artifact, "filename": u.filename, "tags": tags})
        })
        .collect();
    axum::Json(found).into_response()
}

async fn remove(
    State(a): State<Artifacts>,
    Path((project, id)): Path<(String, Uuid)>,
    h: HeaderMap,
) -> Response {
    if !a.authorised(&h, &project) {
        return forbidden(&a, &h);
    }
    if a.delete_fails.load(Ordering::SeqCst) {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let mut g = a.inner.lock().unwrap();
    g.deletes.push(id);
    if g.store.remove(&id).is_some() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"detail": "Artifact not found"})),
        )
            .into_response()
    }
}

/// Start the fake.
pub async fn artifacts() -> Artifacts {
    let mut a = Artifacts::default();
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    a.url = format!("http://{}", l.local_addr().unwrap());
    let app = Router::new()
        .route("/projects/{project}/artifacts", get(list))
        .route("/projects/{project}/artifacts/upload", post(upload))
        .route("/projects/{project}/artifacts/{id}/download", get(download))
        .route("/projects/{project}/artifacts/{id}", delete(remove))
        .with_state(a.clone());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    a
}
