//! PK T8 (plan PK-9): super_admin platform curation on the platform plane. Every route is super_admin only (a tenant
//! admin gets 403 and nothing reaches strata); `AGENT_PLATFORM_TENANT_ID` unset → 503 `platform_tenant_unset`; a
//! document without topic keys or with a bad provenance shape is 422 locally; a licence strata refuses is strata's
//! 422 passed through; store → ingest → register → publish → reference writes a `knowledge_pack` row and a new tag
//! whose members hold it, and the template fold (T3) shows the pin; a draft pack is 409 `pack_unpublished`;
//! unreference tombstones and tags; URL jobs go to strata_ingest with per-URL provenance; a file upload (T6b) sends
//! `topic_keys` + `provenance` before the file parts and a licence strata refuses reads `licence_refused`.
//!
//! Curation never reads the platform owner's agent `tenants` row (PK Q2: production had none), so it is not asserted
//! absent: boot bootstraps the platform tenant when `AGENT_PLATFORM_TENANT_ID` is set. Real Postgres 18 (the pk
//! binary's container, a fresh database per test) and the strata fake.
use crate::common;

use agent_api::knowledge::packs::template as packs;
use agent_api::platform::templates;
use agent_model::compile::Selection;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::strata_fake::{self, StrataFake, lock};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send, super_admin};
use serde_json::{Value, json};
use uuid::Uuid;

const BASE: &str = "/api/platform/knowledge";
const CCA: &str = "job/customer_care_agent";

struct W {
    e: TestEnv,
    fake: StrataFake,
    owner: Uuid,
    sa: String,
    /// a tenant admin (every tenant role, not super_admin)
    admin: String,
}

async fn world_with(platform_id: bool) -> W {
    let fake = strata_fake::start().await;
    let owner = Uuid::new_v4();
    let tenant = Uuid::new_v4();
    lock(&fake.rag).platform_owner = Some(owner);
    let e = common::env_with(|v| {
        fake.env(&[owner, tenant], v);
        if platform_id {
            v.insert("AGENT_PLATFORM_TENANT_ID".into(), owner.to_string());
        }
    })
    .await;
    insert_tenant(&e.owner, tenant, &format!("T8 {tenant}")).await;
    let admin_sub = format!("u-t8-admin-{}", tenant.simple());
    insert_user(&e.owner, &admin_sub, tenant, "tenant_admin").await;
    W {
        e,
        fake,
        owner,
        sa: super_admin(),
        admin: person(&admin_sub, &[]),
    }
}

