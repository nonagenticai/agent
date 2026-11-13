//! In-process strata_rag + strata_ingest (axum on 127.0.0.1:0, one server each), mirroring the routes, bodies and
//! error codes of strata_rag `7c2f1ad` (`src/api/{mod,handlers,eval_api,platform,auth}.rs`, `src/query.rs`) and
//! strata_ingest `ce9590e` (`src/api/{mod,handlers,auth,store_providers}.rs`) — B3 plan §7.
//!
//! - Auth (G1): every route needs a bearer token pinned to one tenant ([`StrataFake::pin`]); unknown/missing → 401
//!   `unauthorized`. rag: `X-Tenant-Id` may repeat the pinned tenant, another one is 403 `tenant_mismatch`, a
//!   non-uuid 400 `invalid_tenant`. ingest: the token alone decides the tenant (the header is ignored).
//! - Every request is recorded ([`Recorded`]), refused ones included.
//! - [`Mock::script`]: a one-shot answer for `(method, path)` after auth, before the route (scripted failures).
//! - Routes not served here answer an empty 404 exactly as an unknown path does. rag + ingest
//!   `POST /api/tenant/providers/test` (T0t: strata_rag #25 `db99e9c`, strata_ingest #13 `be0c669`) is served by
//!   [`provider_test`]; a test scripts an empty 404 to play a strata without it. Ingest
//!   `POST /api/stores/{id}/reingest-copy` + `GET …/{job}`
//!   (strata_ingest #13, T4a) start a `running` copy in [`Mock::reingest_jobs`]; rag `reembed-copy` mirrors the
//!   source subtree under its target at once.
//! - rag query/eval follow strata's target rules: at most one of `root_id` / `root_ids` / `leaf_ids` (else 422), none
//!   named while the tenant has >1 root → 409 `ambiguous_root`, a named store neither the tenant's nor a readable pack
//!   root (or listed in [`Mock::unreadable`]) → 404 `not found: store <id>`.
//! - Platform knowledge (PK T2; strata_rag `src/api/platform.rs`, `src/db/platform.rs`): packs live in [`Mock::packs`]
//!   (`published_at: null` = draft), exclusions in [`Mock::exclusions`], the owner is [`Mock::platform_owner`].
//!   A pack is readable by the owner always, by any other tenant when published and not excluded (segment-wise
//!   prefix, all versions); retired packs stay readable but leave tenant listings. Readable packs' subtrees (owner
//!   stores under the pack root) are readable by queries too; `…/stores` lists the subtree, 404 unless readable.
//!   Register / publish / retire are owner-only (403 `forbidden`). Writes into a published pack's subtree (store
//!   create/patch/delete, store providers, JSON ingest) are 409 `pack_published`. JSON ingest
//!   (`POST /api/stores/{id}/documents`) into an owner store needs provenance with an allow-listed licence (422).
//!   [`Mock::platform_off`] makes every `/api/platform/*` route 404.
//! - strata_ingest #15 (PK T6b): `topic_keys` / `supersedes` / `provenance` on `POST /api/ingest/jobs` — JSON
//!   request-level (a URL item `{url, ...}` overrides field by field), multipart text fields BEFORE the first `file`
//!   part (after it: 400). Keys: ≤32, each 1..=128 of `[a-z0-9_./-]`; provenance `{source_url, license,
//!   retrieved_at (RFC 3339)}`; a bad value is 400 before any job. Into a store of the ingest mock's
//!   [`Mock::platform_owner`] provenance is required (400) and a licence off [`PLATFORM_LICENSES`] creates the job
//!   `failed` with `error.code` `provenance_rejected`. The fields land in the job's `meta`. Multipart bodies are
//!   recorded part by part in [`Mock::forms`].
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Svc {
    Rag,
    Ingest,
}

/// One request as the fake received it.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    /// The raw query string (`""` when none).
    pub query: String,
    /// The bearer token, if any.
    pub token: Option<String>,
    pub tenant_header: Option<String>,
    pub actor: Option<String>,
    pub content_type: Option<String>,
    /// The JSON body (`Null` for an empty or non-JSON body).
    pub body: Value,
    pub body_len: usize,
    /// The tenant the token is pinned to (`None`: refused at auth).
    pub tenant: Option<Uuid>,
}

/// A scripted one-shot answer.
#[derive(Clone, Debug)]
pub struct Scripted {
    pub method: String,
    pub path: String,
    pub status: u16,
    /// `Null` = empty body.
    pub body: Value,
    /// Only for a request of this tenant (`None`: any tenant).
    pub tenant: Option<Uuid>,
}

/// One request held after it is recorded ([`Mock::pause_at`]): `reached` is notified when it arrives, its answer
/// waits for `release`.
#[derive(Clone)]
pub struct Pause {
    pub method: String,
    pub path: String,
    pub reached: Arc<tokio::sync::Notify>,
    pub release: Arc<tokio::sync::Notify>,
}

/// A rag store.
#[derive(Clone, Debug)]
pub struct FakeStore {
    pub tenant: Uuid,
    pub id: Uuid,
    pub parent_id: Option<Uuid>,
    pub kind: String,
    pub name: String,
    pub description: String,
}

/// strata's default embed model identity in the fake.
pub const DEFAULT_EMBED: &str = "BAAI/bge-m3@1024";

#[derive(Default)]
pub struct Mock {
    pub svc: Option<Svc>,
    /// token → tenant.
    pub tokens: HashMap<String, Uuid>,
    pub requests: Vec<Recorded>,
    pub scripted: Vec<Scripted>,
    /// The next `method path` (exact path) waits for [`Pause::release`] before it is answered (once).
    pub pause: Option<Pause>,
    // rag
    pub stores: Vec<FakeStore>,
    /// (tenant, `DocumentListItem` JSON).
    pub documents: Vec<(Uuid, Value)>,
    /// `results` of every query answer (`QueryResult` JSON).
    pub hits: Vec<Value>,
    /// Published packs every tenant may read (`PlatformPack` JSON: key, version, root_id, published_at, retired_at).
    pub packs: Vec<Value>,
    /// `/api/platform/*` answers 404 (`STRATA_PLATFORM_OWNER` unset).
    pub platform_off: bool,
    /// strata's `STRATA_PLATFORM_OWNER` (tests set it): the only tenant that may register, publish and retire packs,
    /// and whose stores are platform stores (JSON ingest needs provenance). `None` = no tenant is the owner.
    pub platform_owner: Option<Uuid>,
    /// (tenant, `PlatformExclusion` JSON: key_prefix, reason, created_at).
    pub exclusions: Vec<(Uuid, Value)>,
    /// Store ids a query/eval may not read (an excluded or deleted pack root): 404.
    pub unreadable: HashSet<Uuid>,
    /// Tenant provider views (rag: embed/rerank/chat; ingest: ocr/vision/transcribe).
    pub tenant_providers: HashMap<Uuid, Value>,
    /// store id → its own registrations (the PUT body as sent).
    pub store_providers: HashMap<Uuid, Value>,
    /// store id → `stores.embed_model` as strata tagged it at creation: a root takes the tenant's embed registration
    /// (or the default `BAAI/bge-m3@1024`), a child its parent's. A store's own embed binding wins.
    pub store_models: HashMap<Uuid, String>,
    /// (tenant, `ReembedCopyJob` JSON).
    pub reembed_jobs: Vec<(Uuid, Value)>,
    /// (tenant, `Question` JSON).
    pub questions: Vec<(Uuid, Value)>,
    /// (tenant, `EvalRun` JSON).
    pub runs: Vec<(Uuid, Value)>,
    // ingest
    /// Leaves strata_ingest accepts as targets; empty = any store.
    pub leaves: Vec<Uuid>,
    /// (tenant, `JobView` JSON).
    pub jobs: Vec<(Uuid, Value)>,
    /// (tenant, `ReingestCopyJob` JSON; strata_ingest #13). Tests move `state` / `missing_originals` themselves.
    pub reingest_jobs: Vec<(Uuid, Value)>,
    /// (tenant, every part of a multipart `POST /api/ingest/jobs` in the order received; PK T6b), refused ones
    /// included: a test asserts on the #15 text fields and on their place before the first file part.
    pub forms: Vec<(Uuid, Vec<FormPart>)>,
}

/// One part of a recorded multipart body: a text field (`file_name` `None`, `value` its text) or a file part
/// (`file_name` set, `value` its bytes read lossily).
#[derive(Clone, Debug)]
pub struct FormPart {
    pub name: String,
    pub file_name: Option<String>,
    pub value: String,
}

pub type Shared = Arc<Mutex<Mock>>;

