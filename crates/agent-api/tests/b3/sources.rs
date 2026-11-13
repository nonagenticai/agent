//! B3 T7: a job's knowledge sources (plan §5.4, §7 G3): create / save / delete with the fixture's refusal codes, the
//! sealed write-only credentials (§0 R2, AAD `source_cred`), the mapping into the job's draft ontology (B5 J11 targets), the purge through
//! strata_rag's prune, the test connection (one guarded GET, the saved key only when the draft names none) and the
//! column preview (CSV from the source URL, an uploaded XLSX with a sheet and no header row, and every refusal).
//! Real Postgres 18 (one container for this binary, a fresh database per test), the in-process strata fakes
//! (`common::strata_fake`) and a mock source host on 127.0.0.1, reached through the test-only loopback netguard
//! policy.
use crate::common;

use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::sync::{Arc, Mutex};

use crate::common::strata_fake::{self, StrataFake, lock};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use agent_api::connectors::transport::Transport;
use agent_api::netguard::Policy;
use agent_api::providers::secret::{self, SecretKind};
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get as route_get;
use base64::Engine as _;
use serde_json::{Value, json};
use uuid::Uuid;

/// The key the mock source host accepts.
const GOOD: &str = "src-key-good-0001";
/// Another key (replaces GOOD in the save test).
const OTHER: &str = "src-key-other-0002";
const JOB: &str = "support";

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
    vars.insert(
        "PROVIDER_SECRET_KEYS".to_string(),
        format!(r#"{{"s1":"{key}"}}"#),
    );
    vars.insert("PROVIDER_SECRET_KEY_CURRENT".to_string(), "s1".to_string());
}

// ---- the mock source host ---------------------------------------------------------------------------------------------

/// Every (path, `Authorization` header) the host saw, in order.
#[derive(Clone, Default)]
struct Hits(Arc<Mutex<Vec<(String, Option<String>)>>>);

impl Hits {
    fn all(&self) -> Vec<(String, Option<String>)> {
        self.0.lock().unwrap().clone()
    }

    fn record(&self, path: &str, h: &HeaderMap) -> Option<String> {
        let a = h
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        self.0.lock().unwrap().push((path.to_string(), a.clone()));
        a
    }
}

async fn promos(State(h): State<Hits>, headers: HeaderMap) -> Response {
    let auth = h.record("/promos", &headers);
    if auth.as_deref() == Some(format!("Bearer {GOOD}").as_str()) {
        axum::Json(json!([
            {"code": "AUTUMN15", "title": "15% off throws", "percent": 15},
            {"code": "LAMPS10", "title": "10% off lamps", "percent": 10}
        ]))
        .into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error": "unauthorized"})),
        )
            .into_response()
    }
}

async fn products(State(h): State<Hits>, headers: HeaderMap) -> Response {
    h.record("/products.csv", &headers);
    (
        [(header::CONTENT_TYPE, "text/csv")],
        "sku,title,price_eur\nLL-10204,Oslo sofa,1149.00\nLL-10311,Lune lamp,189.00\n",
    )
        .into_response()
}

async fn page(State(h): State<Hits>, headers: HeaderMap) -> Response {
    h.record("/page", &headers);
    (
        [(header::CONTENT_TYPE, "text/html")],
        "<html><body>Returns</body></html>",
    )
        .into_response()
}

/// The host's base URL (`http://127.0.0.1:<port>`) and what it saw.
async fn source_host() -> (String, Hits) {
    let hits = Hits::default();
    let app = Router::new()
        .route("/promos", route_get(promos))
        .route("/products.csv", route_get(products))
        .route("/page", route_get(page))
        .with_state(hits.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (format!("http://{addr}"), hits)
}

// ---- the world --------------------------------------------------------------------------------------------------------

struct W {
    e: TestEnv,
    /// The API with the loopback netguard policy.
    app: Router,
    fake: StrataFake,
    host: String,
    hits: Hits,
    tenant: Uuid,
    leaf: Uuid,
    group: Uuid,
    /// knowledge_manager
    km: String,
    /// an observer (no role)
    obs: String,
}

/// A member of `tenant` holding `role`; its token.
async fn member(e: &TestEnv, tenant: Uuid, role: &str) -> String {
    let sub = format!("u-{role}-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, role).await;
    person(&sub, &[])
}

/// A store of `tenant` inserted as the owner; its id.
async fn store(e: &TestEnv, tenant: Uuid, key: &str, kind: &str, parent: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO knowledge_store (tenant_id, id, key, label, parent_id, kind, region, updated_by) \
         VALUES ($1, $2, $3, $3, $4, $5, 'eu', 'test')",
    )
    .bind(tenant)
    .bind(id)
    .bind(key)
    .bind(parent)
    .bind(kind)
    .execute(&e.owner)
    .await
    .unwrap();
    id
}

/// A fresh tenant with the job `support` (serving nl, en), a group `faq` and its leaf `faq.policy`, the keyring and
/// the strata fakes.
async fn world() -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| {
        fake.env(&[tenant], v);
        with_keys(v);
    })
    .await;
    insert_tenant(&e.owner, tenant, &format!("T7 {tenant}")).await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, languages, created_by) \
         VALUES ($1, $2, 'Support', '[]', ARRAY['nl', 'en'], 'test')",
    )
    .bind(tenant)
    .bind(JOB)
    .execute(&e.owner)
    .await
    .unwrap();
    let group = store(&e, tenant, "faq", "group", None).await;
    let leaf = store(&e, tenant, "faq.policy", "leaf", Some(group)).await;
    let km = member(&e, tenant, "knowledge_manager").await;
    let obs = member(&e, tenant, "tenant_user").await;
    let mut st = e.state.clone();
    st.connectors = Transport::new(Policy {
        allow_loopback: true,
        private_targets: vec![],
    });
    let app = agent_api::router(st);
    let (host, hits) = source_host().await;
    W {
        e,
        app,
        fake,
        host,
        hits,
        tenant,
        leaf,
        group,
        km,
        obs,
    }
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

