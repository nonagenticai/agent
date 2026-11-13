//! Per-job API keys (B13 §4, crate::interaction::apikeys): create / list / revoke under `integrations_manager`, the
//! secret shown once, and the key as a caller of the interaction routes only (`JobCaller`). Real Postgres 18 (one
//! container for this binary), over a fake interaction backend.
use crate::common;

use std::sync::{Arc, Mutex};

use agent_api::interaction::{
    BackendError, BackendEvent, InteractionBackend, InteractionRequest, Usage,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde_json::{Value, json};
use uuid::Uuid;

/// "Hello", done; remembers the last request.
#[derive(Default)]
struct Hello {
    last: Mutex<Option<InteractionRequest>>,
}

impl InteractionBackend for Hello {
    fn respond(
        &self,
        req: InteractionRequest,
    ) -> BoxStream<'static, Result<BackendEvent, BackendError>> {
        *self.last.lock().unwrap() = Some(req);
        stream::iter(vec![
            Ok(BackendEvent::Delta("Hello".into())),
            Ok(BackendEvent::Done {
                usage: Some(Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                }),
            }),
        ])
        .boxed()
    }
}

const KEYS: &str = "/api/jobs/faq_agent/api-keys";
const NATIVE: &str = "/api/jobs/faq_agent/interactions";

/// An env over `hello`: tenant Alpha with an integrations manager `u-im`, an observer `u-obs` and the jobs
/// `faq_agent` + `second_job`; tenant Beta with `other_job`.
async fn setup(hello: Arc<Hello>) -> TestEnv {
    let mut e = common::env().await;
    e.state.interaction = hello;
    e.app = agent_api::router(e.state.clone());
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-im", t, "integrations_manager").await;
    insert_user(&e.owner, "u-obs", t, "tenant_user").await;
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, "Beta").await;
    for (tenant, key) in [(t, "faq_agent"), (t, "second_job"), (other, "other_job")] {
        sqlx::query(
            "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, 'J', '[]', 'test')",
        )
        .bind(tenant)
        .bind(key)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    e
}

fn im() -> String {
    person("u-im", &[])
}