pub fn lock(m: &Shared) -> MutexGuard<'_, Mock> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Mock {
    /// Answer the next `method path` (exact path, no query) with `status` and `body` (`Null` = empty), once.
    pub fn script(&mut self, method: &str, path: &str, status: u16, body: Value) {
        self.scripted.push(Scripted {
            method: method.to_string(),
            path: path.to_string(),
            status,
            body,
            tenant: None,
        });
    }

    /// [`Mock::script`] for a request of `tenant` only (another tenant's same call passes through).
    pub fn script_for(&mut self, tenant: Uuid, method: &str, path: &str, status: u16, body: Value) {
        self.scripted.push(Scripted {
            method: method.to_string(),
            path: path.to_string(),
            status,
            body,
            tenant: Some(tenant),
        });
    }

    /// Holds the next `method path` (exact path, no query) until the returned [`Pause::release`] is notified.
    pub fn pause_at(&mut self, method: &str, path: &str) -> Pause {
        let p = Pause {
            method: method.to_string(),
            path: path.to_string(),
            reached: Arc::new(tokio::sync::Notify::new()),
            release: Arc::new(tokio::sync::Notify::new()),
        };
        self.pause = Some(p.clone());
        p
    }

    /// A rag store of `tenant`; returns its id.
    pub fn add_store(
        &mut self,
        tenant: Uuid,
        parent: Option<Uuid>,
        kind: &str,
        name: &str,
    ) -> Uuid {
        let id = Uuid::new_v4();
        let tag = match parent {
            Some(p) => self.embed_model_of(p),
            None => self.tenant_embed_id(tenant),
        };
        self.store_models.insert(id, tag);
        self.stores.push(FakeStore {
            tenant,
            id,
            parent_id: parent,
            kind: kind.to_string(),
            name: name.to_string(),
            description: name.to_string(),
        });
        id
    }

    /// A published pack readable by every tenant; returns its root id.
    pub fn add_pack(&mut self, key: &str, version: i32) -> Uuid {
        let root = Uuid::new_v4();
        self.packs.push(json!({
            "key": key, "version": version, "root_id": root,
            "published_at": now(), "retired_at": null
        }));
        root
    }

    /// A pack of [`Mock::platform_owner`] (which must be set) with a real subtree: a router root named
    /// `<key>@<version>` and one leaf per `leaves` name under it; published when `published`. Returns (root, leaves).
    pub fn add_owned_pack(
        &mut self,
        key: &str,
        version: i32,
        leaves: &[&str],
        published: bool,
    ) -> (Uuid, Vec<Uuid>) {
        let owner = self.platform_owner.expect("set Mock::platform_owner first");
        let root = self.add_store(owner, None, "router", &format!("{key}@{version}"));
        let ids = leaves
            .iter()
            .map(|n| self.add_store(owner, Some(root), "leaf", n))
            .collect();
        let published_at = if published { json!(now()) } else { Value::Null };
        self.packs.push(json!({
            "key": key, "version": version, "root_id": root,
            "published_at": published_at, "retired_at": null
        }));
        (root, ids)
    }

    /// The embed model a new root of `tenant` is made with: its tenant embed registration, else the default.
    pub fn tenant_embed_id(&self, tenant: Uuid) -> String {
        let embed = self
            .tenant_providers
            .get(&tenant)
            .map(|v| v["embed"].clone())
            .unwrap_or(Value::Null);
        if embed["source"] != "tenant" {
            return DEFAULT_EMBED.to_string();
        }
        if let Some(id) = embed["model_id"].as_str() {
            return id.to_string();
        }
        format!(
            "{}@{}",
            embed["model"].as_str().unwrap_or(""),
            embed["dims"].as_i64().unwrap_or(1024)
        )
    }

    /// `stores.embed_model` of `id`: its own embed binding, else its tag (or its nearest ancestor's), else the
    /// default.
    pub fn embed_model_of(&self, id: Uuid) -> String {
        if let Some(m) = self
            .store_providers
            .get(&id)
            .and_then(|own| own["embed"]["model"].as_str())
        {
            return format!("{m}@1024");
        }
        let mut cur = Some(id);
        while let Some(i) = cur {
            if let Some(tag) = self.store_models.get(&i) {
                return tag.clone();
            }
            cur = self
                .stores
                .iter()
                .find(|s| s.id == i)
                .and_then(|s| s.parent_id);
        }
        DEFAULT_EMBED.to_string()
    }

    fn roots_of(&self, tenant: Uuid) -> usize {
        self.stores
            .iter()
            .filter(|s| s.tenant == tenant && s.parent_id.is_none())
            .count()
    }

    fn store(&self, tenant: Uuid, id: Uuid) -> Option<&FakeStore> {
        self.stores
            .iter()
            .find(|s| s.tenant == tenant && s.id == id)
    }

    fn view(&self, s: &FakeStore) -> Value {
        let children: Vec<Uuid> = self
            .stores
            .iter()
            .filter(|c| c.parent_id == Some(s.id))
            .map(|c| c.id)
            .collect();
        json!({
            "id": s.id, "parent_id": s.parent_id, "kind": s.kind, "name": s.name,
            "description": s.description, "token_count": 0, "chunk_count": 0, "children": children
        })
    }

    /// Own stores, and the stores of a pack readable by `tenant` ([`Mock::pack_readable`]): its root and, when the
    /// root is a fake store, its subtree.
    fn readable(&self, tenant: Uuid, id: Uuid) -> bool {
        if self.unreadable.contains(&id) {
            return false;
        }
        if self.store(tenant, id).is_some() {
            return true;
        }
        let root = json!(self.root_of(id));
        self.packs
            .iter()
            .any(|p| p["root_id"] == root && self.pack_readable(tenant, p))
    }

    /// The top ancestor of `id` among all tenants' stores (`id` itself when it is not a fake store).
    fn root_of(&self, id: Uuid) -> Uuid {
        let mut cur = id;
        while let Some(parent) = self
            .stores
            .iter()
            .find(|s| s.id == cur)
            .and_then(|s| s.parent_id)
        {
            cur = parent;
        }
        cur
    }

    /// strata `readable_packs`: the owner reads every pack; another tenant a published one it is not excluded from.
    /// Retired packs stay readable.
    fn pack_readable(&self, tenant: Uuid, p: &Value) -> bool {
        if self.platform_owner == Some(tenant) {
            return true;
        }
        !p["published_at"].is_null() && !self.excluded(tenant, p["key"].as_str().unwrap_or(""))
    }

    /// `key` is covered by one of `tenant`'s exclusions.
    fn excluded(&self, tenant: Uuid, key: &str) -> bool {
        self.exclusions.iter().any(|(t, x)| {
            *t == tenant && key_matches_prefix(key, x["key_prefix"].as_str().unwrap_or(""))
        })
    }

    fn pack_idx(&self, key: &str, version: i64) -> Option<usize> {
        self.packs
            .iter()
            .position(|p| p["key"] == json!(key) && p["version"].as_i64() == Some(version))
    }

    /// 409 `pack_published` when `id` lies in a published pack's subtree (strata `assert_writable`).
    fn frozen_pack(&self, id: Uuid) -> Option<Response> {
        let root = json!(self.root_of(id));
        self.packs
            .iter()
            .find(|p| p["root_id"] == root && !p["published_at"].is_null())
            .map(|p| {
                err(
                    409,
                    "pack_published",
                    &format!(
                        "pack {}@{} is published",
                        p["key"].as_str().unwrap_or(""),
                        p["version"]
                    ),
                )
            })
    }

    /// strata `pack_stores`: the subtree of `root`, root first, ordered by name path.
    fn pack_subtree(&self, root: Uuid) -> Vec<Value> {
        let Some(r) = self.stores.iter().find(|s| s.id == root) else {
            return Vec::new();
        };
        let mut out: std::collections::BTreeMap<Vec<String>, Value> =
            std::collections::BTreeMap::new();
        let mut queue = vec![(r.clone(), vec![r.name.clone()])];
        while let Some((s, path)) = queue.pop() {
            for c in self.stores.iter().filter(|c| c.parent_id == Some(s.id)) {
                let mut cp = path.clone();
                cp.push(c.name.clone());
                queue.push((c.clone(), cp));
            }
            let chunks: i64 = self
                .documents
                .iter()
                .filter(|(_, d)| d["store_id"] == json!(s.id))
                .map(|(_, d)| d["chunk_count"].as_i64().unwrap_or(0))
                .sum();
            let view = json!({
                "id": s.id, "parent_id": s.parent_id, "kind": s.kind, "name": s.name, "path": path,
                "embed_model": self.embed_model_of(s.id), "chunk_count": chunks
            });
            out.insert(path, view);
        }
        out.into_values().collect()
    }
}

/// Both fakes.
pub struct StrataFake {
    pub rag_url: String,
    pub ingest_url: String,
    pub rag: Shared,
    pub ingest: Shared,
}

pub fn rag_token(t: Uuid) -> String {
    format!("rag-{t}")
}

pub fn ingest_token(t: Uuid) -> String {
    format!("ing-{t}")
}

impl StrataFake {
    /// Pins a token per tenant in both services ([`rag_token`], [`ingest_token`]).
    pub fn pin(&self, tenants: &[Uuid]) {
        for t in tenants {
            lock(&self.rag).tokens.insert(rag_token(*t), *t);
            lock(&self.ingest).tokens.insert(ingest_token(*t), *t);
        }
    }

    /// The agent environment for these fakes: both URLs and the token pairs of `tenants` (also pinned here).
    pub fn env(&self, tenants: &[Uuid], vars: &mut HashMap<String, String>) {
        self.pin(tenants);
        let pairs = |f: fn(Uuid) -> String| {
            tenants
                .iter()
                .map(|t| format!("{t}:{}", f(*t)))
                .collect::<Vec<_>>()
                .join(",")
        };
        vars.insert("STRATA_RAG_URL".into(), self.rag_url.clone());
        vars.insert("STRATA_RAG_TOKENS".into(), pairs(rag_token));
        vars.insert("STRATA_INGEST_URL".into(), self.ingest_url.clone());
        vars.insert("STRATA_INGEST_TOKENS".into(), pairs(ingest_token));
    }

    pub fn rag_requests(&self) -> Vec<Recorded> {
        lock(&self.rag).requests.clone()
    }

    pub fn ingest_requests(&self) -> Vec<Recorded> {
        lock(&self.ingest).requests.clone()
    }
}

/// Starts both fakes.
pub async fn start() -> StrataFake {
    let rag: Shared = Arc::new(Mutex::new(Mock {
        svc: Some(Svc::Rag),
        ..Mock::default()
    }));
    let ingest: Shared = Arc::new(Mutex::new(Mock {
        svc: Some(Svc::Ingest),
        ..Mock::default()
    }));
    let rag_url = serve(rag.clone()).await;
    let ingest_url = serve(ingest.clone()).await;
    StrataFake {
        rag_url,
        ingest_url,
        rag,
        ingest,
    }
}

async fn serve(m: Shared) -> String {
    let app = Router::new().fallback(handle).with_state(m);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

fn now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6fZ")
        .to_string()
}

fn err(status: u16, code: &str, message: &str) -> Response {
    let st = StatusCode::from_u16(status).unwrap();
    (
        st,
        axum::Json(json!({"error": {"code": code, "message": message}})),
    )
        .into_response()
}

fn ok(status: u16, body: Value) -> Response {
    (StatusCode::from_u16(status).unwrap(), axum::Json(body)).into_response()
}

fn empty(status: u16) -> Response {
    StatusCode::from_u16(status).unwrap().into_response()
}