fn uri(rest: &str) -> String {
    format!("/api/jobs/{JOB}/sources{rest}")
}

/// `base` with the keys of `over` set (a `null` in `over` sets null).
fn merged(base: Value, over: Value) -> Value {
    let mut b = base;
    if let (Some(b), Some(o)) = (b.as_object_mut(), over.as_object()) {
        for (k, v) in o {
            b.insert(k.clone(), v.clone());
        }
    }
    b
}

impl W {
    async fn call(&self, tok: &str, method: &str, rest: &str, body: Value) -> (StatusCode, Value) {
        send(&self.app, json(method, &uri(rest), Some(tok), body)).await
    }

    async fn read(&self, rest: &str) -> (StatusCode, Value) {
        send(&self.app, get(&uri(rest), Some(&self.obs))).await
    }

    fn website(&self, name: &str) -> Value {
        json!({
            "name": name, "type": "website",
            "config": {"urls": [format!("{}/page", self.host)]},
            "mapping": [], "target_store_id": self.leaf, "schedule": null, "enabled": true
        })
    }

    fn api(&self, name: &str) -> Value {
        json!({
            "name": name, "type": "api",
            "config": {"url": format!("{}/promos", self.host), "auth": {"type": "bearer"}},
            "mapping": [], "target_store_id": self.leaf, "schedule": null, "enabled": true
        })
    }

    fn datatable(&self, name: &str) -> Value {
        json!({
            "name": name, "type": "datatable",
            "config": {"url": format!("{}/products.csv", self.host), "format": "csv"},
            "mapping": [], "target_store_id": self.leaf, "schedule": null, "enabled": true
        })
    }

    /// Creates `body` as the knowledge_manager (201 expected); the source.
    async fn create(&self, body: Value) -> Value {
        let (s, b) = self.call(&self.km, "POST", "", body).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        b
    }

    async fn sealed(&self, id: &str) -> Option<String> {
        sqlx::query_scalar("SELECT credentials_sealed FROM source WHERE id = $1::uuid")
            .bind(id)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }

    fn open(&self, id: &str, sealed: &str) -> String {
        secret::open(
            self.e.state.provider_keyring.as_deref(),
            self.tenant,
            SecretKind::SourceCred,
            Uuid::parse_str(id).unwrap(),
            sealed,
        )
        .unwrap()
        .expose()
        .to_string()
    }

    /// The `details` of every `action` row about `id`, oldest first.
    async fn audit(&self, action: &str, id: &str) -> Vec<Value> {
        sqlx::query_scalar(
            "SELECT details FROM audit_event WHERE action = $1 AND resource_id = $2 ORDER BY id",
        )
        .bind(action)
        .bind(id)
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
    }

    /// A run of source `id` inserted as the owner.
    async fn run(&self, id: &str, status: &str, documents: i32) {
        sqlx::query(
            "INSERT INTO source_run (tenant_id, job_id, id, source_id, status, finished_at, started_by, documents) \
             SELECT $1, j.id, $2, $3::uuid, $4, CASE WHEN $4 = 'running' THEN NULL ELSE now() END, 'test', $5 \
               FROM jobs j WHERE j.tenant_id = $1 AND j.key = $6",
        )
        .bind(self.tenant)
        .bind(Uuid::new_v4())
        .bind(id)
        .bind(status)
        .bind(documents)
        .bind(JOB)
        .execute(&self.e.owner)
        .await
        .unwrap();
    }

    /// A strata_rag document of source `source_id` in the fake.
    fn doc(&self, source_id: &str) {
        lock(&self.fake.rag).documents.push((
            self.tenant,
            json!({
                "id": Uuid::new_v4(), "store_id": Uuid::new_v4(), "source": "api", "title": "t",
                "token_count": 10, "chunk_count": 1, "source_id": source_id, "source_run": "r1",
                "metadata": {}, "created_at": "2026-10-21T10:00:00Z"
            }),
        ));
    }

    fn docs_of(&self, source_id: &str) -> usize {
        lock(&self.fake.rag)
            .documents
            .iter()
            .filter(|(_, d)| d["source_id"] == json!(source_id))
            .count()
    }

    /// Source `id`'s row, and the tenant's run and audit counts.
    async fn snapshot(&self, id: &str) -> (String, i64, i64) {
        let row: String =
            sqlx::query_scalar("SELECT row_to_json(s)::text FROM source s WHERE s.id = $1::uuid")
                .bind(id)
                .fetch_one(&self.e.owner)
                .await
                .unwrap();
        let runs = self
            .count("SELECT count(*) FROM source_run WHERE tenant_id = $1")
            .await;
        let audit = self
            .count("SELECT count(*) FROM audit_event WHERE tenant_id = $1")
            .await;
        (row, runs, audit)
    }

    async fn count(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql)
            .bind(self.tenant)
            .fetch_one(&self.e.owner)
            .await
            .unwrap()
    }
}