/// A key of `faq_agent` created by `u-im`: (id, secret).
async fn create(e: &TestEnv, label: &str) -> (Uuid, String) {
    let (s, b) = send(
        &e.app,
        json("POST", KEYS, Some(&im()), json!({"label": label})),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    (
        b["key"]["id"].as_str().unwrap().parse().unwrap(),
        b["secret"].as_str().unwrap().to_string(),
    )
}

/// A JSON request carrying `secret` in `header` (`authorization` as a Bearer, or `x-api-key` as is).
fn with_key(method: &str, uri: &str, header: &str, secret: &str, body: Value) -> Request<Body> {
    let value = if header == "authorization" {
        format!("Bearer {secret}")
    } else {
        secret.to_string()
    };
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header(header, value)
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn code(b: &Value) -> Option<&str> {
    b["error"]["code"].as_str()
}

#[tokio::test]
async fn create_returns_secret_once_and_list_hides_it() {
    let e = setup(Arc::new(Hello::default())).await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            KEYS,
            Some(&im()),
            json!({"label": "CRM bridge", "expires_at": "2099-01-01T00:00:00Z"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let secret = b["secret"].as_str().unwrap().to_string();
    assert!(secret.starts_with("ak_") && secret.len() == 43, "{secret}");
    let key = &b["key"];
    assert_eq!(key["label"], "CRM bridge");
    assert_eq!(key["prefix"], &secret[..10]);
    assert_eq!(key["state"], "active");
    assert_eq!(key["created_by"], "u-im");
    assert_eq!(key["last_used_at"], Value::Null);
    assert_eq!(key["revoked_at"], Value::Null);
    assert!(
        key["expires_at"]
            .as_str()
            .unwrap()
            .starts_with("2099-01-01T00:00:00")
    );
    assert!(
        key.get("secret").is_none() && key.get("key_hash").is_none(),
        "{key}"
    );

    let (s, list) = send(&e.app, get(KEYS, Some(&im()))).await;
    assert_eq!(s, StatusCode::OK, "{list}");
    assert_eq!(list["items"].as_array().unwrap().len(), 1, "{list}");
    assert_eq!(list["items"][0]["id"], key["id"]);
    assert!(
        !list.to_string().contains(&secret),
        "the list never carries the secret"
    );
    // only the hash is stored
    let stored: Vec<u8> = sqlx::query_scalar("SELECT key_hash FROM job_api_keys")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(stored.len(), 32);
    let audit: Vec<(String, Value)> = sqlx::query_as(
        "SELECT action, details FROM audit_event WHERE action LIKE 'job_api_key.%' ORDER BY id",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(audit.len(), 1, "{audit:?}");
    assert_eq!(audit[0].0, "job_api_key.create");
    assert!(
        !audit[0].1.to_string().contains(&secret),
        "the audit never carries the secret"
    );

    for bad in [
        json!({"label": " "}),
        json!({"label": "x", "expires_at": "2001-01-01T00:00:00Z"}),
    ] {
        let (s, b) = send(&e.app, json("POST", KEYS, Some(&im()), bad.clone())).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
    }
}

/// D109: a key caller is a customer party (`api:<key id>`, or the body's own), so its request is a B4 runtime turn —
/// never the playground backend; this job has no live release, so the authenticated key gets 409 `job_not_live`
/// (the runtime's answer is tests/kh `t6_answer_path`). A member without a party is the staff playground.
#[tokio::test]
async fn key_is_a_customer_and_never_reaches_the_playground() {
    let hello = Arc::new(Hello::default());
    let e = setup(hello.clone()).await;
    let (id, secret) = create(&e, "bot").await;
    let not_live = |s: StatusCode, b: &Value| {
        assert_eq!(
            (s, code(b)),
            (StatusCode::CONFLICT, Some("job_not_live")),
            "{b}"
        );
    };
    let (s, b) = send(
        &e.app,
        with_key(
            "POST",
            NATIVE,
            "authorization",
            &secret,
            json!({"input": "hi"}),
        ),
    )
    .await;
    not_live(s, &b);
    // the Bearer scheme is case-insensitive
    let lower = Request::builder()
        .method("POST")
        .uri(NATIVE)
        .header("content-type", "application/json")
        .header("authorization", format!("bearer {secret}"))
        .body(Body::from(json!({"input": "hi"}).to_string()))
        .unwrap();
    let (s, b) = send(&e.app, lower).await;
    not_live(s, &b);
    // a party the caller names is a customer too
    let (s, b) = send(
        &e.app,
        with_key(
            "POST",
            NATIVE,
            "x-api-key",
            &secret,
            json!({"input": "hi", "party": {"id": "crm:7"}}),
        ),
    )
    .await;
    not_live(s, &b);
    // the OpenAI shape over X-Api-Key
    let (s, b) = send(
        &e.app,
        with_key(
            "POST",
            "/api/jobs/faq_agent/chat/completions",
            "x-api-key",
            &secret,
            json!({"messages": [{"role": "user", "content": "hi"}]}),
        ),
    )
    .await;
    not_live(s, &b);
    assert!(
        hello.last.lock().unwrap().is_none(),
        "a key caller never reaches the playground"
    );
    let used: Option<chrono::DateTime<chrono::Utc>> =
        sqlx::query_scalar("SELECT last_used_at FROM job_api_keys WHERE id = $1")
            .bind(id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert!(used.is_some(), "last_used_at is stamped");
    // a member keeps the staff playground
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            NATIVE,
            Some(&person("u-obs", &[])),
            json!({"input": "hi"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["output_text"], "Hello");
    assert!(hello.last.lock().unwrap().clone().unwrap().party.is_none());
}

#[tokio::test]
async fn revoked_key_401() {
    let e = setup(Arc::new(Hello::default())).await;
    let (id, secret) = create(&e, "bot").await;
    let uri = format!("{KEYS}/{id}");
    let (s, b) = send(&e.app, json("DELETE", &uri, Some(&im()), json!({}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    // revoking again is idempotent, audited once
    let (s, _) = send(&e.app, json("DELETE", &uri, Some(&im()), json!({}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, b) = send(
        &e.app,
        with_key(
            "POST",
            NATIVE,
            "authorization",
            &secret,
            json!({"input": "hi"}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, Some("invalid_api_key")),
        "{b}"
    );
    let (_, list) = send(&e.app, get(KEYS, Some(&im()))).await;
    assert_eq!(list["items"][0]["state"], "revoked", "{list}");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'job_api_key.revoke' AND resource_id = $1",
    )
    .bind(id.to_string())
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(n, 1);
    let (s, b) = send(
        &e.app,
        json(
            "DELETE",
            &format!("{KEYS}/{}", Uuid::new_v4()),
            Some(&im()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, Some("api_key_not_found"))
    );
}

#[tokio::test]
async fn expired_key_401() {
    let e = setup(Arc::new(Hello::default())).await;
    let (id, secret) = create(&e, "bot").await;
    sqlx::query("UPDATE job_api_keys SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(id)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        with_key(
            "POST",
            NATIVE,
            "authorization",
            &secret,
            json!({"input": "hi"}),
        ),
    )
    .await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNAUTHORIZED, Some("invalid_api_key")),
        "{b}"
    );
    let (_, list) = send(&e.app, get(KEYS, Some(&im()))).await;
    assert_eq!(list["items"][0]["state"], "expired", "{list}");
    // unknown and malformed keys get the same answer
    let unknown = format!("ak_{}", "a".repeat(40));
    for k in [unknown.as_str(), "ak_short", "not-a-key"] {
        let (s, b) = send(
            &e.app,
            with_key("POST", NATIVE, "x-api-key", k, json!({"input": "hi"})),
        )
        .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::UNAUTHORIZED, Some("invalid_api_key")),
            "{k}: {b}"
        );
    }
}

#[tokio::test]
async fn key_of_other_job_404() {
    let e = setup(Arc::new(Hello::default())).await;
    let (_, secret) = create(&e, "bot").await;
    for uri in [
        "/api/jobs/second_job/interactions",
        "/api/jobs/other_job/interactions",
        "/api/jobs/no_such_job/interactions",
    ] {
        let (s, b) = send(
            &e.app,
            with_key(
                "POST",
                uri,
                "authorization",
                &secret,
                json!({"input": "hi"}),
            ),
        )
        .await;
        assert_eq!(
            (s, code(&b)),
            (StatusCode::NOT_FOUND, Some("job_not_found")),
            "{uri}: {b}"
        );
    }
}

#[tokio::test]
async fn key_cannot_reach_admin_routes() {
    let e = setup(Arc::new(Hello::default())).await;
    let (_, secret) = create(&e, "bot").await;
    let bearer =
        |method: &str, uri: &str| with_key(method, uri, "authorization", &secret, json!({}));
    for req in [
        bearer("GET", "/api/jobs/faq_agent/channel-bindings"),
        bearer("GET", KEYS),
        bearer("POST", KEYS),
        bearer("GET", "/api/jobs/faq_agent/interactions/webhook-secret"),
        bearer("GET", "/api/me"),
    ] {
        let uri = req.uri().to_string();
        let (s, b) = send(&e.app, req).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{uri}: {b}");
    }
}

#[tokio::test]
async fn create_requires_integrations_manager() {
    let e = setup(Arc::new(Hello::default())).await;
    let obs = person("u-obs", &[]);
    let (_, b) = send(
        &e.app,
        json("POST", KEYS, Some(&obs), json!({"label": "x"})),
    )
    .await;
    assert_eq!(b["error"]["missing_role"], "integrations_manager", "{b}");
    let (s, b) = send(&e.app, get(KEYS, Some(&obs))).await;
    assert_eq!(
        (s, b["error"]["missing_role"].as_str()),
        (StatusCode::FORBIDDEN, Some("integrations_manager"))
    );
    let (id, _) = create(&e, "bot").await;
    let (s, b) = send(
        &e.app,
        json("DELETE", &format!("{KEYS}/{id}"), Some(&obs), json!({})),
    )
    .await;
    assert_eq!(
        (s, b["error"]["missing_role"].as_str()),
        (StatusCode::FORBIDDEN, Some("integrations_manager"))
    );
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM job_api_keys WHERE revoked_at IS NULL")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1, "the observer created and revoked nothing");
}