fn header_str(h: &HeaderMap, name: &str) -> Option<String> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

fn bearer(h: &HeaderMap) -> Option<String> {
    let v = header_str(h, "authorization")?;
    let (scheme, token) = v.trim().split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then(|| token.to_string())
}

fn query_map(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k.to_string(), decode(v))
        })
        .collect()
}

/// Percent-decoding of a query value.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(Ok(v)) = s.get(i + 1..i + 3).map(|h| u8::from_str_radix(h, 16))
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn handle(
    State(m): State<Shared>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let svc = lock(&m).svc.unwrap_or(Svc::Rag);
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();
    let token = bearer(&headers);
    let content_type = header_str(&headers, "content-type");
    let json_body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let tenant = token.as_ref().and_then(|t| lock(&m).tokens.get(t).copied());
    lock(&m).requests.push(Recorded {
        method: method.to_string(),
        path: path.clone(),
        query: query.clone(),
        token: token.clone(),
        tenant_header: header_str(&headers, "x-tenant-id"),
        actor: header_str(&headers, "x-actor-id"),
        content_type: content_type.clone(),
        body: json_body.clone(),
        body_len: body.len(),
        tenant,
    });
    let paused = {
        let mut g = lock(&m);
        let hit = g
            .pause
            .as_ref()
            .is_some_and(|p| p.method == method.as_str() && p.path == path);
        if hit { g.pause.take() } else { None }
    };
    if let Some(p) = paused {
        p.reached.notify_one();
        p.release.notified().await;
    }
    if path == "/health" {
        return ok(200, json!({"status": "ok"}));
    }
    let Some(tenant) = tenant else {
        let mut r = err(401, "unauthorized", "missing or invalid bearer token");
        r.headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return r;
    };
    if svc == Svc::Rag
        && let Some(h) = header_str(&headers, "x-tenant-id")
    {
        match Uuid::parse_str(h.trim()) {
            Ok(t) if !t.is_nil() && t == tenant => {}
            Ok(t) if !t.is_nil() => {
                return err(
                    403,
                    "tenant_mismatch",
                    "tenant mismatch: X-Tenant-Id names a tenant this token is not for",
                );
            }
            _ => {
                return err(
                    400,
                    "invalid_tenant",
                    "invalid tenant: X-Tenant-Id must be a non-nil tenant uuid",
                );
            }
        }
    }
    {
        let mut g = lock(&m);
        if let Some(i) = g.scripted.iter().position(|s| {
            s.method == method.as_str() && s.path == path && s.tenant.is_none_or(|t| t == tenant)
        }) {
            let s = g.scripted.remove(i);
            return if s.body.is_null() {
                empty(s.status)
            } else {
                ok(s.status, s.body)
            };
        }
    }
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    let q = query_map(&query);
    let mut g = lock(&m);
    match svc {
        Svc::Rag => rag(&mut g, tenant, &method, &segs, &q, json_body),
        Svc::Ingest => ingest(
            &mut g,
            tenant,
            &method,
            &segs,
            &q,
            content_type.as_deref(),
            &body,
            json_body,
        ),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// strata_rag

fn not_found_store(id: Uuid) -> Response {
    err(404, "not_found", &format!("not found: store {id}"))
}

/// strata's PUT rule for a write-only key: a capability sent without `api_key` keeps the key stored for the same
/// `base_url` (`has_api_key` stays true); another `base_url` has none.
fn keep_keys(prev: Option<&Value>, view: &mut Value, input: &Value) {
    let Some(prev) = prev else {
        return;
    };
    let Some(caps) = view.as_object_mut() else {
        return;
    };
    for (cap, v) in caps.iter_mut() {
        let sent_key = input.get(cap).and_then(|i| i.get("api_key")).is_some();
        let old = &prev[cap.as_str()];
        if !sent_key
            && v["source"] == "tenant"
            && old["has_api_key"] == true
            && old["base_url"] == v["base_url"]
        {
            v["has_api_key"] = json!(true);
        }
    }
}

/// strata's `deny_unknown_fields` on provider PUTs (`PutProviders`, `PutStoreProviders`, `ProviderInput`; rag
/// `src/providers/registry.rs`, ingest the same without `dims` / `thinking` / `temperature`): another capability or
/// another field is 422 `invalid_input` (B3 review M3: drift in what agent sends is caught here, not in production).
/// The capabilities of strata_rag's and strata_ingest's provider sets.
const RAG_CAPS: [&str; 3] = ["embed", "rerank", "chat"];
const INGEST_CAPS: [&str; 3] = ["ocr", "vision", "transcribe"];

fn unknown_provider_field(b: &Value, caps: &[&str], rag: bool) -> Option<Response> {
    const FIELDS: [&str; 9] = [
        "base_url",
        "dialect",
        "model",
        "model_id",
        "regions",
        "zdr",
        "timeout_ms",
        "concurrency",
        "api_key",
    ];
    const RAG_FIELDS: [&str; 3] = ["dims", "thinking", "temperature"];
    let Some(m) = b.as_object() else {
        return Some(err(422, "invalid_input", "the body is not an object"));
    };
    for (cap, v) in m {
        if !caps.contains(&cap.as_str()) {
            return Some(err(422, "invalid_input", &format!("unknown field `{cap}`")));
        }
        if v.is_null() {
            continue;
        }
        let Some(f) = v.as_object() else {
            return Some(err(422, "invalid_input", &format!("{cap}: not an object")));
        };
        if let Some(k) = f
            .keys()
            .find(|k| !FIELDS.contains(&k.as_str()) && !(rag && RAG_FIELDS.contains(&k.as_str())))
        {
            return Some(err(
                422,
                "invalid_input",
                &format!("{cap}: unknown field `{k}`"),
            ));
        }
    }
    None
}

fn rag_capability_view(input: Option<&Value>, own: &str) -> Value {
    match input {
        Some(v) if v.is_object() => {
            let mut view = Map::new();
            view.insert("source".into(), json!(own));
            for k in [
                "base_url", "dialect", "model", "model_id", "dims", "regions", "zdr",
            ] {
                view.insert(k.into(), v.get(k).cloned().unwrap_or(Value::Null));
            }
            view.insert(
                "timeout_ms".into(),
                v.get("timeout_ms").cloned().unwrap_or(json!(30000)),
            );
            view.insert(
                "concurrency".into(),
                v.get("concurrency").cloned().unwrap_or(json!(8)),
            );
            view.insert("has_api_key".into(), json!(v.get("api_key").is_some()));
            view.insert("updated_at".into(), json!(now()));
            Value::Object(view)
        }
        // a tenant capability with no registration is served by the deployment default (strata `source: default`;
        // B3 T12 readiness reads a reported `unconfigured` as "nothing deployed"); a store binding has no default
        _ if own == "tenant" => json!({"source": "default"}),
        _ => json!({"source": "unconfigured"}),
    }
}

/// Exactly one of root_id / root_ids / leaf_ids (or none), checked against what `tenant` may read.
// The refusal is the fake's HTTP response itself; boxing it buys nothing in a test fake.
#[allow(clippy::result_large_err)]
fn check_targets(g: &Mock, tenant: Uuid, b: &Value) -> Result<Vec<Uuid>, Response> {
    let named: Vec<&str> = ["root_id", "root_ids", "leaf_ids"]
        .into_iter()
        .filter(|k| b.get(*k).is_some_and(|v| !v.is_null()))
        .collect();
    if named.len() > 1 {
        return Err(err(
            422,
            "invalid_input",
            "invalid input: name at most one of root_id, root_ids and leaf_ids",
        ));
    }
    let ids: Vec<Uuid> = match named.first() {
        None => {
            let roots = g.roots_of(tenant);
            if roots > 1 {
                return Err(err(
                    409,
                    "ambiguous_root",
                    &format!(
                        "ambiguous root: this tenant has {roots} root stores; name what to search with root_id, root_ids or leaf_ids"
                    ),
                ));
            }
            return Ok(Vec::new());
        }
        Some(&"root_id") => vec![
            b["root_id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .unwrap_or_default(),
        ],
        Some(k) => {
            let list: Vec<Uuid> = b[*k]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().and_then(|s| Uuid::parse_str(s).ok()))
                        .collect()
                })
                .unwrap_or_default();
            if list.is_empty() {
                return Err(err(
                    422,
                    "invalid_input",
                    &format!("invalid input: {k} must name at least one store"),
                ));
            }
            list
        }
    };
    if let Some(bad) = ids.iter().find(|id| !g.readable(tenant, **id)) {
        return Err(not_found_store(*bad));
    }
    Ok(ids)
}