// ---- create ------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_validations() {
    let w = world().await;
    // a website source in `urls` mode (config.urls[]); the language defaults to the job's first (D68)
    let ok = w.create(w.website("Help centre")).await;
    assert_eq!(ok["job"], JOB);
    assert_eq!(ok["type"], "website");
    assert_eq!(ok["language"], "nl");
    assert_eq!(ok["config"]["urls"][0], format!("{}/page", w.host));
    assert_eq!(ok["has_credentials"], false);
    assert_eq!(ok["last_run"], Value::Null);
    let dup_field = json!([
        {"source_field": "price", "entity": "Offering", "attribute": "price"},
        {"source_field": " Price", "entity": "Offering", "attribute": "price"}
    ]);
    let cases: Vec<(Value, StatusCode, &str)> = vec![
        (
            merged(w.website("x"), json!({"name": "   "})),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_source",
        ),
        (
            w.website("Help centre"),
            StatusCode::CONFLICT,
            "source_exists",
        ),
        (
            merged(w.website("Group"), json!({"target_store_id": w.group})),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_target_store",
        ),
        (
            merged(
                w.website("Nowhere"),
                json!({"target_store_id": Uuid::new_v4()}),
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_target_store",
        ),
        (
            merged(
                w.website("CGNAT"),
                json!({"config": {"urls": ["https://100.64.0.1/feed"]}}),
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
            "address_refused",
        ),
        (
            merged(
                w.website("Cluster"),
                json!({"config": {"mode": "sitemap", "url": "https://minio.internal/sitemap.xml"}}),
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
            "address_refused",
        ),
        (
            merged(w.api("No URL"), json!({"config": {}})),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_source",
        ),
        (
            merged(
                w.api("Plain http"),
                json!({"config": {"url": "http://api.example.com/promos"}}),
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_source",
        ),
        (
            merged(w.website("German"), json!({"language": "de"})),
            StatusCode::UNPROCESSABLE_ENTITY,
            "language_not_served",
        ),
        (
            merged(w.website("Daily"), json!({"schedule": "every day at 3"})),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_schedule",
        ),
        (
            merged(w.api("Dup"), json!({"mapping": dup_field})),
            StatusCode::UNPROCESSABLE_ENTITY,
            "duplicate_field",
        ),
        (
            merged(
                w.api("Unknown"),
                json!({"mapping": [{"source_field": "code", "entity": "Offering", "attribute": "code"}]}),
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_attribute",
        ),
        (
            merged(
                w.api("Leaky"),
                json!({"config": {"url": format!("{}/promos", w.host), "token": "abc"}}),
            ),
            StatusCode::UNPROCESSABLE_ENTITY,
            "secret_rejected",
        ),
    ];
    for (body, status, want) in cases {
        let (s, b) = w.call(&w.km, "POST", "", body.clone()).await;
        assert_eq!((s, code(&b)), (status, want), "{body}: {b}");
    }
    // a served language other than the first is kept
    let en = w
        .create(merged(w.website("English"), json!({"language": "en"})))
        .await;
    assert_eq!(en["language"], "en");
    // nothing refused was stored
    let n = w
        .count("SELECT count(*) FROM source WHERE tenant_id = $1")
        .await;
    assert_eq!(n, 2);
}

#[tokio::test]
async fn create_seals_credentials_never_returns_them() {
    let w = world().await;
    let b = w
        .create(merged(
            w.api("Promotions"),
            json!({"credentials": {"secret": GOOD}}),
        ))
        .await;
    let id = b["id"].as_str().unwrap().to_string();
    assert_eq!(b["has_credentials"], true, "{b}");
    assert!(b["credentials_updated_at"].is_string(), "{b}");
    assert!(
        !b.to_string().contains(GOOD),
        "the key is never returned: {b}"
    );
    let sealed = w.sealed(&id).await.expect("sealed");
    assert!(
        sealed.starts_with("v2:") && !sealed.contains(GOOD),
        "{sealed}"
    );
    assert_eq!(w.open(&id, &sealed), GOOD, "sealed for this tenant and row");
    // bound to the row: the same value does not open as another source's
    assert!(
        secret::open(
            w.e.state.provider_keyring.as_deref(),
            w.tenant,
            SecretKind::SourceCred,
            Uuid::new_v4(),
            &sealed
        )
        .is_err()
    );
    let audit = w.audit("source.create", &id).await;
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["credentials"], "set");
    assert!(!audit[0].to_string().contains(GOOD));
    for rest in ["".to_string(), format!("/{id}")] {
        let (s, b) = w.read(&rest).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(!b.to_string().contains(GOOD), "{b}");
        assert!(!b.to_string().contains("v2:"), "{b}");
    }
    // a key on a source that sends none is refused
    let (s, b) = w
        .call(
            &w.km,
            "POST",
            "",
            merged(
                w.website("Keyed site"),
                json!({"credentials": {"secret": GOOD}}),
            ),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_source"),
        "{b}"
    );
}

// ---- save --------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn save_type_change_422_type_fixed() {
    let w = world().await;
    let b = w.create(w.api("Promotions")).await;
    let id = b["id"].as_str().unwrap();
    let (s, b) = w
        .call(&w.km, "PUT", &format!("/{id}"), w.datatable("Promotions"))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "type_fixed"),
        "{b}"
    );
    // an unknown id is 404 before the body is read
    let (s, b) = w
        .call(&w.km, "PUT", &format!("/{}", Uuid::new_v4()), json!({}))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "source_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn save_credentials_keep_clear_replace() {
    let w = world().await;
    let b = w
        .create(merged(
            w.api("Promotions"),
            json!({"credentials": {"secret": GOOD}}),
        ))
        .await;
    let id = b["id"].as_str().unwrap().to_string();
    let first = w.sealed(&id).await.unwrap();
    // absent = keep (a rename does not touch the key)
    let (s, b) = w
        .call(&w.km, "PUT", &format!("/{id}"), w.api("Promotions NL"))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (b["name"].as_str(), &b["has_credentials"]),
        (Some("Promotions NL"), &json!(true))
    );
    assert_eq!(w.sealed(&id).await.as_deref(), Some(first.as_str()));
    // {secret} = replace
    let (s, b) = w
        .call(
            &w.km,
            "PUT",
            &format!("/{id}"),
            merged(
                w.api("Promotions NL"),
                json!({"credentials": {"secret": OTHER}}),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(!b.to_string().contains(OTHER), "{b}");
    let second = w.sealed(&id).await.unwrap();
    assert_ne!(second, first);
    assert_eq!(w.open(&id, &second), OTHER);
    // null = clear
    let (s, b) = w
        .call(
            &w.km,
            "PUT",
            &format!("/{id}"),
            merged(w.api("Promotions NL"), json!({"credentials": null})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["has_credentials"], false);
    assert_eq!(b["credentials_updated_at"], Value::Null);
    assert_eq!(w.sealed(&id).await, None);
    let words: Vec<Value> = w
        .audit("source.update", &id)
        .await
        .into_iter()
        .map(|d| d["credentials"].clone())
        .collect();
    assert_eq!(words, [json!("kept"), json!("replaced"), json!("cleared")]);
}

#[tokio::test]
async fn save_keeping_key_for_new_host_422() {
    let w = world().await;
    let b = w
        .create(merged(
            w.api("Promotions"),
            json!({"credentials": {"secret": GOOD}}),
        ))
        .await;
    let id = b["id"].as_str().unwrap().to_string();
    let first = w.sealed(&id).await.unwrap();
    let before = w.snapshot(&id).await;
    let moved_config = json!({"url": w.host.replace("127.0.0.1", "127.0.0.2") + "/promos",
                              "auth": {"type": "bearer"}});
    // absent credentials = keep: the stored key never follows the URL to another host
    let moved = merged(w.api("Promotions"), json!({"config": moved_config.clone()}));
    let (s, b) = w.call(&w.km, "PUT", &format!("/{id}"), moved).await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "secret_required_for_new_host"
        ),
        "{b}"
    );
    assert_eq!(
        w.snapshot(&id).await,
        before,
        "the refused save changed nothing"
    );
    // a path edit on the same origin keeps the key
    let same = merged(
        w.api("Promotions"),
        json!({"config": {"url": format!("{}/promos/", w.host),
                          "auth": {"type": "bearer"}}}),
    );
    let (s, b) = w.call(&w.km, "PUT", &format!("/{id}"), same).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(w.sealed(&id).await.as_deref(), Some(first.as_str()));
    // the new host with the key typed again, or with the key cleared, is accepted
    let retyped = merged(
        w.api("Promotions"),
        json!({"config": moved_config.clone(), "credentials": {"secret": OTHER}}),
    );
    let (s, b) = w.call(&w.km, "PUT", &format!("/{id}"), retyped).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(w.open(&id, &w.sealed(&id).await.unwrap()), OTHER);
    let back = merged(w.api("Promotions"), json!({"credentials": null}));
    let (s, b) = w.call(&w.km, "PUT", &format!("/{id}"), back).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(w.sealed(&id).await, None);
    // without a stored key the URL moves freely
    let moved = merged(w.api("Promotions"), json!({"config": moved_config}));
    let (s, b) = w.call(&w.km, "PUT", &format!("/{id}"), moved).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

// ---- mapping: the job's draft ontology (B5 J11) -----------------------------------------------------------------------

impl W {
    /// A tenant entity `name` of the job's draft ontology with `attributes`, inserted as the owner.
    async fn entity(&self, name: &str, hidden: bool, attributes: Value) {
        sqlx::query(
            "INSERT INTO ontology_entity (tenant_id, job_id, name, label, scope, attributes, hidden, created_by) \
             SELECT tenant_id, id, $3, $3, 'generic', $4, $5, 'test' FROM jobs WHERE tenant_id = $1 AND key = $2",
        )
        .bind(self.tenant)
        .bind(JOB)
        .bind(name)
        .bind(attributes)
        .bind(hidden)
        .execute(&self.e.owner)
        .await
        .unwrap();
    }
}

#[tokio::test]
async fn mapping_targets_list_the_draft_ontology_attributes() {
    let w = world().await;
    let (s, b) = w.read("/mapping-targets").await;
    assert_eq!(
        (s, b),
        (StatusCode::OK, json!([])),
        "no ontology, no targets"
    );
    w.entity(
        "Offering",
        false,
        json!([
            {"name": "sku", "data_type": "string", "pii": false, "synonyms": []},
            {"name": "price", "data_type": "number", "pii": false, "synonyms": []}
        ]),
    )
    .await;
    w.entity(
        "Customer",
        false,
        json!([{"name": "email", "data_type": "string", "pii": true, "synonyms": []}]),
    )
    .await;
    w.entity(
        "Retired",
        true,
        json!([{"name": "code", "data_type": "string", "pii": false, "synonyms": []}]),
    )
    .await;
    let (s, b) = w.read("/mapping-targets").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!([
            {"entity": "Customer", "attribute": "email", "type": "string", "pii": true},
            {"entity": "Offering", "attribute": "sku", "type": "string", "pii": false},
            {"entity": "Offering", "attribute": "price", "type": "number", "pii": false}
        ]),
        "every attribute of a visible entity; the hidden one is not a target"
    );
}

