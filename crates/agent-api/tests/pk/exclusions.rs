//! PK T7: tenant-wide platform exclusions (plan PK-10). `GET/PUT/DELETE /api/knowledge/platform-exclusions` proxy
//! strata_rag `/api/platform/exclusions` with the tenant's own pair (path, query encoding, body, `X-Actor-Id`), writes
//! need `knowledge_manager`, a bad prefix is 422 `invalid_key_prefix` before strata is called, the writes are audited
//! with `{key_prefix}` only, and the listing cache's `platform_pack.excluded_by` follows the exclusions segment-wise.
//! Real Postgres 18 (one container for the binary, a fresh database per test) and the in-process strata fake.
use crate::common;

use crate::common::strata_fake::{self, Recorded, StrataFake, lock};
use crate::common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

const URI: &str = "/api/knowledge/platform-exclusions";

struct W {
    e: TestEnv,
    fake: StrataFake,
    tenant: Uuid,
    /// knowledge_manager (uuid sub, so strata gets `X-Actor-Id`)
    km: String,
    km_sub: String,
    /// a member without a role
    obs: String,
}

async fn world() -> W {
    let fake = strata_fake::start().await;
    let tenant = Uuid::new_v4();
    let e = common::env_with(|v| fake.env(&[tenant], v)).await;
    insert_tenant(&e.owner, tenant, &format!("T7 {tenant}")).await;
    let km_sub = Uuid::new_v4().to_string();
    insert_user(&e.owner, &km_sub, tenant, "knowledge_manager").await;
    let km = person(&km_sub, &[]);
    let obs_sub = format!("u-obs-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &obs_sub, tenant, "tenant_user").await;
    let obs = person(&obs_sub, &[]);
    W {
        e,
        fake,
        tenant,
        km,
        km_sub,
        obs,
    }
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

impl W {
    async fn call(&self, tok: &str, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        send(&self.e.app, json(method, uri, Some(tok), body)).await
    }

    async fn read(&self, uri: &str) -> (StatusCode, Value) {
        send(&self.e.app, get(uri, Some(&self.obs))).await
    }

    /// The fake's exclusion requests (`/api/platform/exclusions`) with `method`.
    fn rag(&self, method: &str) -> Vec<Recorded> {
        self.fake
            .rag_requests()
            .into_iter()
            .filter(|r| r.method == method && r.path == "/api/platform/exclusions")
            .collect()
    }

    /// (key_prefix detail, resource_id) of every audit row with `action`, oldest first.
    async fn audits(&self, action: &str) -> Vec<(Value, String)> {
        sqlx::query_as(
            "SELECT details, resource_id FROM audit_event WHERE tenant_id = $1 AND action = $2 ORDER BY id",
        )
        .bind(self.tenant)
        .bind(action)
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
    }

    /// `(pack_key, excluded_by)` of the tenant's listing cache, by key.
    async fn cache(&self) -> Vec<(String, Option<String>)> {
        sqlx::query_as(
            "SELECT pack_key, excluded_by FROM platform_pack WHERE tenant_id = $1 ORDER BY pack_key, version",
        )
        .bind(self.tenant)
        .fetch_all(&self.e.owner)
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn put_get_delete_proxy_the_tenant_pair_and_audit_the_prefix_only() {
    let w = world().await;
    // empty to start
    let (s, b) = w.read(URI).await;
    assert_eq!((s, b.clone()), (StatusCode::OK, json!([])), "{b}");

    let (s, b) = w
        .call(
            &w.km,
            "PUT",
            URI,
            json!({"key_prefix": " industry/retail ", "reason": "  we sell no retail  "}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["key_prefix"], "industry/retail");
    assert_eq!(b["reason"], "we sell no retail");
    assert!(
        b["created_at"].as_str().is_some_and(|c| !c.is_empty()),
        "{b}"
    );

    let puts = w.rag("PUT");
    assert_eq!(puts.len(), 1, "{puts:#?}");
    assert_eq!(
        puts[0].body,
        json!({"key_prefix": "industry/retail", "reason": "we sell no retail"})
    );
    assert_eq!(puts[0].token, Some(strata_fake::rag_token(w.tenant)));
    assert_eq!(puts[0].tenant_header, Some(w.tenant.to_string()));
    assert_eq!(puts[0].actor.as_deref(), Some(w.km_sub.as_str()));

    // the fake holds it for this tenant only
    assert_eq!(lock(&w.fake.rag).exclusions.len(), 1);
    assert_eq!(lock(&w.fake.rag).exclusions[0].0, w.tenant);

    // a member reads it back
    let (s, b) = w.read(URI).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b.as_array().map(Vec::len), Some(1), "{b}");
    assert_eq!(b[0]["key_prefix"], "industry/retail");
    assert_eq!(b[0]["reason"], "we sell no retail");

    // audit: the prefix, never the reason
    let set = w.audits("knowledge.platform_exclusion.set").await;
    assert_eq!(set.len(), 1, "{set:?}");
    assert_eq!(set[0].0["key_prefix"], "industry/retail", "{:?}", set[0]);
    assert!(set[0].0.get("reason").is_none(), "{:?}", set[0]);
    assert_eq!(set[0].1, "industry/retail");
    assert!(!set[0].0.to_string().contains("we sell"));

    // DELETE: the prefix is percent-encoded in the query
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("{URI}?key_prefix=industry%2Fretail"),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    let dels = w.rag("DELETE");
    assert_eq!(dels.len(), 1, "{dels:#?}");
    assert_eq!(dels[0].query, "key_prefix=industry%2Fretail");
    assert!(lock(&w.fake.rag).exclusions.is_empty());
    let rm = w.audits("knowledge.platform_exclusion.remove").await;
    assert_eq!(rm.len(), 1, "{rm:?}");
    assert_eq!(rm[0].0["key_prefix"], "industry/retail", "{:?}", rm[0]);
    assert_eq!(rm[0].1, "industry/retail");

    // DELETE of an unknown prefix: strata's 404 passes through, nothing audited
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("{URI}?key_prefix=base"),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(
        w.audits("knowledge.platform_exclusion.remove").await.len(),
        1
    );
}

#[tokio::test]
async fn writes_need_knowledge_manager() {
    let w = world().await;
    let (s, b) = w
        .call(
            &w.obs,
            "PUT",
            URI,
            json!({"key_prefix": "base", "reason": "no"}),
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
    assert_eq!(b["error"]["missing_role"], "knowledge_manager", "{b}");
    let (s, b) = w
        .call(
            &w.obs,
            "DELETE",
            &format!("{URI}?key_prefix=base"),
            Value::Null,
        )
        .await;
    assert_eq!((s, code(&b)), (StatusCode::FORBIDDEN, "forbidden"), "{b}");
    assert!(w.rag("PUT").is_empty() && w.rag("DELETE").is_empty());
    assert!(
        w.audits("knowledge.platform_exclusion.set")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn bad_prefix_and_reason_are_refused_before_strata() {
    let w = world().await;
    for bad in [
        "Industry",
        "industry/",
        "a/b/c/d/e/f/g",
        "industry retail",
        "x@1",
        "",
    ] {
        let (s, b) = w
            .call(&w.km, "PUT", URI, json!({"key_prefix": bad, "reason": "r"}))
            .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, "invalid_key_prefix"),
            "{bad:?}: {b}"
        );
    }
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("{URI}?key_prefix=Industry%2F"),
            Value::Null,
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_key_prefix"),
        "{b}"
    );
    let long = "x".repeat(501);
    for (reason, want) in [
        ("   ", "invalid_reason"),
        (long.as_str(), "invalid_reason"),
        ("jane.doe@example.com", "secret_rejected"),
    ] {
        let (s, b) = w
            .call(
                &w.km,
                "PUT",
                URI,
                json!({"key_prefix": "base", "reason": reason}),
            )
            .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNPROCESSABLE_ENTITY, want),
            "{b}"
        );
    }
    // 500 characters are fine
    let (s, b) = w
        .call(
            &w.km,
            "PUT",
            URI,
            json!({"key_prefix": "base", "reason": "y".repeat(500)}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // only that last one reached strata
    assert_eq!(w.rag("PUT").len(), 1);
    assert!(w.rag("DELETE").is_empty());
}

#[tokio::test]
async fn platform_off_lists_empty_and_refuses_writes() {
    let w = world().await;
    lock(&w.fake.rag).platform_off = true;
    let (s, b) = w.read(URI).await;
    assert_eq!((s, b.clone()), (StatusCode::OK, json!([])), "{b}");
    let (s, b) = w
        .call(
            &w.km,
            "PUT",
            URI,
            json!({"key_prefix": "base", "reason": "no"}),
        )
        .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::SERVICE_UNAVAILABLE, "platform_knowledge_off"),
        "{b}"
    );
    assert!(
        w.audits("knowledge.platform_exclusion.set")
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn excluded_by_marks_matching_cached_packs_segment_wise() {
    let w = world().await;
    {
        let mut g = lock(&w.fake.rag);
        g.add_pack("base", 1);
        g.add_pack("industry/retail", 1);
        g.add_pack("industry/retail", 2);
        g.add_pack("industry/retail/hr", 1);
        g.add_pack("industry/retailx", 1);
    }
    // the listing seeds the cache
    let (s, b) = w.read("/api/knowledge/platform-packs").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b.as_array().map(Vec::len), Some(5), "{b}");
    assert!(w.cache().await.iter().all(|(_, x)| x.is_none()));

    let (s, b) = w
        .call(
            &w.km,
            "PUT",
            URI,
            json!({"key_prefix": "industry/retail", "reason": "not retail"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let p = Some("industry/retail".to_string());
    assert_eq!(
        w.cache().await,
        vec![
            ("base".to_string(), None),
            ("industry/retail".to_string(), p.clone()),
            ("industry/retail".to_string(), p.clone()),
            ("industry/retail/hr".to_string(), p.clone()),
            ("industry/retailx".to_string(), None),
        ]
    );
    // the refresh also took the excluded packs out of the listing
    let listed: Vec<(String, bool)> = sqlx::query_as(
        "SELECT pack_key, listed FROM platform_pack WHERE tenant_id = $1 ORDER BY pack_key, version",
    )
    .bind(w.tenant)
    .fetch_all(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(listed.iter().filter(|(_, l)| !l).count(), 3, "{listed:?}");

    // a wider prefix wins as the shortest cover; removing it falls back to the narrower one
    let (s, b) = w
        .call(
            &w.km,
            "PUT",
            URI,
            json!({"key_prefix": "industry", "reason": "none"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let i = Some("industry".to_string());
    assert_eq!(
        w.cache().await,
        vec![
            ("base".to_string(), None),
            ("industry/retail".to_string(), i.clone()),
            ("industry/retail".to_string(), i.clone()),
            ("industry/retail/hr".to_string(), i.clone()),
            ("industry/retailx".to_string(), i.clone()),
        ]
    );
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("{URI}?key_prefix=industry"),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert_eq!(
        w.cache().await,
        vec![
            ("base".to_string(), None),
            ("industry/retail".to_string(), p.clone()),
            ("industry/retail".to_string(), p.clone()),
            ("industry/retail/hr".to_string(), p.clone()),
            ("industry/retailx".to_string(), None),
        ]
    );
    let (s, b) = w
        .call(
            &w.km,
            "DELETE",
            &format!("{URI}?key_prefix=industry%2Fretail"),
            Value::Null,
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert!(w.cache().await.iter().all(|(_, x)| x.is_none()));
    // and the packs are listed again
    let relisted: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform_pack WHERE tenant_id = $1 AND listed")
            .bind(w.tenant)
            .fetch_one(&w.e.owner)
            .await
            .unwrap();
    assert_eq!(relisted, 5);
}