fn rag(
    g: &mut Mock,
    tenant: Uuid,
    method: &Method,
    segs: &[&str],
    q: &HashMap<String, String>,
    b: Value,
) -> Response {
    match (method.as_str(), segs) {
        ("GET", ["api", "stores"]) => {
            let stores: Vec<Value> = g
                .stores
                .iter()
                .filter(|s| s.tenant == tenant)
                .map(|s| g.view(s))
                .collect();
            ok(200, json!({ "stores": stores }))
        }
        ("POST", ["api", "stores"]) => {
            let kind = b["kind"].as_str().unwrap_or("");
            let name = b["name"].as_str().unwrap_or("");
            if !matches!(kind, "router" | "leaf")
                || name.is_empty()
                || !b["description"].is_string()
            {
                return err(
                    422,
                    "invalid_input",
                    "invalid input: kind, name and description are required",
                );
            }
            let parent = b["parent_id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok());
            if let Some(p) = parent {
                match g.store(tenant, p) {
                    None => return not_found_store(p),
                    Some(s) if s.kind != "router" => {
                        return err(
                            422,
                            "invalid_target",
                            &format!("invalid target: store {p} is a leaf"),
                        );
                    }
                    Some(_) => {}
                }
                if let Some(r) = g.frozen_pack(p) {
                    return r;
                }
            }
            if g.stores
                .iter()
                .any(|s| s.tenant == tenant && s.parent_id == parent && s.name == name)
            {
                return err(409, "duplicate_name", &format!("duplicate name: {name}"));
            }
            let id = g.add_store(tenant, parent, kind, name);
            if let Some(s) = g.stores.iter_mut().find(|s| s.id == id) {
                s.description = b["description"].as_str().unwrap_or("").to_string();
            }
            let s = g.store(tenant, id).cloned().unwrap();
            ok(201, g.view(&s))
        }
        ("GET" | "PATCH" | "DELETE", ["api", "stores", id]) => {
            let Ok(id) = Uuid::parse_str(id) else {
                return err(422, "invalid_input", "invalid input: invalid id in path");
            };
            let Some(s) = g.store(tenant, id).cloned() else {
                return not_found_store(id);
            };
            if *method != Method::GET
                && let Some(r) = g.frozen_pack(id)
            {
                return r;
            }
            match method.as_str() {
                "GET" => ok(200, g.view(&s)),
                "PATCH" => {
                    if b.get("name").is_none() && b.get("description").is_none() {
                        return err(
                            422,
                            "invalid_input",
                            "invalid input: nothing to update: give name and/or description",
                        );
                    }
                    if let Some(st) = g.stores.iter_mut().find(|x| x.id == id) {
                        if let Some(n) = b["name"].as_str() {
                            st.name = n.to_string();
                        }
                        if let Some(d) = b["description"].as_str() {
                            st.description = d.to_string();
                        }
                    }
                    let s = g.store(tenant, id).cloned().unwrap();
                    ok(200, g.view(&s))
                }
                _ => {
                    let recursive = q.get("recursive").is_some_and(|v| v == "true");
                    let has_children = g.stores.iter().any(|c| c.parent_id == Some(id));
                    if has_children && !recursive {
                        return err(409, "has_children", &format!("store {id} has children"));
                    }
                    let mut gone = vec![id];
                    let mut i = 0;
                    while i < gone.len() {
                        let cur = gone[i];
                        gone.extend(
                            g.stores
                                .iter()
                                .filter(|c| c.parent_id == Some(cur))
                                .map(|c| c.id),
                        );
                        i += 1;
                    }
                    g.stores.retain(|x| !gone.contains(&x.id));
                    empty(204)
                }
            }
        }
        (m @ ("GET" | "PUT" | "DELETE"), ["api", "stores", id, "providers"]) => {
            let Some(id) = Uuid::parse_str(id)
                .ok()
                .filter(|i| g.store(tenant, *i).is_some())
            else {
                return err(404, "not_found", &format!("not found: store {id}"));
            };
            if m != "GET"
                && let Some(r) = g.frozen_pack(id)
            {
                return r;
            }
            match m {
                "PUT" => {
                    if let Some(r) = unknown_provider_field(&b, &RAG_CAPS, true) {
                        return r;
                    }
                    g.store_providers.insert(id, b);
                }
                "DELETE" => {
                    g.store_providers.remove(&id);
                }
                _ => {}
            }
            let own = g.store_providers.get(&id).cloned().unwrap_or(Value::Null);
            let embed_model = g.embed_model_of(id);
            // a capability the store does not bind is the tenant's (strata `store_view`; roots only here)
            let cap = |name: &str| match own.get(name) {
                Some(v) if v.is_object() => rag_capability_view(Some(v), "store"),
                _ => g
                    .tenant_providers
                    .get(&tenant)
                    .and_then(|v| v.get(name))
                    .cloned()
                    .unwrap_or_else(|| json!({"source": "unconfigured"})),
            };
            ok(
                200,
                json!({
                    "store_id": id, "embed_model": embed_model,
                    "embed": cap("embed"), "rerank": cap("rerank"), "chat": cap("chat"),
                }),
            )
        }
        ("POST", ["api", "stores", id, "reembed-copy"]) => {
            let Some(src) = Uuid::parse_str(id)
                .ok()
                .filter(|i| g.store(tenant, *i).is_some())
            else {
                return err(404, "not_found", &format!("not found: store {id}"));
            };
            let Some(target) = b["target_store"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                return err(
                    422,
                    "invalid_input",
                    "invalid input: target_store is required",
                );
            };
            if g.store(tenant, target).is_none() {
                return not_found_store(target);
            }
            // the copy mirrors the source subtree under the target (strata writes it when the copy commits; here
            // at once, so a test sees the tree; T4a)
            let mut queue = vec![(src, target)];
            while let Some((from, to)) = queue.pop() {
                let kids: Vec<FakeStore> = g
                    .stores
                    .iter()
                    .filter(|s| s.parent_id == Some(from))
                    .cloned()
                    .collect();
                for k in kids {
                    let id = Uuid::new_v4();
                    g.stores.push(FakeStore {
                        tenant,
                        id,
                        parent_id: Some(to),
                        kind: k.kind.clone(),
                        name: k.name.clone(),
                        description: k.description.clone(),
                    });
                    queue.push((k.id, id));
                }
            }
            let job = json!({
                "job": Uuid::new_v4(), "source_store": src, "target_store": target,
                "embed_model": "BAAI/bge-m3@1024",
                "copy_vectors": b["copy_vectors"].as_bool().unwrap_or(false),
                "state": "running", "chunks_done": 0, "chunks_total": 0, "started_at": now()
            });
            g.reembed_jobs.push((tenant, job.clone()));
            ok(202, job)
        }
        ("GET", ["api", "stores", id, "reembed-copy", job]) => g
            .reembed_jobs
            .iter()
            .find(|(t, j)| *t == tenant && j["source_store"] == json!(id) && j["job"] == json!(job))
            .map(|(_, j)| ok(200, j.clone()))
            .unwrap_or_else(|| {
                err(
                    404,
                    "not_found",
                    &format!("not found: reembed-copy job {job}"),
                )
            }),
        ("GET", ["api", "stores", id, "documents"]) => {
            let Some(store) = Uuid::parse_str(id)
                .ok()
                .and_then(|i| g.store(tenant, i).cloned())
            else {
                return err(404, "not_found", &format!("not found: store {id}"));
            };
            if store.kind != "leaf" {
                return ok(200, json!({"items": [], "next": null}));
            }
            let limit = q
                .get("limit")
                .and_then(|l| l.parse::<usize>().ok())
                .unwrap_or(50)
                .min(500);
            let items: Vec<Value> = g
                .documents
                .iter()
                .filter(|(t, d)| *t == tenant && d["store_id"] == json!(store.id))
                .filter(|(_, d)| {
                    q.get("source_id")
                        .is_none_or(|s| d["source_id"] == json!(s))
                })
                .map(|(_, d)| d.clone())
                .take(limit)
                .collect();
            ok(200, json!({"items": items, "next": null}))
        }
        ("POST", ["api", "stores", id, "documents"]) => ingest_json(g, tenant, id, &b),
        ("DELETE", ["api", "documents", id]) => {
            let before = g.documents.len();
            g.documents
                .retain(|(t, d)| !(*t == tenant && d["id"].as_str() == Some(*id)));
            if g.documents.len() == before {
                return err(404, "not_found", &format!("not found: document {id}"));
            }
            empty(204)
        }
        ("DELETE", ["api", "sources", source_id, "documents"]) => {
            let all = q.get("all").is_some_and(|v| v == "true");
            let except = q.get("except_run").cloned();
            if !all && except.is_none() {
                return err(
                    422,
                    "invalid_input",
                    "invalid input: give except_run or all=true",
                );
            }
            let allow_empty = q.get("allow_empty").is_some_and(|v| v == "true");
            let only = q.get("store_id").cloned();
            // B3 T8: per leaf, as strata `Engine::prune_source`: one store (`store_id`) or every leaf holding the
            // source; with `except_run`, a leaf where no document of the source carries that run is left untouched
            // (`empty_sync_run`, unless `allow_empty`)
            let of_source = |t: &Uuid, d: &Value| {
                *t == tenant
                    && d["source_id"].as_str() == Some(*source_id)
                    && only
                        .as_ref()
                        .is_none_or(|s| d["store_id"].as_str() == Some(s.as_str()))
            };
            let mut leaves: Vec<String> = g
                .documents
                .iter()
                .filter(|(t, d)| of_source(t, d))
                .filter_map(|(_, d)| d["store_id"].as_str().map(str::to_string))
                .collect();
            leaves.sort();
            leaves.dedup();
            let mut refused: Vec<String> = Vec::new();
            let mut stores: Vec<String> = Vec::new();
            let mut deleted = 0usize;
            for leaf in leaves {
                if let Some(run) = except.as_deref()
                    && !allow_empty
                    && !g.documents.iter().any(|(t, d)| {
                        of_source(t, d)
                            && d["store_id"].as_str() == Some(leaf.as_str())
                            && d["source_run"].as_str() == Some(run)
                    })
                {
                    refused.push(leaf);
                    continue;
                }
                let before = g.documents.len();
                g.documents.retain(|(t, d)| {
                    !(of_source(t, d)
                        && d["store_id"].as_str() == Some(leaf.as_str())
                        && (all || d["source_run"].as_str() != except.as_deref()))
                });
                let n = before - g.documents.len();
                if n > 0 {
                    stores.push(leaf);
                }
                deleted += n;
            }
            let kept = g.documents.iter().filter(|(t, d)| of_source(t, d)).count();
            let report = json!({"deleted": deleted, "kept": kept, "stores": stores, "refused_stores": refused});
            if refused.is_empty() {
                return ok(200, report);
            }
            let mut body = report;
            body["error"] = json!({"code": "empty_sync_run", "message": "empty sync run: no document carries the run"});
            ok(409, body)
        }
        ("POST", ["api", "query"]) => {
            if b["query"].as_str().is_none_or(|s| s.trim().is_empty()) {
                return err(
                    422,
                    "invalid_input",
                    "invalid input: query must not be empty",
                );
            }
            let ids = match check_targets(g, tenant, &b) {
                Ok(ids) => ids,
                Err(r) => return r,
            };
            let routed = b.get("leaf_ids").is_none_or(Value::is_null);
            let mode = if routed {
                b.get("route_mode")
                    .cloned()
                    .filter(|v| !v.is_null())
                    .unwrap_or(json!("hybrid"))
            } else {
                Value::Null
            };
            ok(
                200,
                json!({
                    "results": g.hits.clone(),
                    "routing": {"mode_used": mode, "leaves": ids, "laya_consulted": false},
                    "timings_ms": {"embed": 3, "route": 1, "search": 2, "rerank": 4, "total": 11},
                    "providers": {"embed": {"provider": "fake", "model": "BAAI/bge-m3"}},
                    "timings_by_model": [{"embed_model": "BAAI/bge-m3@1024", "leaves": ids.len(), "embed": 3, "search": 2, "path": "exact"}],
                }),
            )
        }
        ("POST", ["api", "eval"]) => {
            let tenant_source = b["source"] == json!("tenant")
                || b.get("question_ids").is_some()
                || b.get("questions").is_some();
            if !tenant_source {
                return err(
                    409,
                    "eval_not_ready",
                    "eval not ready: the server's eval questions file is missing or invalid (see the server log)",
                );
            }
            if g.runs
                .iter()
                .any(|(t, r)| *t == tenant && r["status"] == json!("running"))
            {
                return err(
                    409,
                    "eval_running",
                    "eval running: an eval of this tenant is already running",
                );
            }
            let root = match b.get("root_id").filter(|v| !v.is_null()) {
                Some(v) => {
                    let Some(id) = v.as_str().and_then(|s| Uuid::parse_str(s).ok()) else {
                        return err(422, "invalid_input", "invalid input: root_id");
                    };
                    if !g.readable(tenant, id) {
                        return not_found_store(id);
                    }
                    Some(id)
                }
                None => {
                    let roots = g.roots_of(tenant);
                    if roots > 1 {
                        return err(
                            409,
                            "ambiguous_root",
                            &format!(
                                "ambiguous root: this tenant has {roots} root stores; name what to search with root_id, root_ids or leaf_ids"
                            ),
                        );
                    }
                    None
                }
            };
            let count = b["questions"]
                .as_array()
                .map(Vec::len)
                .unwrap_or_else(|| g.questions.iter().filter(|(t, _)| *t == tenant).count());
            if count == 0 {
                return err(
                    409,
                    "eval_not_ready",
                    "eval not ready: this tenant has no eval questions; add some with POST /api/eval/questions",
                );
            }
            let id = Uuid::new_v4();
            let source = if b.get("questions").is_some() {
                "inline"
            } else {
                "tenant"
            };
            let run = json!({
                "id": id, "source": source,
                "root_id": root, "modes": b["modes"].as_array().cloned().unwrap_or_else(|| vec![json!("embed"), json!("hybrid")]),
                "beam": b["beam"].as_u64().unwrap_or(3), "min_score": b["min_score"].as_f64().unwrap_or(0.35),
                "status": "running", "error": null, "progress": {"done": 0, "total": count},
                "verdict": null, "started_by": null, "started_at": now(), "finished_at": null,
                "took_ms": null, "question_count": count, "reports": null,
            });
            g.runs.push((tenant, run.clone()));
            let mut r = ok(202, run);
            r.headers_mut().insert(
                header::LOCATION,
                HeaderValue::from_str(&format!("/api/eval/runs/{id}")).unwrap(),
            );
            r
        }
        ("GET", ["api", "eval", "questions"]) => {
            let items: Vec<Value> = g
                .questions
                .iter()
                .filter(|(t, _)| *t == tenant)
                .map(|(_, q)| q.clone())
                .collect();
            ok(200, json!({"items": items, "next": null}))
        }
        ("POST", ["api", "eval", "questions"]) => {
            let Some(query) = b["query"].as_str().map(str::trim).filter(|s| !s.is_empty()) else {
                return err(
                    422,
                    "invalid_input",
                    "invalid input: query must not be empty",
                );
            };
            if g.questions.iter().any(|(t, x)| {
                *t == tenant
                    && x["query"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case(query))
            }) {
                return err(
                    409,
                    "question_exists",
                    "question exists: the same query is already a question",
                );
            }
            let row = question(&b, query);
            g.questions.push((tenant, row.clone()));
            ok(201, row)
        }
        ("POST", ["api", "eval", "questions", "import"]) => {
            let Some(list) = b["questions"].as_array().cloned() else {
                return err(422, "invalid_input", "invalid input: questions is empty");
            };
            let mut added = 0;
            for x in &list {
                if let Some(query) = x["query"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    g.questions.push((tenant, question(x, query)));
                    added += 1;
                }
            }
            let total = g.questions.iter().filter(|(t, _)| *t == tenant).count();
            ok(201, json!({"added": added, "total": total}))
        }
        (m @ ("PATCH" | "DELETE"), ["api", "eval", "questions", id]) => {
            let Some(i) = g
                .questions
                .iter()
                .position(|(t, x)| *t == tenant && x["id"].as_str() == Some(*id))
            else {
                return err(404, "not_found", &format!("not found: question {id}"));
            };
            if m == "DELETE" {
                g.questions.remove(i);
                return empty(204);
            }
            if let (Some(obj), Some(patch)) = (g.questions[i].1.as_object_mut(), b.as_object()) {
                for (k, v) in patch {
                    obj.insert(k.clone(), v.clone());
                }
                obj.insert("updated_at".into(), json!(now()));
            }
            ok(200, g.questions[i].1.clone())
        }
        ("GET", ["api", "eval", "runs"]) => {
            let items: Vec<Value> = g
                .runs
                .iter()
                .filter(|(t, _)| *t == tenant)
                .map(|(_, r)| r.clone())
                .collect();
            ok(200, json!({"items": items, "next": null}))
        }
        ("GET", ["api", "eval", "runs", id]) => g
            .runs
            .iter()
            .find(|(t, r)| *t == tenant && r["id"].as_str() == Some(*id))
            .map(|(_, r)| {
                let mut r = r.clone();
                r["questions"] = json!([]);
                r["results"] = json!([]);
                ok(200, r)
            })
            .unwrap_or_else(|| err(404, "not_found", &format!("not found: eval run {id}"))),
        (m @ ("GET" | "PUT"), ["api", "tenant", "providers"]) => {
            if m == "PUT" {
                if let Some(r) = unknown_provider_field(&b, &RAG_CAPS, true) {
                    return r;
                }
                let mut view = json!({
                    "embed": rag_capability_view(b.get("embed"), "tenant"),
                    "rerank": rag_capability_view(b.get("rerank"), "tenant"),
                    "chat": rag_capability_view(b.get("chat"), "tenant"),
                });
                keep_keys(g.tenant_providers.get(&tenant), &mut view, &b);
                g.tenant_providers.insert(tenant, view);
            }
            ok(
                200,
                g.tenant_providers.get(&tenant).cloned().unwrap_or_else(|| {
                    json!({"embed": {"source": "default"}, "rerank": {"source": "default"}, "chat": {"source": "default"}})
                }),
            )
        }
        (m, ["api", "platform", rest @ ..]) => platform(g, tenant, m, rest, q, &b),
        ("POST", ["api", "tenant", "providers", "test"]) => {
            provider_test(g, tenant, &b, &["embed", "rerank", "chat"], true)
        }
        _ => empty(404),
    }
}

/// strata `valid_pack_key`: 1 to 6 `/`-separated segments of `[a-z0-9_-]+`.
fn valid_pack_key(key: &str) -> bool {
    let segs: Vec<&str> = key.split('/').collect();
    segs.len() <= 6
        && segs.iter().all(|s| {
            !s.is_empty()
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
        })
}

/// strata `key_matches_prefix`: the key itself or any key below it, segment-wise.
fn key_matches_prefix(key: &str, prefix: &str) -> bool {
    key == prefix || key.strip_prefix(prefix).is_some_and(|r| r.starts_with('/'))
}

/// strata's `DEFAULT_PLATFORM_LICENSES`.
pub const PLATFORM_LICENSES: [&str; 7] = [
    "CC0-1.0",
    "PDDL-1.0",
    "public-domain",
    "CC-BY-4.0",
    "CC-BY-SA-4.0",
    "OGL-UK-3.0",
    "ODC-BY-1.0",
];

/// `/api/platform/*` (`rest` after `api/platform`), strata_rag `src/api/platform.rs`.
fn platform(
    g: &mut Mock,
    tenant: Uuid,
    method: &str,
    rest: &[&str],
    q: &HashMap<String, String>,
    b: &Value,
) -> Response {
    if g.platform_off {
        return err(
            404,
            "not_found",
            "not found: platform knowledge is not enabled",
        );
    }
    let owner = g.platform_owner == Some(tenant);
    let forbidden = || {
        err(
            403,
            "forbidden",
            "forbidden: only the platform owner manages platform packs",
        )
    };
    let bad_path = || {
        err(
            422,
            "invalid_input",
            "invalid input: invalid pack in path (/{key}/{version}, key URL-encoded)",
        )
    };
    match (method, rest) {
        ("GET", ["packs"]) => {
            let packs: Vec<Value> = g
                .packs
                .iter()
                .filter(|p| owner || (g.pack_readable(tenant, p) && p["retired_at"].is_null()))
                .cloned()
                .collect();
            ok(200, json!({ "packs": packs }))
        }
        ("POST", ["packs"]) => {
            if !owner {
                return forbidden();
            }
            if b.as_object().is_none_or(|o| {
                o.keys()
                    .any(|k| !matches!(k.as_str(), "key" | "version" | "root_id"))
            }) {
                return err(422, "invalid_input", "invalid input: unknown field");
            }
            let key = b["key"].as_str().map(str::trim).unwrap_or("");
            let (Some(version), Some(root)) = (
                b["version"].as_i64(),
                b["root_id"].as_str().and_then(|s| Uuid::parse_str(s).ok()),
            ) else {
                return err(
                    422,
                    "invalid_input",
                    "invalid input: key, version and root_id are required",
                );
            };
            if !valid_pack_key(key) || version < 1 {
                return err(
                    422,
                    "invalid_input",
                    &format!(
                        "invalid input: key {key:?} must be 1 to 6 `/`-separated segments of [a-z0-9_-]"
                    ),
                );
            }
            let Some(store) = g.store(tenant, root) else {
                return not_found_store(root);
            };
            if store.parent_id.is_some() {
                return err(
                    422,
                    "invalid_target",
                    &format!("invalid target: store {root} is not a root store"),
                );
            }
            if g.pack_idx(key, version).is_some()
                || g.packs.iter().any(|p| p["root_id"] == json!(root))
            {
                return err(
                    409,
                    "duplicate_name",
                    &format!("duplicate name: pack {key}@{version}"),
                );
            }
            let pack = json!({
                "key": key, "version": version, "root_id": root, "published_at": null, "retired_at": null
            });
            g.packs.push(pack.clone());
            ok(201, pack)
        }
        ("POST", ["packs", key, version, action @ ("publish" | "retire")]) => {
            if !owner {
                return forbidden();
            }
            let key = decode(key);
            let Ok(version) = version.parse::<i64>() else {
                return bad_path();
            };
            let Some(i) = g.pack_idx(&key, version) else {
                return err(
                    404,
                    "not_found",
                    &format!("not found: pack {key}@{version}"),
                );
            };
            let pack = &mut g.packs[i];
            if *action == "publish" {
                if pack["published_at"].is_null() {
                    pack["published_at"] = json!(now());
                }
            } else {
                if pack["published_at"].is_null() {
                    return err(
                        422,
                        "invalid_target",
                        &format!(
                            "invalid target: pack {key}@{version} is a draft; publish it first"
                        ),
                    );
                }
                if pack["retired_at"].is_null() {
                    pack["retired_at"] = json!(now());
                }
            }
            ok(200, pack.clone())
        }
        ("GET", ["packs", key, version, "stores"]) => {
            let key = decode(key);
            let Ok(version) = version.parse::<i64>() else {
                return bad_path();
            };
            let Some(pack) = g
                .pack_idx(&key, version)
                .map(|i| g.packs[i].clone())
                .filter(|p| g.pack_readable(tenant, p))
            else {
                return err(
                    404,
                    "not_found",
                    &format!("not found: pack {key}@{version}"),
                );
            };
            let root = pack["root_id"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .unwrap_or_default();
            ok(
                200,
                json!({
                    "pack": format!("{key}@{version}"), "root_id": root, "stores": g.pack_subtree(root)
                }),
            )
        }
        ("GET", ["exclusions"]) => {
            let list: Vec<Value> = g
                .exclusions
                .iter()
                .filter(|(t, _)| *t == tenant)
                .map(|(_, x)| x.clone())
                .collect();
            ok(200, json!({ "exclusions": list }))
        }
        ("PUT", ["exclusions"]) => {
            if b.as_object().is_none_or(|o| {
                o.keys()
                    .any(|k| !matches!(k.as_str(), "key_prefix" | "reason"))
            }) {
                return err(422, "invalid_input", "invalid input: unknown field");
            }
            let prefix = b["key_prefix"].as_str().map(str::trim).unwrap_or("");
            if !valid_pack_key(prefix) {
                return err(
                    422,
                    "invalid_input",
                    &format!(
                        "invalid input: key_prefix {prefix:?} must be 1 to 6 `/`-separated segments of [a-z0-9_-]"
                    ),
                );
            }
            let reason = b["reason"].as_str().map(str::trim).unwrap_or("");
            if reason.is_empty() {
                return err(
                    422,
                    "invalid_input",
                    "invalid input: an exclusion needs a reason",
                );
            }
            let row = match g
                .exclusions
                .iter_mut()
                .find(|(t, x)| *t == tenant && x["key_prefix"] == json!(prefix))
            {
                Some((_, x)) => {
                    x["reason"] = json!(reason);
                    x.clone()
                }
                None => {
                    let x = json!({"key_prefix": prefix, "reason": reason, "created_at": now()});
                    g.exclusions.push((tenant, x.clone()));
                    x
                }
            };
            ok(200, row)
        }
        ("DELETE", ["exclusions"]) => {
            let Some(prefix) = q.get("key_prefix").map(|s| s.trim().to_string()) else {
                return err(
                    422,
                    "invalid_input",
                    "invalid input: missing field `key_prefix`",
                );
            };
            let before = g.exclusions.len();
            g.exclusions
                .retain(|(t, x)| !(*t == tenant && x["key_prefix"] == json!(prefix)));
            if g.exclusions.len() == before {
                return err(
                    404,
                    "not_found",
                    &format!("not found: exclusion {prefix:?}"),
                );
            }
            empty(204)
        }
        _ => empty(404),
    }
}

/// strata_rag JSON ingest (`POST /api/stores/{id}/documents`, `IngestJson`): the store must be the caller's leaf, not
/// in a published pack (409 `pack_published`); a store of [`Mock::platform_owner`] needs `provenance` with every
/// field set and an allow-listed licence ([`PLATFORM_LICENSES`], case-insensitive), else 422 with strata's
/// message. A document already in the store under the same `source_id` is answered 200 `existing`.
fn ingest_json(g: &mut Mock, tenant: Uuid, id: &str, b: &Value) -> Response {
    let Some(store) = Uuid::parse_str(id)
        .ok()
        .and_then(|i| g.store(tenant, i).cloned())
    else {
        return err(404, "not_found", &format!("not found: store {id}"));
    };
    if store.kind != "leaf" {
        return err(
            422,
            "invalid_target",
            &format!("invalid target: store {} is a router", store.id),
        );
    }
    if let Some(r) = g.frozen_pack(store.id) {
        return r;
    }
    if b["text"].as_str().is_none_or(|t| t.trim().is_empty()) {
        return err(
            422,
            "invalid_input",
            "invalid input: text must not be empty",
        );
    }
    let prov = &b["provenance"];
    if g.platform_owner == Some(store.tenant) {
        if !prov.is_object() {
            return err(
                422,
                "invalid_input",
                &format!(
                    "invalid input: store {} holds platform knowledge: provenance {{source_url, license, retrieved_at}} is required",
                    store.id
                ),
            );
        }
        for f in ["source_url", "license", "retrieved_at"] {
            if prov[f].as_str().is_none_or(|v| v.trim().is_empty()) {
                return err(
                    422,
                    "invalid_input",
                    &format!("invalid input: provenance.{f} is required"),
                );
            }
        }
        let lic = prov["license"].as_str().unwrap_or("").trim();
        if !PLATFORM_LICENSES
            .iter()
            .any(|a| a.eq_ignore_ascii_case(lic))
        {
            return err(
                422,
                "invalid_input",
                &format!(
                    "invalid input: licence `{lic}` is not allowed for platform knowledge; allowed (STRATA_PLATFORM_LICENSES): {}",
                    PLATFORM_LICENSES.join(", ")
                ),
            );
        }
    }
    let keys = |k: &str| -> Vec<String> {
        let mut v: Vec<String> = b[k]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v.dedup();
        v
    };
    if let Some(sid) = b["source_id"].as_str()
        && let Some((_, d)) = g.documents.iter().find(|(t, d)| {
            *t == tenant && d["store_id"] == json!(store.id) && d["source_id"] == json!(sid)
        })
    {
        let mut out = d.clone();
        out["status"] = json!("existing");
        return ok(200, out);
    }
    let mut metadata = b["metadata"].as_object().cloned().unwrap_or_default();
    if prov.is_object() {
        metadata.insert("provenance".into(), prov.clone());
    }
    let doc = json!({
        "id": Uuid::new_v4(), "store_id": store.id,
        "source": b["source"].as_str().unwrap_or("api"), "title": b["title"].clone(),
        "content_hash": "", "token_count": 1, "chunk_count": 1,
        "source_id": b["source_id"].clone(), "source_run": b["source_run"].clone(),
        "metadata": metadata, "topic_keys": keys("topic_keys"), "supersedes": keys("supersedes"),
        "created_at": now(),
    });
    g.documents.push((tenant, doc.clone()));
    let mut out = doc;
    out["status"] = json!("created");
    ok(201, out)
}

/// `POST /api/tenant/providers/test` (T0t) of either service: `{capability, dialect, base_url, model, api_key?}`
/// (rag also takes `model_id`, `dims`, `regions`, `zdr`, `timeout_ms`, `concurrency`, `thinking`, `temperature`;
/// ingest `model_id`, `regions`, `zdr`, `timeout_ms`; anything else 422 `invalid_input`). Stores nothing.
/// - capability not the service's → 422 `unknown_capability`; a `.internal` / loopback-named host → 422
///   `provider_host_not_allowed`;
/// - no `api_key` → the tenant's stored key of that capability, only for the same `base_url` (else 422
///   `secret_required_for_new_host`);
/// - the probe: a key containing `wrong` fails as the provider's 401 → 200 `ok: false`, `error_class: "auth"`;
///   anything else answers. rag: `{ok, capability, model, latency_ms, detail, dims?, error_class?}`; ingest
///   `{ok, detail}`.
pub fn provider_test(g: &Mock, tenant: Uuid, b: &Value, caps: &[&str], rag: bool) -> Response {
    let allowed: &[&str] = if rag {
        &[
            "capability",
            "dialect",
            "base_url",
            "model",
            "api_key",
            "model_id",
            "dims",
            "regions",
            "zdr",
            "timeout_ms",
            "concurrency",
            "thinking",
            "temperature",
        ]
    } else {
        &[
            "capability",
            "dialect",
            "base_url",
            "model",
            "api_key",
            "model_id",
            "regions",
            "zdr",
            "timeout_ms",
        ]
    };
    let Some(obj) = b.as_object() else {
        return err(
            422,
            "invalid_input",
            "invalid input: expected a JSON object",
        );
    };
    if let Some(k) = obj.keys().find(|k| !allowed.contains(&k.as_str())) {
        return err(
            422,
            "invalid_input",
            &format!("invalid input: unknown field `{k}`"),
        );
    }
    let text = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
    let (Some(cap), Some(_dialect), Some(base), Some(model)) = (
        text("capability"),
        text("dialect"),
        text("base_url"),
        text("model"),
    ) else {
        return err(
            422,
            "invalid_input",
            "invalid input: capability, dialect, base_url and model are required",
        );
    };
    if !caps.contains(&cap.as_str()) {
        return err(
            422,
            "unknown_capability",
            &format!("capability {cap:?} is not one this service serves"),
        );
    }
    if base.contains(".internal") || base.contains("localhost") {
        return err(
            422,
            "provider_host_not_allowed",
            &format!("{cap}: base_url host is not allowed"),
        );
    }
    let key = match text("api_key") {
        Some(k) => k,
        None => {
            let stored = g
                .tenant_providers
                .get(&tenant)
                .map(|v| v[cap.as_str()].clone())
                .unwrap_or(Value::Null);
            if stored["source"] != "tenant"
                || stored["has_api_key"] != true
                || stored["base_url"].as_str() != Some(base.trim_end_matches('/'))
            {
                return err(
                    422,
                    "secret_required_for_new_host",
                    &format!(
                        "{cap}: base_url differs from the stored registration, so its key is not sent there; send api_key"
                    ),
                );
            }
            "stored".to_string()
        }
    };
    let ok_probe = !key.contains("wrong");
    let detail = if ok_probe {
        format!("{cap}: the provider answered the probe")
    } else {
        format!("{cap}: the provider probe failed with error class auth")
    };
    if !rag {
        return ok(200, json!({"ok": ok_probe, "detail": detail}));
    }
    let mut out = json!({"ok": ok_probe, "capability": cap, "model": model, "latency_ms": 3, "detail": detail});
    if ok_probe && cap == "embed" {
        out["dims"] = json!(1024);
    }
    if !ok_probe {
        out["error_class"] = json!("auth");
    }
    ok(200, out)
}

fn question(b: &Value, query: &str) -> Value {
    json!({
        "id": Uuid::new_v4(), "query": query,
        "expected_document": b.get("expected_document").cloned().unwrap_or(Value::Null),
        "expected_leaf": b.get("expected_leaf").cloned().unwrap_or(Value::Null),
        "expected_document_ids": b.get("expected_document_ids").cloned().unwrap_or(json!([])),
        "expected_chunk_ids": b.get("expected_chunk_ids").cloned().unwrap_or(json!([])),
        "expected_text": b.get("expected_text").cloned().unwrap_or(Value::Null),
        "created_by": null, "created_at": now(), "updated_at": now(),
    })
}

// ---------------------------------------------------------------------------------------------------------------
// strata_ingest

const JOB_STATUSES: [&str; 8] = [
    "queued",
    "running",
    "cancelling",
    "completed",
    "failed",
    "rejected",
    "cancelled",
    "deleting",
];

fn ingest_bad(msg: &str) -> Response {
    err(400, "bad_request", msg)
}

fn job_view(tenant: Uuid, store: Uuid, source: Value, options: Value, meta: Value) -> Value {
    let t = now();
    json!({
        "id": Uuid::now_v7(), "tenant": tenant, "store_id": store, "source": source,
        "options": options, "meta": meta, "status": "queued", "error": null,
        "document_ids": [], "created_at": t, "updated_at": t, "steps": [],
    })
}

/// strata_ingest #15 fields of one job: (the fields for the job's `meta`, the `provenance_rejected` message when the
/// licence is refused).
type PkFields = (Map<String, Value>, Option<String>);

fn pk_key_ok(k: &str) -> bool {
    (1..=128).contains(&k.len())
        && k.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'/' | b'-')
        })
}