#[tokio::test]
async fn create_structured_mapping_accepts_ontology_attributes_and_422_unknown_attribute() {
    let w = world().await;
    w.entity(
        "Offering",
        false,
        json!([{"name": "sku", "data_type": "string", "pii": false, "synonyms": []}]),
    )
    .await;
    // a row naming an attribute of the ontology is stored (trimmed)
    let b = w
        .create(merged(
            w.datatable("Catalogue"),
            json!({"mapping": [{"source_field": " sku ", "entity": "Offering", "attribute": "sku"}]}),
        ))
        .await;
    assert_eq!(
        b["mapping"],
        json!([{"source_field": "sku", "entity": "Offering", "attribute": "sku"}])
    );
    // an attribute the ontology does not have, or an entity it does not have, is refused
    for (entity, attribute) in [("Offering", "colour"), ("Warranty", "sku")] {
        let mapped = merged(
            w.datatable("Catalogue 2"),
            json!({"mapping": [{"source_field": "x", "entity": entity, "attribute": attribute}]}),
        );
        let (s, b) = w.call(&w.km, "POST", "", mapped).await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "unknown_attribute"),
            "{entity}.{attribute}: {b}"
        );
    }
    // a hidden entity's attributes are not targets
    w.entity(
        "Retired",
        true,
        json!([{"name": "code", "data_type": "string", "pii": false, "synonyms": []}]),
    )
    .await;
    let (s, b) = w
        .call(
            &w.km,
            "POST",
            "",
            merged(
                w.datatable("Catalogue 3"),
                json!({"mapping": [{"source_field": "c", "entity": "Retired", "attribute": "code"}]}),
            ),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_attribute"),
        "{b}"
    );
    // a website has no fields to map: a mapping sent with it is not stored
    let b = w
        .create(merged(
            w.website("Help"),
            json!({"mapping": [{"source_field": "x", "entity": "Offering", "attribute": "x"}]}),
        ))
        .await;
    assert_eq!(b["mapping"], json!([]));
}