async fn world() -> W {
    world_with(true).await
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

fn prov(license: &str) -> Value {
    json!({"source_url": "https://www.gov.uk/guidance/returns", "license": license,
           "retrieved_at": "2026-10-21T08:00:00Z"})
}

impl W {
    async fn call(&self, tok: &str, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        let uri = format!("{BASE}{path}");
        if method == "GET" {
            send(&self.e.app, get(&uri, Some(tok))).await
        } else {
            send(&self.e.app, json(method, &uri, Some(tok), body)).await
        }
    }

    async fn sa(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        self.call(&self.sa, method, path, body).await
    }

    /// A router root and one leaf under it, created through the routes: (root, leaf).
    async fn tree(&self, name: &str) -> (String, String) {
        let (s, root) = self
            .sa("POST", "/stores", json!({"name": name, "kind": "router"}))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{root}");
        let root = root["id"].as_str().unwrap().to_string();
        let (s, leaf) = self
            .sa(
                "POST",
                "/stores",
                json!({"name": "docs", "parent_id": root, "description": "leaf"}),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{leaf}");
        assert_eq!(leaf["kind"], "leaf", "a store is a leaf by default: {leaf}");
        (root, leaf["id"].as_str().unwrap().to_string())
    }

    async fn platform_audits(&self, action: &str) -> Vec<Value> {
        sqlx::query_scalar("SELECT details FROM platform_audit WHERE action = $1 ORDER BY id")
            .bind(action)
            .fetch_all(&self.e.owner)
            .await
            .unwrap()
    }

    /// `(op, fields)` of the current `knowledge_pack` row `key` on `layer`.
    async fn row(&self, layer: &str, key: &str) -> Option<(String, Value)> {
        sqlx::query_as(
            "SELECT r.op, r.fields FROM layer_row r JOIN layer l ON l.id = r.layer_id \
              WHERE l.key = $1 AND r.kind = 'knowledge_pack' AND r.key = $2 AND r.is_current",
        )
        .bind(layer)
        .bind(key)
        .fetch_optional(&self.e.owner)
        .await
        .unwrap()
    }

    /// The (kind, key, op) members of `family@vN`.
    async fn members(&self, family: &str, version: i64) -> Vec<(String, String, String)> {
        sqlx::query_as(
            "SELECT r.kind, r.key, r.op FROM layer_tag_row m JOIN layer_row r ON r.id = m.row_id \
              WHERE m.family = $1 AND m.version = $2 AND r.kind = 'knowledge_pack' ORDER BY r.key",
        )
        .bind(family)
        .bind(i32::try_from(version).unwrap())
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
    }
}

/// The seed template's selection (the head's `template_selection`).
fn sel() -> Selection {
    serde_json::from_value(json!({
        "profile": "retail_ecommerce",
        "industry": "retail",
        "departments": ["industry_dept/retail/customer_service"],
    }))
    .unwrap()
}

#[tokio::test]
async fn every_route_is_super_admin_only() {
    let w = world().await;
    let id = Uuid::new_v4();
    let routes: [(&str, String, Value); 16] = [
        ("GET", "/stores".into(), json!(null)),
        ("POST", "/stores".into(), json!({"name": "x"})),
        ("DELETE", format!("/stores/{id}"), json!(null)),
        ("GET", format!("/stores/{id}/documents"), json!(null)),
        (
            "POST",
            format!("/stores/{id}/documents"),
            json!({"text": "t", "topic_keys": ["a"], "provenance": prov("CC0-1.0")}),
        ),
        (
            "POST",
            format!("/stores/{id}/ingest-jobs"),
            json!({"urls": ["https://example.org/a"], "topic_keys": ["a"], "license": "CC0-1.0"}),
        ),
        ("DELETE", format!("/documents/{id}"), json!(null)),
        ("POST", format!("/stores/{id}/upload"), json!(null)),
        ("GET", format!("/ingest-jobs/{id}"), json!(null)),
        ("GET", "/packs".into(), json!(null)),
        (
            "POST",
            "/packs".into(),
            json!({"key": "job/x", "version": 1, "store_id": id}),
        ),
        ("POST", "/packs/job%2Fx/1/publish".into(), json!(null)),
        ("POST", "/packs/job%2Fx/1/retire".into(), json!(null)),
        ("GET", "/packs/job%2Fx/1/stores".into(), json!(null)),
        (
            "POST",
            "/packs/job%2Fx/1/references".into(),
            json!({"layer": CCA, "stage": "*", "kind": "*"}),
        ),
        (
            "DELETE",
            format!("/packs/job%2Fx/references?layer={CCA}&stage=*&kind=*"),
            json!(null),
        ),
    ];
    for (m, p, b) in &routes {
        let (s, body) = w.call(&w.admin, m, p, b.clone()).await;
        assert_eq!(
            (s, code(&body)),
            (StatusCode::FORBIDDEN, "super_admin_required"),
            "{m} {p}: {body}"
        );
    }
    assert!(
        w.fake.rag_requests().is_empty() && w.fake.ingest_requests().is_empty(),
        "nothing reached strata"
    );
    // and the super_admin is let through (the gate is the only difference)
    let (s, b) = w.sa("GET", "/packs", json!(null)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

#[tokio::test]
async fn unset_platform_tenant_is_503() {
    let w = world_with(false).await;
    for (m, p, b) in [
        ("GET", "/stores".to_string(), json!(null)),
        ("GET", "/packs".to_string(), json!(null)),
        (
            "POST",
            "/packs/job%2Fx/1/references".to_string(),
            json!({"layer": CCA, "stage": "*", "kind": "*"}),
        ),
        (
            "DELETE",
            format!("/packs/job%2Fx/references?layer={CCA}&stage=*&kind=*"),
            json!(null),
        ),
    ] {
        let (s, body) = w.sa(m, &p, b).await;
        assert_eq!(
            (s, code(&body)),
            (StatusCode::SERVICE_UNAVAILABLE, "platform_tenant_unset"),
            "{m} {p}: {body}"
        );
    }
    assert!(w.fake.rag_requests().is_empty());
}

#[tokio::test]
async fn documents_need_topic_keys_and_provenance_shape_and_strata_owns_the_licence_list() {
    let w = world().await;
    let (_, leaf) = w.tree("pk-t8-docs").await;
    let path = format!("/stores/{leaf}/documents");

    // no topic keys: 422 locally, strata never called
    let before = w.fake.rag_requests().len();
    let (s, b) = w
        .sa(
            "POST",
            &path,
            json!({"text": "Returns within 30 days.", "provenance": prov("CC-BY-4.0")}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_topic_keys"),
        "{b}"
    );
    // a bad provenance shape: 422 locally
    for bad in [
        json!({"source_url": "http://example.org/a", "license": "CC-BY-4.0", "retrieved_at": "2026-10-21T08:00:00Z"}),
        json!({"source_url": "https://postgres.internal/a", "license": "CC-BY-4.0", "retrieved_at": "2026-10-21T08:00:00Z"}),
        json!({"source_url": "https://example.org/a", "license": "CC-BY-4.0", "retrieved_at": "21 Oct 2026"}),
    ] {
        let (s, b) = w
            .sa(
                "POST",
                &path,
                json!({"text": "x", "topic_keys": ["returns.policy"], "provenance": bad}),
            )
            .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_provenance"),
            "{b}"
        );
    }
    assert_eq!(
        w.fake.rag_requests().len(),
        before,
        "no local refusal reached strata"
    );

    // a non-redistributable licence passes the agent's shape check and strata refuses it: 422 with strata's message
    let (s, b) = w
        .sa(
            "POST",
            &path,
            json!({"text": "x", "topic_keys": ["returns.policy"], "provenance": prov("CC-BY-NC-4.0")}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_input"),
        "{b}"
    );
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap_or("")
            .contains("not allowed"),
        "strata's message passes through: {b}"
    );

    // the allowed licence: 201, topic keys + provenance sent with the curator pair
    let (s, b) = w
        .sa(
            "POST",
            &path,
            json!({"title": "Returns", "text": "Returns within 30 days.", "source_id": "gov-returns",
                   "topic_keys": ["returns.policy", " returns.policy "], "provenance": prov("CC-BY-4.0")}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let doc_id = b["id"].as_str().unwrap().to_string();
    let sent = w
        .fake
        .rag_requests()
        .into_iter()
        .rfind(|r| r.method == "POST" && r.path == format!("/api/stores/{leaf}/documents"))
        .unwrap();
    assert_eq!(sent.token, Some(strata_fake::rag_token(w.owner)));
    assert_eq!(sent.tenant_header, Some(w.owner.to_string()));
    assert_eq!(sent.body["topic_keys"], json!(["returns.policy"]));
    assert_eq!(sent.body["provenance"], prov("CC-BY-4.0"));
    assert_eq!(sent.body["source_id"], "gov-returns");

    // the audit row holds ids and keys, never the text
    let audits = w
        .platform_audits("platform.knowledge_document.ingest")
        .await;
    assert_eq!(audits.len(), 1, "{audits:?}");
    assert_eq!(audits[0]["document_id"], doc_id.as_str());
    assert!(!audits[0].to_string().contains("30 days"), "{}", audits[0]);

    // listed, then deleted
    let (s, b) = w.sa("GET", &path, json!(null)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"].as_array().map(Vec::len), Some(1), "{b}");
    let (s, b) = w
        .sa("DELETE", &format!("/documents/{doc_id}"), json!(null))
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert_eq!(
        w.platform_audits("platform.knowledge_document.delete")
            .await
            .len(),
        1
    );
}

#[tokio::test]
async fn url_jobs_go_to_strata_ingest_with_per_url_provenance() {
    let w = world().await;
    let (_, leaf) = w.tree("pk-t8-urls").await;
    let path = format!("/stores/{leaf}/ingest-jobs");
    let (s, b) = w
        .sa(
            "POST",
            &path,
            json!({"urls": ["https://10.1.2.3/a"], "topic_keys": ["a"], "license": "CC0-1.0"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_urls"),
        "{b}"
    );
    let (s, b) = w
        .sa(
            "POST",
            &path,
            json!({"urls": ["https://example.org/a"], "license": "CC0-1.0"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_topic_keys"),
        "{b}"
    );
    assert!(w.fake.ingest_requests().is_empty());

    let (s, b) = w
        .sa(
            "POST",
            &path,
            json!({"urls": ["https://example.org/a", "https://example.org/b?page=2"],
                   "topic_keys": ["hr/leave"], "license": "OGL-UK-3.0", "retrieved_at": "2026-10-21T08:00:00Z"}),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert_eq!(b["jobs"].as_array().map(Vec::len), Some(2), "{b}");
    let sent = w.fake.ingest_requests();
    assert_eq!(sent.len(), 1, "{sent:#?}");
    assert_eq!(sent[0].token, Some(strata_fake::ingest_token(w.owner)));
    assert_eq!(sent[0].body["store_id"], leaf.as_str());
    assert_eq!(sent[0].body["topic_keys"], json!(["hr/leave"]));
    assert_eq!(
        sent[0].body["urls"][1],
        json!({"url": "https://example.org/b?page=2", "provenance": {
            "source_url": "https://example.org/b?page=2", "license": "OGL-UK-3.0",
            "retrieved_at": "2026-10-21T08:00:00Z"}})
    );
}

#[tokio::test]
async fn register_publish_reference_tags_the_family_and_the_fold_shows_the_pin() {
    let w = world().await;
    let (root, leaf) = w.tree("pk-t8-pack").await;
    let (s, b) = w
        .sa(
            "POST",
            &format!("/stores/{leaf}/documents"),
            json!({"text": "Returns within 30 days.", "topic_keys": ["returns.policy"], "provenance": prov("CC-BY-4.0")}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");

    let (s, b) = w
        .sa(
            "POST",
            "/packs",
            json!({"key": "job/pk_t8", "version": 1, "store_id": root}),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert!(b["published_at"].is_null(), "a draft: {b}");

    // the curator reads the pack's stores (a draft too): root first, then the leaf; no audit row for a read
    let (s, b) = w
        .sa("GET", "/packs/job%2Fpk_t8/1/stores", json!(null))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["pack"], "job/pk_t8@1", "{b}");
    assert_eq!(b["root_id"], root.as_str(), "{b}");
    let ids: Vec<&str> = b["stores"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [root.as_str(), leaf.as_str()], "{b}");
    assert_eq!(b["stores"][1]["kind"], "leaf", "{b}");
    assert_eq!(b["stores"][1]["path"], json!(["pk-t8-pack", "docs"]), "{b}");
    assert_eq!(
        w.fake.rag_requests().last().map(|r| r.path.clone()),
        Some("/api/platform/packs/job%2Fpk_t8/1/stores".to_string())
    );
    let (s, b) = w
        .sa("GET", "/packs/job%2Fpk_t8/9/stores", json!(null))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "pack_not_found"),
        "{b}"
    );
    let (s, b) = w.sa("GET", "/packs/Bad%20Key/1/stores", json!(null)).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_pack"),
        "{b}"
    );

    // retiring a draft: the pack's own 422, never the connector-URL message (T10)
    let (s, b) = w
        .sa("POST", "/packs/job%2Fpk_t8/1/retire", json!(null))
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "pack_not_published"),
        "{b}"
    );
    let m = b["error"]["message"].as_str().unwrap_or("");
    assert!(
        m.contains("only a published pack can be retired") && !m.contains("https"),
        "{b}"
    );

    // a draft cannot be referenced, and nothing is written
    let refs = "/packs/job%2Fpk_t8/1/references";
    let body = json!({"layer": CCA, "stage": "*", "kind": "*"});
    let (s, b) = w.sa("POST", refs, body.clone()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "pack_unpublished"),
        "{b}"
    );
    assert!(w.row(CCA, "*|*|job/pk_t8").await.is_none());
    // an unknown pack is 404
    let (s, b) = w
        .sa("POST", "/packs/job%2Fpk_t8/9/references", body.clone())
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "pack_not_found"),
        "{b}"
    );

    let (s, b) = w
        .sa("POST", "/packs/job%2Fpk_t8/1/publish", json!(null))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["published_at"].is_string(), "{b}");
    // published: the subtree is frozen (strata's 409 passes through)
    let (s, b) = w
        .sa(
            "POST",
            &format!("/stores/{leaf}/documents"),
            json!({"text": "more", "topic_keys": ["returns.policy"], "provenance": prov("CC-BY-4.0")}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "pack_published"),
        "{b}"
    );

    // a bad row key (whitespace in the kind) and an unknown layer are refused before anything is written
    let (s, b) = w
        .sa(
            "POST",
            refs,
            json!({"layer": CCA, "stage": "*", "kind": "fa q"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_key"),
        "{b}"
    );
    let (s, b) = w
        .sa(
            "POST",
            refs,
            json!({"layer": "job/no_such_job", "stage": "*", "kind": "*"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "layer_not_found"),
        "{b}"
    );

    // reference: a row + a new tag of the family whose members hold it
    let (s, b) = w.sa("POST", refs, body.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["family"], CCA, "{b}");
    let v = b["version"].as_i64().unwrap();
    assert_eq!(b["tag"], format!("{CCA}@v{v}"));
    assert_eq!(b["row"]["op"], "add", "{b}");
    assert_eq!(
        w.row(CCA, "*|*|job/pk_t8").await,
        Some(("add".to_string(), json!({"version": 1})))
    );
    assert_eq!(
        w.members(CCA, v).await,
        [(
            "knowledge_pack".to_string(),
            "*|*|job/pk_t8".to_string(),
            "add".to_string()
        )]
    );
    assert_eq!(
        w.platform_audits("platform.knowledge_pack.reference")
            .await
            .len(),
        1
    );
    // the same reference again: 409, no new tag
    let (s, b) = w.sa("POST", refs, body.clone()).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::CONFLICT, "already_referenced"),
        "{b}"
    );

    // T3's fold over the new tag gives every kind the pin
    let mut c = w.e.state.db.acquire().await.unwrap();
    let pins = templates::resolve_template(&mut c, CCA, i32::try_from(v).unwrap())
        .await
        .unwrap()
        .pins;
    let folded = packs::fold(&mut c, &pins, &sel()).await.unwrap();
    assert!(
        folded
            .iter()
            .any(|r| r.key() == "*|*|job/pk_t8" && r.version == 1),
        "{folded:?}"
    );
    assert_eq!(
        packs::pins_for(&folded, "support", "faq", &[]),
        ["job/pk_t8@1"]
    );
    drop(c);

    // unreference: a tombstone and another tag
    let (s, b) = w
        .sa(
            "DELETE",
            &format!("/packs/job%2Fpk_t8/references?layer={CCA}&stage=*&kind=*"),
            json!(null),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let v2 = b["version"].as_i64().unwrap();
    assert!(v2 > v, "a new tag: {b}");
    assert_eq!(
        w.row(CCA, "*|*|job/pk_t8").await.map(|r| r.0),
        Some("tombstone".into())
    );
    assert_eq!(
        w.members(CCA, v2).await,
        [(
            "knowledge_pack".to_string(),
            "*|*|job/pk_t8".to_string(),
            "tombstone".to_string()
        )]
    );
    let (s, b) = w
        .sa(
            "DELETE",
            &format!("/packs/job%2Fpk_t8/references?layer={CCA}&stage=*&kind=*"),
            json!(null),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "reference_not_found"),
        "{b}"
    );
    assert_eq!(
        w.platform_audits("platform.knowledge_pack.unreference")
            .await
            .len(),
        1
    );

    // retire: a retired pack cannot be referenced again
    let (s, b) = w
        .sa("POST", "/packs/job%2Fpk_t8/1/retire", json!(null))
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = w.sa("POST", refs, body).await;
    assert_eq!((s, code(&b)), (StatusCode::CONFLICT, "pack_retired"), "{b}");
    let (s, b) = w.sa("GET", "/packs", json!(null)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(
        b[0]["retired_at"].is_string(),
        "the owner listing keeps retired packs: {b}"
    );
    for a in ["register", "publish", "retire"] {
        assert_eq!(
            w.platform_audits(&format!("platform.knowledge_pack.{a}"))
                .await
                .len(),
            1,
            "{a}"
        );
    }
}

// ---------------------------------------------------------------------------------------------------------------
// File upload (T6b: strata_ingest #15 multipart)

const BOUNDARY: &str = "pk-t6b-curation-7c1e";

/// One form field: (name, file name of a file field, bytes).
type Field<'a> = (&'a str, Option<&'a str>, Vec<u8>);

fn form(fields: &[Field<'_>]) -> Vec<u8> {
    let mut b = Vec::new();
    for (name, file, data) in fields {
        b.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        let head = match file {
            Some(f) => format!(
                "Content-Disposition: form-data; name=\"{name}\"; filename=\"{f}\"\r\n\
                 Content-Type: application/pdf\r\n\r\n"
            ),
            None => format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n"),
        };
        b.extend_from_slice(head.as_bytes());
        b.extend_from_slice(data);
        b.extend_from_slice(b"\r\n");
    }
    b.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    b
}

fn text<'a>(name: &'a str, v: &str) -> Field<'a> {
    (name, None, v.as_bytes().to_vec())
}

fn pdf(name: &str) -> Field<'_> {
    ("file", Some(name), b"%PDF-1.7 platform".to_vec())
}

impl W {
    async fn upload(&self, leaf: &str, fields: &[Field<'_>]) -> (StatusCode, Value) {
        let req = Request::post(format!("{BASE}/stores/{leaf}/upload"))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .header("authorization", format!("Bearer {}", self.sa))
            .body(Body::from(form(fields)))
            .unwrap();
        send(&self.e.app, req).await
    }
}

/// Every field is checked before strata (422 / 415, nothing sent); the happy upload sends `topic_keys` and
/// `provenance` (retrieved_at defaulted) BEFORE the file part whatever the curator's order, on the owner's ingest
/// token, and is audited without content; a licence strata refuses fails the job `provenance_rejected`, which the
/// job read shows as `licence_refused`.
#[tokio::test]
async fn file_upload_sends_topic_keys_and_provenance_first_and_maps_a_refused_licence() {
    let w = world().await;
    lock(&w.fake.ingest).platform_owner = Some(w.owner);
    let (_, leaf) = w.tree("pk-t6b-files").await;
    let good_prov =
        r#"{"source_url": "https://www.gov.uk/guidance/leave", "license": "OGL-UK-3.0"}"#;

    for (fields, want) in [
        (
            vec![text("provenance", good_prov), pdf("a.pdf")],
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_topic_keys"),
        ),
        (
            vec![
                text("topic_keys", r#"["Hr/Leave"]"#),
                text("provenance", good_prov),
                pdf("a.pdf"),
            ],
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_topic_keys"),
        ),
        (
            vec![text("topic_keys", r#"["hr/leave"]"#), pdf("a.pdf")],
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_provenance"),
        ),
        (
            vec![
                text("topic_keys", r#"["hr/leave"]"#),
                text(
                    "provenance",
                    r#"{"source_url": "http://intranet.local/x", "license": "OGL-UK-3.0"}"#,
                ),
                pdf("a.pdf"),
            ],
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_provenance"),
        ),
        (
            vec![
                text("topic_keys", r#"["hr/leave"]"#),
                text("provenance", good_prov),
            ],
            (StatusCode::UNPROCESSABLE_ENTITY, "no_file"),
        ),
        (
            vec![
                text("topic_keys", r#"["hr/leave"]"#),
                text("provenance", good_prov),
                ("file", Some("setup.exe"), b"MZ".to_vec()),
            ],
            (StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_type"),
        ),
    ] {
        let (s, b) = w.upload(&leaf, &fields).await;
        assert_eq!((s, code(&b)), want, "{b}");
    }
    assert!(
        w.fake.ingest_requests().is_empty(),
        "refused before strata_ingest"
    );

    // the file first, the fields after: strata still gets the fields first
    let (s, b) = w
        .upload(
            &leaf,
            &[
                pdf("leave.pdf"),
                text("topic_keys", r#"[" hr/leave ", "hr/holiday", "hr/leave"]"#),
                text("provenance", good_prov),
            ],
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let job_id = b["jobs"][0]["job_id"].as_str().unwrap().to_string();
    let sent = w.fake.ingest_requests();
    assert_eq!(sent.len(), 1, "{sent:#?}");
    assert_eq!(sent[0].token, Some(strata_fake::ingest_token(w.owner)));
    let parts = lock(&w.fake.ingest).forms.last().unwrap().1.clone();
    let names: Vec<&str> = parts.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["store_id", "topic_keys", "provenance", "file"]);
    assert_eq!(parts[0].value, leaf);
    let keys: Value = serde_json::from_str(&parts[1].value).unwrap();
    assert_eq!(keys, json!(["hr/holiday", "hr/leave"]));
    let prov: Value = serde_json::from_str(&parts[2].value).unwrap();
    assert_eq!(prov["license"], "OGL-UK-3.0");
    assert_eq!(prov["source_url"], "https://www.gov.uk/guidance/leave");
    assert!(
        chrono::DateTime::parse_from_rfc3339(prov["retrieved_at"].as_str().unwrap()).is_ok(),
        "retrieved_at defaults to now: {prov}"
    );
    assert_eq!(parts[3].file_name.as_deref(), Some("leave.pdf"));
    let (s, j) = w
        .sa("GET", &format!("/ingest-jobs/{job_id}"), json!(null))
        .await;
    assert_eq!(s, StatusCode::OK, "{j}");
    assert_eq!(j["curator_status"], "queued", "{j}");
    assert!(j["curator_message"].is_null(), "{j}");
    let audits = w
        .platform_audits("platform.knowledge_document.upload")
        .await;
    assert_eq!(audits.len(), 1);
    assert_eq!(audits[0]["files"], json!(["leave.pdf"]));
    assert_eq!(audits[0]["topic_keys"], json!(["hr/holiday", "hr/leave"]));
    assert!(
        !audits[0].to_string().contains("PDF-1.7"),
        "no content in the audit"
    );

    // a licence strata refuses: the job fails provenance_rejected, read as licence_refused
    let (s, b) = w
        .upload(
            &leaf,
            &[
                text("topic_keys", r#"["hr/leave"]"#),
                text(
                    "provenance",
                    r#"{"source_url": "https://example.org/x", "license": "CC-BY-NC-4.0",
                        "retrieved_at": "2026-10-21T08:00:00Z"}"#,
                ),
                pdf("nc.pdf"),
            ],
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let job_id = b["jobs"][0]["job_id"].as_str().unwrap().to_string();
    let (s, j) = w
        .sa("GET", &format!("/ingest-jobs/{job_id}"), json!(null))
        .await;
    assert_eq!(s, StatusCode::OK, "{j}");
    assert_eq!(j["status"], "failed", "{j}");
    assert_eq!(j["error"]["code"], "provenance_rejected", "{j}");
    assert_eq!(j["curator_status"], "licence_refused", "{j}");
    let m = j["curator_message"].as_str().unwrap_or_default();
    assert!(
        m.contains("CC-BY-NC-4.0") && m.contains("STRATA_PLATFORM_LICENSES"),
        "{m}"
    );
}