/// The #15 fields of one job: `top` (request-level) overridden field by field by `item` (a URL item's own). A bad
/// value is a 400 (before any job exists). `owner`: the store is the platform owner's, so provenance is required and
/// its licence must be allow-listed (else the job fails `provenance_rejected`).
fn pk_fields(owner: bool, top: &Value, item: Option<&Value>) -> Result<PkFields, Box<Response>> {
    let mut out = Map::new();
    for f in ["topic_keys", "supersedes", "provenance"] {
        let v = item
            .and_then(|i| i.get(f))
            .or_else(|| top.get(f))
            .filter(|v| !v.is_null());
        let Some(v) = v else { continue };
        if f == "provenance" {
            let shaped = ["source_url", "license", "retrieved_at"]
                .iter()
                .all(|k| v[*k].as_str().is_some_and(|s| !s.trim().is_empty()));
            let when_ok = v["retrieved_at"]
                .as_str()
                .is_some_and(|t| chrono::DateTime::parse_from_rfc3339(t).is_ok());
            if !shaped || !when_ok {
                return Err(Box::new(ingest_bad(
                    "invalid provenance: {source_url, license, retrieved_at (RFC 3339)} are required",
                )));
            }
        } else {
            let keys = v
                .as_array()
                .filter(|a| a.len() <= 32)
                .filter(|a| a.iter().all(|k| k.as_str().is_some_and(pk_key_ok)));
            if keys.is_none() {
                return Err(Box::new(ingest_bad(&format!(
                    "invalid {f}: at most 32 keys of 1..=128 characters of [a-z0-9_./-]"
                ))));
            }
        }
        out.insert(f.into(), v.clone());
    }
    if !owner {
        return Ok((out, None));
    }
    let Some(prov) = out.get("provenance") else {
        return Err(Box::new(ingest_bad(
            "invalid input: the store holds platform knowledge: provenance {source_url, license, retrieved_at} is required",
        )));
    };
    let lic = prov["license"].as_str().unwrap_or("").trim().to_string();
    let refused = (!PLATFORM_LICENSES
        .iter()
        .any(|a| a.eq_ignore_ascii_case(&lic)))
    .then(|| format!("licence `{lic}` is not allowed for platform knowledge"));
    Ok((out, refused))
}