#[tokio::test]
async fn mapping_duplicate_field_422() {
    let w = world().await;
    let body = merged(
        w.api("Promotions"),
        json!({"mapping": [
            {"source_field": "percent", "entity": "Offering", "attribute": "discount"},
            {"source_field": "PERCENT ", "entity": "Offering", "attribute": "discount_pct"}
        ]}),
    );
    let (s, b) = w.call(&w.km, "POST", "", body).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "duplicate_field"),
        "{b}"
    );
}

// ---- delete ------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn delete_purge_calls_prune_all_and_counts() {
    let w = world().await;
    let id = w.create(w.website("Help centre")).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let other = Uuid::new_v4().to_string();
    w.doc(&id);
    w.doc(&id);
    w.doc(&other);
    let (s, b) = w
        .call(&w.km, "DELETE", &format!("/{id}?purge=true"), Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({"purged": true, "documents_deleted": 2, "documents_kept": 0})
    );
    let prunes: Vec<_> = w
        .fake
        .rag_requests()
        .into_iter()
        .filter(|r| r.method == "DELETE" && r.path == format!("/api/sources/{id}/documents"))
        .collect();
    assert_eq!(prunes.len(), 1, "one prune of the whole source");
    assert!(prunes[0].query.contains("all=true"), "{}", prunes[0].query);
    assert_eq!(w.docs_of(&id), 0);
    assert_eq!(w.docs_of(&other), 1, "another source's documents stay");
    let (s, b) = w.read(&format!("/{id}")).await;
    assert_eq!((s, code(&b)), (StatusCode::NOT_FOUND, "source_not_found"));
    let audit = w.audit("source.delete", &id).await;
    assert_eq!(audit[0]["purge"], true);
    assert_eq!(audit[0]["documents_deleted"], 2);
}

#[tokio::test]
async fn delete_without_purge_keeps_documents() {
    let w = world().await;
    let id = w.create(w.api("Promotions")).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    w.doc(&id);
    w.doc(&id);
    w.run(&id, "completed", 2).await;
    let (s, b) = w
        .call(&w.km, "DELETE", &format!("/{id}?purge=false"), Value::Null)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({"purged": false, "documents_deleted": 0, "documents_kept": 2})
    );
    assert!(
        w.fake.rag_requests().iter().all(|r| r.method != "DELETE"),
        "no strata delete without purge"
    );
    assert_eq!(w.docs_of(&id), 2);
    let left = w
        .count("SELECT count(*) FROM source s JOIN source_run r ON r.source_id = s.id WHERE s.tenant_id = $1")
        .await;
    assert_eq!(left, 0, "the source and its runs are gone");
}

#[tokio::test]
async fn delete_while_running_409() {
    let w = world().await;
    let id = w.create(w.api("Promotions")).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    w.doc(&id);
    w.run(&id, "running", 0).await;
    let (s, b) = w
        .call(&w.km, "DELETE", &format!("/{id}?purge=true"), Value::Null)
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "run_in_progress"),
        "{b}"
    );
    assert!(w.fake.rag_requests().is_empty(), "nothing reached strata");
    assert_eq!(w.docs_of(&id), 1);
    let (s, _) = w.read(&format!("/{id}")).await;
    assert_eq!(s, StatusCode::OK, "the source is kept");
}

// ---- test connection ---------------------------------------------------------------------------------------------------

#[tokio::test]
async fn test_source_file_422_not_testable() {
    let w = world().await;
    let draft = json!({"name": "Manuals", "type": "file", "config": {}, "mapping": [],
                       "target_store_id": w.leaf, "schedule": null, "enabled": true});
    let (s, b) = w.call(&w.km, "POST", "/test", draft).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "not_testable"),
        "{b}"
    );
    // a refused address is refused before any call
    let cgnat = merged(
        w.api("x"),
        json!({"config": {"url": "https://100.64.0.1/promos"}}),
    );
    let (s, b) = w.call(&w.km, "POST", "/test", cgnat).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "address_refused"),
        "{b}"
    );
    assert!(w.hits.all().is_empty());
}

#[tokio::test]
async fn test_source_401_without_key_is_ok_false() {
    let w = world().await;
    let (s, b) = w.call(&w.km, "POST", "/test", w.api("Promotions")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        (&b["ok"], &b["status"]),
        (&json!(false), &json!(401)),
        "{b}"
    );
    assert!(
        b["detail"].as_str().unwrap().contains("no key was sent"),
        "{b}"
    );
    assert_eq!(w.hits.all(), [("/promos".to_string(), None)]);
    // the typed key: the records' fields and the first record, the key nowhere in the answer
    let typed = merged(
        w.api("Promotions"),
        json!({"credentials": {"secret": GOOD}}),
    );
    let (s, b) = w.call(&w.km, "POST", "/test", typed).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!((&b["ok"], &b["status"]), (&json!(true), &json!(200)), "{b}");
    assert_eq!(b["fields"], json!(["code", "percent", "title"]));
    assert_eq!(b["sample"]["code"], "AUTUMN15");
    assert!(b["detail"].as_str().unwrap().contains("2 records"), "{b}");
    assert!(!b.to_string().contains(GOOD));
    // a datatable test reads the columns and the first row
    let (s, b) = w
        .call(&w.km, "POST", "/test", w.datatable("Catalogue"))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["ok"], true, "{b}");
    assert_eq!(b["fields"], json!(["sku", "title", "price_eur"]));
    assert_eq!(b["sample"]["sku"], "LL-10204");
}

#[tokio::test]
async fn test_source_uses_stored_key_when_draft_omits() {
    let w = world().await;
    let id = w
        .create(merged(
            w.api("Promotions"),
            json!({"credentials": {"secret": GOOD}}),
        ))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (s, b) = w
        .call(&w.km, "POST", &format!("/{id}/test"), w.api("Promotions"))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!((&b["ok"], &b["status"]), (&json!(true), &json!(200)), "{b}");
    assert!(!b.to_string().contains(GOOD));
    assert_eq!(
        w.hits.all(),
        [("/promos".to_string(), Some(format!("Bearer {GOOD}")))]
    );
    // `null` sends no key
    let none = merged(w.api("Promotions"), json!({"credentials": null}));
    let (s, b) = w.call(&w.km, "POST", &format!("/{id}/test"), none).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], 401, "{b}");
    // the saved key never follows the draft to another host
    let moved = merged(
        w.api("Promotions"),
        json!({"config": {"url": w.host.replace("127.0.0.1", "127.0.0.2") + "/promos",
                          "auth": {"type": "bearer"}}}),
    );
    let (s, b) = w.call(&w.km, "POST", &format!("/{id}/test"), moved).await;
    assert_eq!(
        (s, code(&b)),
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            "secret_required_for_new_host"
        ),
        "{b}"
    );
    assert_eq!(w.hits.all().len(), 2, "the moved test made no call");
}

#[tokio::test]
async fn test_source_stores_nothing() {
    let w = world().await;
    let id = w
        .create(merged(
            w.api("Promotions"),
            json!({"credentials": {"secret": GOOD}}),
        ))
        .await["id"]
        .as_str()
        .unwrap()
        .to_string();
    let before = w.snapshot(&id).await;
    let edited = merged(
        w.api("Renamed"),
        json!({"credentials": {"secret": OTHER}, "enabled": false}),
    );
    let (s, b) = w.call(&w.km, "POST", &format!("/{id}/test"), edited).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], 401, "the typed (wrong) key was used: {b}");
    let (s, _) = w.call(&w.km, "POST", "/test", w.api("Unsaved")).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(w.snapshot(&id).await, before, "a test stores nothing");
    let n = w
        .count("SELECT count(*) FROM source WHERE tenant_id = $1")
        .await;
    assert_eq!(n, 1, "the unsaved draft was not created");
}

// ---- column preview ----------------------------------------------------------------------------------------------------

const BOUNDARY: &str = "t7-preview-boundary";