/// `job` with the #15 `fields` in its meta; a refused licence fails it (`provenance_rejected`, not retried).
fn with_pk(mut job: Value, (fields, refused): &PkFields) -> Value {
    for (k, v) in fields {
        job["meta"][k.as_str()] = v.clone();
    }
    if let Some(m) = refused {
        job["status"] = json!("failed");
        job["error"] = json!({"code": "provenance_rejected", "message": m});
    }
    job
}

/// The text fields and file names of a multipart body (enough for the fake: no streaming, no limits).
fn multipart_parts(content_type: &str, body: &[u8]) -> Vec<(String, Option<String>, String)> {
    let Some(boundary) = content_type
        .split(';')
        .find_map(|p| p.trim().strip_prefix("boundary="))
        .map(|b| format!("--{}", b.trim_matches('"')))
    else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(body);
    text.split(boundary.as_str())
        .filter_map(|part| {
            let (head, value) = part.split_once("\r\n\r\n")?;
            let name = head.split("name=\"").nth(1)?.split('"').next()?.to_string();
            let file = head
                .split("filename=\"")
                .nth(1)
                .and_then(|f| f.split('"').next())
                .map(str::to_string);
            Some((name, file, value.trim_end_matches("\r\n").to_string()))
        })
        .collect()
}