/// A multipart body: `(field, Some((file name, content type)) for a file, bytes)`.
fn multipart(parts: &[(&str, Option<(&str, &str)>, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, file, bytes) in parts {
        out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match file {
            Some((file_name, ct)) => out.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{file_name}\"\r\nContent-Type: {ct}\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => out.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        out.extend_from_slice(bytes);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

impl W {
    async fn upload(&self, body: Vec<u8>) -> (StatusCode, Value) {
        let req = Request::post(uri("/preview-columns"))
            .header("authorization", format!("Bearer {}", self.km))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(Body::from(body))
            .unwrap();
        send(&self.app, req).await
    }
}

/// A zip of `(part name, contents)`, deflated, built in memory.
fn zip_of(parts: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, body) in parts {
        w.start_file(*name, opts).unwrap();
        w.write_all(body).unwrap();
    }
    w.finish().unwrap().into_inner()
}

/// A workbook of `(sheet name, rows)`, every cell an inline string.
fn workbook(sheets: &[(&str, Vec<Vec<&str>>)]) -> Vec<u8> {
    const NS: &str = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;
    let mut wb = format!("<?xml version=\"1.0\"?><workbook {NS}><sheets>");
    let mut rels = String::from(
        r#"<?xml version="1.0"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#,
    );
    let mut parts: Vec<(String, Vec<u8>)> = Vec::new();
    for (i, (name, rows)) in sheets.iter().enumerate() {
        let n = i + 1;
        wb.push_str(&format!(
            r#"<sheet name="{name}" sheetId="{n}" r:id="rId{n}"/>"#
        ));
        rels.push_str(&format!(
            r#"<Relationship Id="rId{n}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{n}.xml"/>"#
        ));
        let data: String = rows
            .iter()
            .map(|r| {
                let cells: String = r
                    .iter()
                    .map(|c| format!(r#"<c t="inlineStr"><is><t>{c}</t></is></c>"#))
                    .collect();
                format!("<row>{cells}</row>")
            })
            .collect();
        parts.push((
            format!("xl/worksheets/sheet{n}.xml"),
            format!(
                "<?xml version=\"1.0\"?><worksheet {NS}><sheetData>{data}</sheetData></worksheet>"
            )
            .into_bytes(),
        ));
    }
    wb.push_str("</sheets></workbook>");
    rels.push_str("</Relationships>");
    parts.push(("xl/workbook.xml".into(), wb.into_bytes()));
    parts.push(("xl/_rels/workbook.xml.rels".into(), rels.into_bytes()));
    let refs: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_slice()))
        .collect();
    zip_of(&refs)
}

fn catalogue() -> Vec<u8> {
    workbook(&[
        (
            "Products",
            vec![
                vec!["sku", "title", "price_eur"],
                vec!["LL-10204", "Oslo sofa", "1149.00"],
                vec!["LL-10311", "Lune lamp", "189.00"],
            ],
        ),
        (
            "Raw export",
            vec![
                vec!["LL-10204", "sofas", "1149.00"],
                vec!["LL-10311", "lamps", "189.00"],
            ],
        ),
    ])
}

const XLSX_TYPE: &str = "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

#[tokio::test]
async fn preview_csv_url() {
    let w = world().await;
    let (s, b) = w
        .call(&w.km, "POST", "/preview-columns", w.datatable("Catalogue"))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({
            "file": "products.csv", "format": "csv", "row_count": 2,
            "columns": ["sku", "title", "price_eur"],
            "rows": [
                {"sku": "LL-10204", "title": "Oslo sofa", "price_eur": "1149.00"},
                {"sku": "LL-10311", "title": "Lune lamp", "price_eur": "189.00"}
            ],
            "sheets": [], "sheet": null, "header_row": true
        })
    );
    assert_eq!(w.hits.all().len(), 1);
    // the URL is guarded like any source URL
    let (s, b) = w
        .call(
            &w.km,
            "POST",
            "/preview-columns",
            merged(
                w.datatable("x"),
                json!({"config": {"url": "https://10.1.2.3/t.csv", "format": "csv"}}),
            ),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "address_refused"),
        "{b}"
    );
    let (s, b) = w
        .call(
            &w.km,
            "POST",
            "/preview-columns",
            merged(w.datatable("x"), json!({"config": {"format": "csv"}})),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_source"),
        "{b}"
    );
}

#[tokio::test]
async fn preview_xlsx_upload_sheet_and_header_row() {
    let w = world().await;
    let book = catalogue();
    // the first sheet by default, its first row the header
    let (s, b) = w
        .upload(multipart(&[(
            "file",
            Some(("catalogue.xlsx", XLSX_TYPE)),
            book.as_slice(),
        )]))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["columns"], json!(["sku", "title", "price_eur"]));
    assert_eq!(b["sheet"], "Products");
    assert_eq!(b["sheets"], json!(["Products", "Raw export"]));
    // a named sheet without a header row
    let (s, b) = w
        .upload(multipart(&[
            ("file", Some(("catalogue.xlsx", XLSX_TYPE)), book.as_slice()),
            ("sheet", None, &b"Raw export"[..]),
            ("header_row", None, &b"false"[..]),
        ]))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({
            "file": "catalogue.xlsx", "format": "xlsx", "row_count": 2,
            "columns": ["column_1", "column_2", "column_3"],
            "rows": [
                {"column_1": "LL-10204", "column_2": "sofas", "column_3": "1149.00"},
                {"column_1": "LL-10311", "column_2": "lamps", "column_3": "189.00"}
            ],
            "sheets": ["Products", "Raw export"], "sheet": "Raw export", "header_row": false
        })
    );
}