fn job_idx(g: &Mock, tenant: Uuid, id: &str) -> Option<usize> {
    g.jobs
        .iter()
        .position(|(t, j)| *t == tenant && j["id"].as_str() == Some(id))
}

/// strata_ingest validates the target store against strata_rag; the fake knows [`Mock::leaves`] (empty = any).
fn known_leaf(g: &Mock, store: Uuid) -> bool {
    g.leaves.is_empty() || g.leaves.contains(&store)
}

#[allow(clippy::too_many_arguments)]
fn ingest(
    g: &mut Mock,
    tenant: Uuid,
    method: &Method,
    segs: &[&str],
    q: &HashMap<String, String>,
    content_type: Option<&str>,
    raw: &[u8],
    b: Value,
) -> Response {
    match (method.as_str(), segs) {
        ("POST", ["api", "ingest", "jobs"]) => {
            let media = content_type
                .and_then(|c| c.split(';').next())
                .map(|c| c.trim().to_ascii_lowercase())
                .unwrap_or_default();
            if media == "multipart/form-data" {
                let parts = multipart_parts(content_type.unwrap_or(""), raw);
                g.forms.push((
                    tenant,
                    parts
                        .iter()
                        .map(|(name, file_name, value)| FormPart {
                            name: name.clone(),
                            file_name: file_name.clone(),
                            value: value.clone(),
                        })
                        .collect(),
                ));
                let field = |n: &str| {
                    parts
                        .iter()
                        .find(|(k, f, _)| k == n && f.is_none())
                        .map(|(_, _, v)| v.clone())
                };
                let Some(store) = field("store_id").and_then(|s| Uuid::parse_str(&s).ok()) else {
                    return ingest_bad("a `store_id` field is required before the first file");
                };
                if !known_leaf(g, store) {
                    return err(404, "unknown_store", "unknown store");
                }
                let options = field("options")
                    .and_then(|o| serde_json::from_str::<Value>(&o).ok())
                    .unwrap_or_else(|| json!({"ocr": "auto", "captions": true, "language": null}));
                // strata_ingest #14 `source_run` and #15 `topic_keys` / `supersedes` / `provenance`: fields before
                // the files
                let first_file = parts.iter().position(|(_, f, _)| f.is_some());
                for name in ["source_run", "topic_keys", "supersedes", "provenance"] {
                    let at = parts.iter().position(|(k, f, _)| k == name && f.is_none());
                    if let (Some(r), Some(f)) = (at, first_file)
                        && r > f
                    {
                        return ingest_bad(&format!(
                            "`{name}` must come before the first file part"
                        ));
                    }
                }
                let mut top = Map::new();
                for name in ["topic_keys", "supersedes", "provenance"] {
                    if let Some(v) = field(name) {
                        let Ok(v) = serde_json::from_str::<Value>(&v) else {
                            return ingest_bad(&format!("invalid {name}: not JSON"));
                        };
                        top.insert(name.into(), v);
                    }
                }
                let owner = g.platform_owner == Some(tenant);
                let pk = match pk_fields(owner, &Value::Object(top), None) {
                    Ok(pk) => pk,
                    Err(r) => return *r,
                };
                let mut meta = field("source_id")
                    .map(|s| json!({"source_id": s}))
                    .unwrap_or_else(|| json!({}));
                if let Some(r) = field("source_run") {
                    meta["source_run"] = json!(r);
                }
                let files: Vec<String> = parts
                    .iter()
                    .filter(|(k, f, _)| k == "file" && f.is_some())
                    .filter_map(|(_, f, _)| f.clone())
                    .collect();
                if files.is_empty() {
                    return ingest_bad("at least one `file` part is required");
                }
                let mut out = Vec::new();
                for f in files {
                    let job = with_pk(
                        job_view(
                            tenant,
                            store,
                            json!({"kind": "upload", "filename": f}),
                            options.clone(),
                            meta.clone(),
                        ),
                        &pk,
                    );
                    out.push(json!({"job_id": job["id"], "status": "queued"}));
                    g.jobs.push((tenant, job));
                }
                return ok(202, json!({ "jobs": out }));
            }
            if media != "application/json" {
                return ingest_bad("Content-Type must be multipart/form-data or application/json");
            }
            let Some(store) = b["store_id"].as_str().and_then(|s| Uuid::parse_str(s).ok()) else {
                return ingest_bad("invalid body: missing field `store_id`");
            };
            if !known_leaf(g, store) {
                return err(404, "unknown_store", "unknown store");
            }
            let mut meta = b["source_id"]
                .as_str()
                .map(|s| json!({"source_id": s}))
                .unwrap_or_else(|| json!({}));
            if let Some(r) = b["source_run"].as_str() {
                meta["source_run"] = json!(r);
            }
            let options = b
                .get("options")
                .cloned()
                .filter(|o| !o.is_null())
                .unwrap_or_else(|| json!({"ocr": "auto", "captions": true, "language": null}));
            let owner = g.platform_owner == Some(tenant);
            if let Some(source) = b.get("source").filter(|s| s.is_object()) {
                let pk = match pk_fields(owner, &b, None) {
                    Ok(pk) => pk,
                    Err(r) => return *r,
                };
                let job = with_pk(job_view(tenant, store, source.clone(), options, meta), &pk);
                let id = job["id"].clone();
                g.jobs.push((tenant, job));
                return ok(
                    202,
                    json!({"jobs": [{"job_id": id, "status": "queued"}], "job_id": id}),
                );
            }
            // #15: a URL item is a string or `{url, topic_keys?, supersedes?, provenance?}`
            let items: Vec<Value> = match (b.get("url"), b.get("urls")) {
                (Some(u), None) => vec![u.clone()],
                (None, Some(us)) => us.as_array().cloned().unwrap_or_default(),
                _ => Vec::new(),
            };
            if items.is_empty() || items.len() > 50 {
                return ingest_bad("exactly one of `url` or `urls` is required");
            }
            let mut planned = Vec::with_capacity(items.len());
            for it in &items {
                let (u, own) = match it {
                    Value::String(u) => (u.clone(), None),
                    Value::Object(o) => match o.get("url").and_then(Value::as_str) {
                        Some(u) => (u.to_string(), Some(it)),
                        None => return ingest_bad("invalid body: a URL item needs `url`"),
                    },
                    _ => return ingest_bad("invalid body: a URL item is a string or an object"),
                };
                match pk_fields(owner, &b, own) {
                    Ok(pk) => planned.push((u, pk)),
                    Err(r) => return *r,
                }
            }
            let mut out = Vec::new();
            for (u, pk) in planned {
                let job = with_pk(
                    job_view(
                        tenant,
                        store,
                        json!({"kind": "url", "url": u}),
                        options.clone(),
                        meta.clone(),
                    ),
                    &pk,
                );
                out.push(json!({"job_id": job["id"], "status": "queued"}));
                g.jobs.push((tenant, job));
            }
            ok(202, json!({ "jobs": out }))
        }
        ("GET", ["api", "ingest", "jobs"]) => {
            if let Some(st) = q.get("status")
                && !JOB_STATUSES.contains(&st.as_str())
            {
                return ingest_bad(&format!(
                    "unknown status `{st}`; one of {}",
                    JOB_STATUSES.join(", ")
                ));
            }
            let limit = q
                .get("limit")
                .and_then(|l| l.parse::<i64>().ok())
                .unwrap_or(50);
            if limit < 1 {
                return ingest_bad("`limit` must be >= 1");
            }
            let jobs: Vec<Value> = g
                .jobs
                .iter()
                .rev()
                .filter(|(t, _)| *t == tenant)
                .filter(|(_, j)| q.get("store_id").is_none_or(|s| j["store_id"] == json!(s)))
                .filter(|(_, j)| q.get("status").is_none_or(|s| j["status"] == json!(s)))
                .map(|(_, j)| j.clone())
                .take(usize::try_from(limit.min(200)).unwrap_or(200))
                .collect();
            ok(200, json!({"jobs": jobs, "next": null}))
        }
        (m @ ("GET" | "DELETE"), ["api", "ingest", "jobs", id]) => {
            let Some(i) = job_idx(g, tenant, id) else {
                return err(404, "not_found", "not found");
            };
            if m == "GET" {
                return ok(200, g.jobs[i].1.clone());
            }
            let job = g.jobs[i].1.clone();
            // strata keeps a page job's parent in `source.parent_job_id` (`db::jobs::add_children`)
            if let Some(parent) = job["source"]["parent_job_id"]
                .as_str()
                .or_else(|| job["meta"]["parent_job_id"].as_str())
            {
                return ok(
                    409,
                    json!({"error": {"code": "child_of_parent_job", "message": "this job is a crawl child; delete its parent job", "parent_job_id": parent}}),
                );
            }
            if matches!(
                job["status"].as_str(),
                Some("queued" | "running" | "cancelling")
            ) {
                return err(
                    409,
                    "conflict",
                    "conflict: the job is still running; cancel it first",
                );
            }
            g.jobs.remove(i);
            ok(
                200,
                json!({"job_id": job["id"], "status": "deleted", "deleted_jobs": 1, "deleted_objects": 0, "document_ids": job["document_ids"], "documents": "retained"}),
            )
        }
        ("POST", ["api", "ingest", "jobs", id, action @ ("cancel" | "retry")]) => {
            let Some(i) = job_idx(g, tenant, id) else {
                return err(404, "not_found", "not found");
            };
            let status = g.jobs[i].1["status"].as_str().unwrap_or("").to_string();
            let next = match (*action, status.as_str()) {
                ("retry", "failed") => "queued",
                ("retry", _) => {
                    return err(
                        409,
                        "conflict",
                        "conflict: only a failed job can be retried",
                    );
                }
                ("cancel", "completed" | "failed" | "rejected" | "deleting") => {
                    return err(409, "conflict", "conflict: the job has ended");
                }
                ("cancel", "running") => "cancelling",
                _ => "cancelled",
            };
            g.jobs[i].1["status"] = json!(next);
            g.jobs[i].1["updated_at"] = json!(now());
            ok(200, g.jobs[i].1.clone())
        }
        (m @ ("GET" | "PUT"), ["api", "tenant", "providers"]) => {
            if m == "PUT" {
                if let Some(r) = unknown_provider_field(&b, &INGEST_CAPS, false) {
                    return r;
                }
                let mut view = json!({
                    "ocr": rag_capability_view(b.get("ocr"), "tenant"),
                    "vision": rag_capability_view(b.get("vision"), "tenant"),
                    "transcribe": rag_capability_view(b.get("transcribe"), "tenant"),
                });
                keep_keys(g.tenant_providers.get(&tenant), &mut view, &b);
                g.tenant_providers.insert(tenant, view);
            }
            ok(
                200,
                g.tenant_providers.get(&tenant).cloned().unwrap_or_else(|| {
                    json!({"ocr": {"source": "default"}, "vision": {"source": "default"}, "transcribe": {"source": "default"}})
                }),
            )
        }
        (m @ ("GET" | "PUT" | "DELETE"), ["api", "stores", id, "providers"]) => {
            let Some(id) = Uuid::parse_str(id)
                .ok()
                .filter(|i| g.leaves.is_empty() || g.leaves.contains(i))
            else {
                return err(404, "unknown_store", "unknown store");
            };
            match m {
                "PUT" => {
                    if let Some(r) = unknown_provider_field(&b, &INGEST_CAPS, false) {
                        return r;
                    }
                    g.store_providers.insert(id, b);
                }
                "DELETE" => {
                    g.store_providers.remove(&id);
                }
                _ => {}
            }
            let own = g.store_providers.get(&id).cloned().unwrap_or(Value::Null);
            ok(
                200,
                json!({
                    "store_id": id,
                    "ocr": rag_capability_view(own.get("ocr"), "store"),
                    "vision": rag_capability_view(own.get("vision"), "store"),
                    "transcribe": rag_capability_view(own.get("transcribe"), "store"),
                }),
            )
        }
        // T0i rest (strata_ingest #13 `be0c669`, T4a): a copy starts `running`; tests move it on in `reingest_jobs`
        ("POST", ["api", "stores", id, "reingest-copy"]) => {
            let Some(src) = Uuid::parse_str(id).ok() else {
                return err(404, "unknown_store", "unknown store");
            };
            let Some(target) = b["target_store"]
                .as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
            else {
                return err(400, "bad_request", "target_store is required");
            };
            let job = json!({
                "job": Uuid::new_v4(), "source_store": src, "target_store": target, "state": "running",
                "documents_done": 0, "documents_total": 0, "documents_failed": 0, "documents_missing": 0,
                "missing_originals": [], "passes": 0, "started_at": now()
            });
            g.reingest_jobs.push((tenant, job.clone()));
            ok(202, job)
        }
        ("GET", ["api", "stores", id, "reingest-copy", job]) => g
            .reingest_jobs
            .iter()
            .find(|(t, j)| *t == tenant && j["source_store"] == json!(id) && j["job"] == json!(job))
            .map(|(_, j)| ok(200, j.clone()))
            .unwrap_or_else(|| err(404, "not_found", "not found")),
        ("POST", ["api", "tenant", "providers", "test"]) => {
            provider_test(g, tenant, &b, &["ocr", "vision", "transcribe"], false)
        }
        _ => empty(404),
    }
}