#[tokio::test]
async fn preview_not_datatable_422_not_a_table() {
    let w = world().await;
    let (s, b) = w
        .call(&w.km, "POST", "/preview-columns", w.api("Promotions"))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "not_a_table"),
        "{b}"
    );
    let (s, b) = w
        .upload(multipart(&[
            ("type", None, &b"api"[..]),
            ("file", Some(("t.csv", "text/csv")), &b"a,b\n1,2\n"[..]),
        ]))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "not_a_table"),
        "{b}"
    );
    assert!(w.hits.all().is_empty());
}

#[tokio::test]
async fn preview_sheet_not_found_422() {
    let w = world().await;
    let (s, b) = w
        .upload(multipart(&[
            (
                "file",
                Some(("catalogue.xlsx", XLSX_TYPE)),
                catalogue().as_slice(),
            ),
            ("sheet", None, &b"Orders"[..]),
        ]))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "sheet_not_found"),
        "{b}"
    );
    assert!(b["error"]["message"].as_str().unwrap().contains("Orders"));
    // a file that is not a workbook is unreadable
    let (s, b) = w
        .upload(multipart(&[(
            "file",
            Some(("broken.xlsx", XLSX_TYPE)),
            &b"not a zip"[..],
        )]))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unreadable_table"),
        "{b}"
    );
}

#[tokio::test]
async fn preview_11mb_413() {
    let w = world().await;
    let mut csv = b"a,b\n".to_vec();
    while csv.len() <= 11 * 1024 * 1024 {
        csv.extend_from_slice(b"1234567890,abcdefghij\n");
    }
    let (s, b) = w
        .upload(multipart(&[(
            "file",
            Some(("big.csv", "text/csv")),
            csv.as_slice(),
        )]))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
        "{b}"
    );
    // just under the cap is read
    let mut ok = b"a,b\n".to_vec();
    while ok.len() < 9 * 1024 * 1024 {
        ok.extend_from_slice(b"1234567890,abcdefghij\n");
    }
    let (s, b) = w
        .upload(multipart(&[(
            "file",
            Some(("ok.csv", "text/csv")),
            ok.as_slice(),
        )]))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["columns"], json!(["a", "b"]));
}

#[tokio::test]
async fn preview_pdf_415() {
    let w = world().await;
    let (s, b) = w
        .upload(multipart(&[(
            "file",
            Some(("manual.pdf", "application/pdf")),
            &b"%PDF-1.7"[..],
        )]))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type"),
        "{b}"
    );
}

// ---- roles -------------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn tenant_user_reads_sources_but_cannot_write() {
    let w = world().await;
    let id = w.create(w.website("Help centre")).await["id"]
        .as_str()
        .unwrap()
        .to_string();
    for rest in [
        "".to_string(),
        format!("/{id}"),
        "/mapping-targets".to_string(),
    ] {
        let (s, b) = w.read(&rest).await;
        assert_eq!(s, StatusCode::OK, "{rest}: {b}");
    }
    let (_, list) = w.read("").await;
    assert_eq!(list["items"].as_array().map(Vec::len), Some(1), "{list}");
    let writes = [
        ("POST", String::new(), w.website("Other")),
        ("PUT", format!("/{id}"), w.website("Help centre")),
        ("DELETE", format!("/{id}"), Value::Null),
        ("POST", "/test".to_string(), w.website("Other")),
        ("POST", format!("/{id}/test"), w.website("Help centre")),
        ("POST", "/preview-columns".to_string(), w.datatable("Other")),
    ];
    for (method, rest, body) in writes {
        let (s, b) = w.call(&w.obs, method, &rest, body).await;
        assert_eq!(
            (s, code(&b), b["error"]["missing_role"].as_str()),
            (
                StatusCode::FORBIDDEN,
                "forbidden",
                Some("knowledge_manager")
            ),
            "{method} {rest}: {b}"
        );
    }
    assert!(w.hits.all().is_empty(), "no refused write reached the host");
}

// ---- schedule (D82) ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn schedule_is_stored_and_never_run_while_disabled() {
    let w = world().await;
    let b = w
        .create(merged(
            w.api("Promotions"),
            json!({"schedule": " * * * * * "}),
        ))
        .await;
    let id = b["id"].as_str().unwrap().to_string();
    assert_eq!(b["schedule"], "* * * * *", "stored trimmed and shown");
    // every cron instant of the last two hours has passed for this source
    sqlx::query(
        "UPDATE source SET created_at = now() - interval '2 hours', updated_at = now() - interval '2 hours' \
         WHERE id = $1::uuid",
    )
    .bind(&id)
    .execute(&w.e.owner)
    .await
    .unwrap();
    // the scheduler is off: no ticker runs in this process (the boot guards are `boot_refuses_schedule_enabled_true`
    // and `disabled_scheduler_spawns_no_task`), so the due source stays unrun
    assert!(
        !agent_api::knowledge::schedule::active(),
        "the source scheduler is not active"
    );
    for rest in ["".to_string(), format!("/{id}")] {
        let (s, b) = w.read(&rest).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    let (_, one) = w.read(&format!("/{id}")).await;
    assert_eq!(one["schedule"], "* * * * *");
    assert_eq!(one["last_run"], Value::Null, "no run");
    let runs = w
        .count("SELECT count(*) FROM source_run WHERE tenant_id = $1")
        .await;
    assert_eq!(runs, 0, "nothing ran the schedule");
    assert!(w.hits.all().is_empty(), "the source host was never called");
    assert!(
        w.fake.ingest_requests().is_empty(),
        "nothing was sent to strata_ingest"
    );
}
