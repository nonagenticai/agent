//! B3 T1 foundations: provider secrets sealed under the ProviderSecret keyring (AAD `<tenant>:<kind>:<row>`), the
//! 503 without `PROVIDER_SECRET_KEYS`, fingerprints, tenant-entered URLs mapped to the UI codes through
//! crate::netguard, the upstream error mapping, the strata token maps, and `Debug` never printing a secret.
use crate::common;

use std::collections::HashMap;

use agent_api::config::{Config, TenantTokens};
use agent_api::crypto::keyring::{KeySet, Keyring, Purpose};
use agent_api::error::ApiError;
use agent_api::netguard::Policy;
use agent_api::providers::secret::{self, SecretKind};
use agent_api::providers::{UrlField, check_url};
use agent_api::upstream::{self, Auth, Call};
use axum::Json;
use axum::Router;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use base64::Engine as _;
use http_body_util::BodyExt;
use reqwest::Method;
use serde_json::{Value, json};
use uuid::Uuid;

/// `{"p1": base64([byte; 32])}` (built at runtime: no key literal is committed).
fn keys_json(byte: u8) -> String {
    let key = base64::engine::general_purpose::STANDARD.encode([byte; 32]);
    format!(r#"{{"p1":"{key}"}}"#)
}

fn provider_ring(byte: u8) -> Keyring {
    let set = KeySet::parse(Purpose::ProviderSecret, Some(&keys_json(byte)), Some("p1"))
        .unwrap()
        .unwrap();
    Keyring::new(Purpose::ProviderSecret, &set)
}

/// The base test environment (no database is contacted: config parsing only) with `extra` on top.
fn config_with(extra: &[(&str, &str)]) -> anyhow::Result<Config> {
    let mut vars = common::base_env("postgres://owner:pw@127.0.0.1:5432/b3_cfg");
    for (k, v) in extra {
        vars.insert((*k).to_string(), (*v).to_string());
    }
    Config::from_lookup(|k| vars.get(k).cloned())
}

async fn error_body(e: ApiError) -> (StatusCode, Value) {
    let resp = e.into_response();
    let st = resp.status();
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    (st, serde_json::from_slice(&b).unwrap())
}

#[test]
fn provider_secret_roundtrip_binds_aad() {
    let kr = provider_ring(3);
    let (t, other_t) = (Uuid::new_v4(), Uuid::new_v4());
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let s = secret::seal(Some(&kr), t, SecretKind::LlmKey, a, "sk-roundtrip-1").unwrap();
    assert!(s.sealed.starts_with("v2:p1:"), "{}", s.sealed);
    assert!(!s.sealed.contains("sk-roundtrip-1"));
    let opened = secret::open(Some(&kr), t, SecretKind::LlmKey, a, &s.sealed).unwrap();
    assert_eq!(opened.expose(), "sk-roundtrip-1");
    assert_eq!(
        secret::aad(t, SecretKind::LlmKey, a),
        format!("{t}:llm_key:{a}")
    );
    // another row, another tenant, another kind, another key: 409 secret_unreadable, never the keyring's text
    let other_ring = provider_ring(4);
    for (ring, tenant, kind, row) in [
        (&kr, t, SecretKind::LlmKey, b),
        (&kr, other_t, SecretKind::LlmKey, a),
        (&kr, t, SecretKind::SlotKey, a),
        (&other_ring, t, SecretKind::LlmKey, a),
    ] {
        let e = secret::open(Some(ring), tenant, kind, row, &s.sealed).unwrap_err();
        assert_eq!(
            e.status_and_code(),
            (StatusCode::CONFLICT, "secret_unreadable")
        );
    }
    let e = secret::open(Some(&kr), t, SecretKind::LlmKey, a, "v1:junk").unwrap_err();
    assert_eq!(e.code(), "secret_unreadable");
    assert_eq!(SecretKind::SlotKey.as_str(), "slot_key");
    assert_eq!(SecretKind::SourceCred.as_str(), "source_cred");
}

#[tokio::test]
async fn provider_secret_keys_unset_is_503_provider_secret_key_unconfigured() {
    let e = common::env().await;
    assert!(
        e.state.provider_keyring.is_none(),
        "PROVIDER_SECRET_KEYS is optional"
    );
    let (t, row) = (Uuid::new_v4(), Uuid::new_v4());
    let kr = e.state.provider_keyring.as_deref();
    let err = secret::seal(kr, t, SecretKind::SlotKey, row, "k").unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_secret_key_unconfigured"
        )
    );
    let err = secret::open(kr, t, SecretKind::SlotKey, row, "v2:p1:AAAA").unwrap_err();
    assert_eq!(
        err.status_and_code(),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            "provider_secret_key_unconfigured"
        )
    );
    // configured: the state carries a ProviderSecret ring that seals and opens
    let e = common::env_with(|vars: &mut HashMap<String, String>| {
        vars.insert("PROVIDER_SECRET_KEYS".into(), keys_json(9));
        vars.insert("PROVIDER_SECRET_KEY_CURRENT".into(), "p1".into());
    })
    .await;
    let kr = e.state.provider_keyring.as_deref().expect("configured");
    assert_eq!(kr.purpose(), Purpose::ProviderSecret);
    let s = secret::seal(Some(kr), t, SecretKind::SourceCred, row, "pw").unwrap();
    let opened = secret::open(Some(kr), t, SecretKind::SourceCred, row, &s.sealed).unwrap();
    assert_eq!(opened.expose(), "pw");
}

#[test]
fn provider_secret_keys_bad_value_names_var() {
    assert!(config_with(&[]).unwrap().provider_secret_keys.is_none());
    let good = keys_json(5);
    let ok = config_with(&[
        ("PROVIDER_SECRET_KEYS", good.as_str()),
        ("PROVIDER_SECRET_KEY_CURRENT", "p1"),
    ])
    .unwrap();
    assert_eq!(ok.provider_secret_keys.as_ref().unwrap().current, "p1");
    let short = format!(
        r#"{{"p1":"{}"}}"#,
        base64::engine::general_purpose::STANDARD.encode([5u8; 16])
    );
    for vars in [
        vec![("PROVIDER_SECRET_KEYS", good.as_str())],
        vec![("PROVIDER_SECRET_KEY_CURRENT", "p1")],
        vec![
            ("PROVIDER_SECRET_KEYS", good.as_str()),
            ("PROVIDER_SECRET_KEY_CURRENT", "p2"),
        ],
        vec![
            ("PROVIDER_SECRET_KEYS", short.as_str()),
            ("PROVIDER_SECRET_KEY_CURRENT", "p1"),
        ],
        vec![
            ("PROVIDER_SECRET_KEYS", "not json"),
            ("PROVIDER_SECRET_KEY_CURRENT", "p1"),
        ],
    ] {
        let e = config_with(&vars).unwrap_err().to_string();
        assert!(e.contains("PROVIDER_SECRET_KEY"), "{e}");
        let key = good.split('"').nth(3).unwrap();
        assert!(!e.contains(key), "{e}");
        assert!(!e.contains("TENANT_DATA"), "{e}");
    }
}

#[test]
fn fingerprint_stable_and_tenant_bound() {
    let (t, u) = (Uuid::new_v4(), Uuid::new_v4());
    let f = secret::fingerprint(t, SecretKind::LlmKey, "sk-fpr");
    assert_eq!(f.len(), 8);
    assert!(f.bytes().all(|b| b.is_ascii_hexdigit()), "{f}");
    assert_eq!(f, secret::fingerprint(t, SecretKind::LlmKey, "sk-fpr"));
    assert_ne!(f, secret::fingerprint(u, SecretKind::LlmKey, "sk-fpr"));
    assert_ne!(f, secret::fingerprint(t, SecretKind::SlotKey, "sk-fpr"));
    assert_ne!(f, secret::fingerprint(t, SecretKind::LlmKey, "sk-fpr2"));
    // the fingerprint the seal returns is the same function, and is stable across seals (fresh nonces)
    let kr = provider_ring(6);
    let row = Uuid::new_v4();
    let a = secret::seal(Some(&kr), t, SecretKind::LlmKey, row, "sk-fpr").unwrap();
    let b = secret::seal(Some(&kr), t, SecretKind::LlmKey, row, "sk-fpr").unwrap();
    assert_ne!(a.sealed, b.sealed);
    assert_eq!(
        (a.fingerprint.as_str(), b.fingerprint.as_str()),
        (f.as_str(), f.as_str())
    );
}

#[test]
fn netguard_maps_to_ui_codes() {
    let policy = Policy::default();
    let t = Uuid::new_v4();
    let refused_targets = [
        "https://10.1.2.3",
        "https://127.0.0.1",
        "https://169.254.169.254/latest",
        "https://172.16.0.1",
        "https://172.31.255.1",
        "https://192.168.1.10",
        "https://100.64.0.1",
        "https://100.127.0.1",
        "https://[::1]",
        "https://minio.internal",
        "https://rag.svc",
        "https://rag.agent.svc.cluster.local",
        "https://minio",
    ];
    for raw in refused_targets {
        for (field, code) in [
            (UrlField::BaseUrl, "invalid_target"),
            (UrlField::Source, "address_refused"),
        ] {
            let e = check_url(raw, &policy, t, field).unwrap_err();
            assert_eq!(
                e.status_and_code(),
                (StatusCode::UNPROCESSABLE_ENTITY, code),
                "{raw}"
            );
        }
    }
    let bad_shapes = [
        "http://api.example.com",
        "not a url",
        "https://user:pw@api.example.com",
        "https://api.example.com/v1?key=x",
        "ftp://api.example.com",
    ];
    for raw in bad_shapes {
        for (field, code) in [
            (UrlField::BaseUrl, "invalid_base_url"),
            (UrlField::Source, "invalid_source"),
        ] {
            let e = check_url(raw, &policy, t, field).unwrap_err();
            assert_eq!(
                e.status_and_code(),
                (StatusCode::UNPROCESSABLE_ENTITY, code),
                "{raw}"
            );
            assert!(!e.to_string().contains("pw"), "{e}");
        }
    }
    for raw in [
        "https://api.openai.com/v1",
        "https://172.15.0.1",
        "https://docs.example.com/feed.xml",
    ] {
        let url = check_url(raw, &policy, t, UrlField::BaseUrl).unwrap();
        assert_eq!(url.scheme(), "https", "{raw}");
        check_url(raw, &policy, t, UrlField::Source).unwrap();
    }
}

/// A strata store id the fake names in its messages: it must never reach a console message.
const LEAKED_STORE: &str = "5d1e0c3a-8b1f-4c2e-9a7d-0e6f4b2a1c9d";

/// A fake upstream on 127.0.0.1:0: fixed answers per path, and `/echo` returning the headers it received.
async fn fake_upstream() -> String {
    let app = Router::new()
        .route("/ok", get(|| async { Json(json!({"items": [1]})) }))
        .route("/no-content", get(|| async { StatusCode::NO_CONTENT }))
        .route(
            "/500-listed",
            get(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": {"code": "conflict", "message": "boom"}})),
                )
            }),
        )
        .route("/502", get(|| async { (StatusCode::BAD_GATEWAY, "bad gateway") }))
        .route(
            "/502-upstream",
            get(|| async {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({"error": {"code": "upstream_unavailable", "message": "provider down"}})),
                )
            }),
        )
        .route(
            "/503",
            get(|| async { (StatusCode::SERVICE_UNAVAILABLE, "") }),
        )
        .route(
            "/401",
            get(|| async {
                (
                    StatusCode::UNAUTHORIZED,
                    Json(json!({"error": {"code": "not_found", "message": "who"}})),
                )
            }),
        )
        .route("/404-bare", get(|| async { (StatusCode::NOT_FOUND, "nope") }))
        .route("/bad-2xx", get(|| async { "not json" }))
        .route(
            "/leaky-store",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": {"code": "not_found",
                        "message": format!("not found: store {LEAKED_STORE}")}})),
                )
            }),
        )
        .route(
            "/leaky-provider",
            get(|| async {
                (
                    StatusCode::CONFLICT,
                    Json(json!({"error": {"code": "provider_not_configured",
                        "message": format!("store {LEAKED_STORE}: no embed provider is configured for this tenant; \
                                            register one with PUT /api/tenant/providers")}})),
                )
            }),
        )
        .route(
            "/empty-sync",
            get(|| async {
                (
                    StatusCode::CONFLICT,
                    Json(json!({"error": {"code": "empty_sync_run", "message": "empty sync run for /srv/strata/x"},
                        "kept": 3, "pruned": 0})),
                )
            }),
        )
        .route(
            "/eval",
            get(|| async {
                (
                    StatusCode::CONFLICT,
                    Json(json!({"error": {"code": "eval_running", "message": "an eval is already running",
                        "run_id": "r-1", "api_key": "sk-leak-1"}})),
                )
            }),
        )
        .route(
            "/missing",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": {"code": "not_found", "message": "no such store"}})),
                )
            }),
        )
        .route(
            "/unlisted",
            get(|| async {
                (
                    StatusCode::CONFLICT,
                    Json(json!({"error": {"code": "weird_new_code", "message": "x"}})),
                )
            }),
        )
        .route(
            "/echo",
            get(|h: HeaderMap| async move {
                let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
                Json(json!({
                    "authorization": get("authorization"),
                    "tenant": get(upstream::TENANT_HEADER),
                    "actor": get(upstream::ACTOR_HEADER),
                }))
            }),
        );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    base
}

async fn get_path(
    http: &reqwest::Client,
    base: &str,
    path: &str,
) -> agent_api::error::ApiResult<upstream::Reply> {
    Call::new("strata_rag", "test", base, Method::GET, path)
        .send(http)
        .await
}

#[tokio::test]
async fn upstream_maps_5xx_to_unavailable() {
    let base = fake_upstream().await;
    let http = reqwest::Client::new();
    for path in [
        "/500-listed",
        "/502",
        "/503",
        "/401",
        "/404-bare",
        "/bad-2xx",
    ] {
        let e = get_path(&http, &base, path).await.unwrap_err();
        assert_eq!(
            e.status_and_code(),
            (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            "{path}"
        );
        let (_, body) = error_body(e).await;
        assert!(!body.to_string().contains("boom"), "{path}: {body}");
    }
    // nothing listening: a connection failure is the same 503
    let closed = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("http://{}", l.local_addr().unwrap())
    };
    let e = get_path(&http, &closed, "/ok").await.unwrap_err();
    assert_eq!(
        e.status_and_code(),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    );
    // the service's own upstream down is its listed 502
    let e = get_path(&http, &base, "/502-upstream").await.unwrap_err();
    assert_eq!(
        e.status_and_code(),
        (StatusCode::BAD_GATEWAY, "upstream_unavailable")
    );
    // successes
    let r = get_path(&http, &base, "/ok").await.unwrap();
    assert_eq!((r.status, r.body), (StatusCode::OK, json!({"items": [1]})));
    let r = get_path(&http, &base, "/no-content").await.unwrap();
    assert_eq!((r.status, r.body), (StatusCode::NO_CONTENT, Value::Null));
}

#[tokio::test]
async fn upstream_passes_listed_codes() {
    let base = fake_upstream().await;
    let http = reqwest::Client::new();
    let e = get_path(&http, &base, "/eval").await.unwrap_err();
    let (st, body) = error_body(e).await;
    assert_eq!(st, StatusCode::CONFLICT);
    assert_eq!(body["error"]["code"], "eval_running");
    assert_eq!(
        body["error"]["message"],
        upstream::message_for("eval_running").unwrap(),
        "agent's own message, not strata's"
    );
    assert_ne!(body["error"]["message"], "an eval is already running");
    assert_eq!(body["error"]["run_id"], "r-1", "extra fields are kept");
    assert!(
        !body.to_string().contains("sk-leak-1"),
        "secret-named fields are scrubbed: {body}"
    );
    let e = get_path(&http, &base, "/missing").await.unwrap_err();
    assert_eq!(e.status_and_code(), (StatusCode::NOT_FOUND, "not_found"));
    let e = get_path(&http, &base, "/unlisted").await.unwrap_err();
    assert_eq!(
        e.status_and_code(),
        (StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    );
    // the allow-list itself
    for code in [
        "conflict",
        "not_found",
        "eval_running",
        "question_exists",
        "too_many_questions",
        "invalid_input",
        "eval_not_ready",
        "child_of_parent_job",
        "upstream_unavailable",
        "pack_published",
        "embed_model_mismatch",
        "provider_not_configured",
        "ambiguous_root",
        "invalid_target",
        "unknown_store",
        "duplicate_name",
        "has_children",
        "empty_sync_run",
        "leaf_full",
        "payload_too_large",
        "probe_failed",
        "region_not_allowed",
        "zdr_required",
        "dims_must_be_1024",
        "dialect_not_allowed",
        "api_key_required",
        "invalid_provider",
        // B3 T4c: strata's provider test route
        "provider_host_not_allowed",
        "secret_required_for_new_host",
        "unknown_capability",
    ] {
        assert_eq!(upstream::pass_through(code), Some(code));
        let msg = upstream::message_for(code).unwrap_or("");
        assert!(!msg.is_empty(), "{code} has an agent message");
    }
    assert_eq!(upstream::pass_through("weird_new_code"), None);
    // the pinned token, the tenant and a uuid actor go out as headers; a nil actor never does
    let tenant = Uuid::new_v4();
    let actor = Uuid::new_v4();
    let r = Call::new("strata_rag", "test", &base, Method::GET, "/echo")
        .auth(Auth {
            token: Some("tok-echo-1"),
            tenant: Some(tenant),
            actor: Some(actor),
            app: None,
        })
        .send(&http)
        .await
        .unwrap();
    assert_eq!(r.body["authorization"], "Bearer tok-echo-1");
    assert_eq!(r.body["tenant"], tenant.to_string());
    assert_eq!(r.body["actor"], actor.to_string());
    let r = Call::new("strata_rag", "test", &base, Method::GET, "/echo")
        .auth(Auth {
            token: None,
            tenant: None,
            actor: Some(Uuid::nil()),
            app: None,
        })
        .send(&http)
        .await
        .unwrap();
    assert_eq!(
        r.body,
        json!({"authorization": null, "tenant": null, "actor": null})
    );
}

#[test]
fn strata_tokens_parse() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let ok = TenantTokens::parse("STRATA_RAG_TOKENS", &format!(" {a}:tok-a , {b}:tok-b,")).unwrap();
    assert_eq!(ok.len(), 2);
    assert_eq!(ok.get(a), Some("tok-a"));
    assert_eq!(ok.get(b), Some("tok-b"));
    assert_eq!(ok.get(Uuid::new_v4()), None);
    assert!(
        TenantTokens::parse("STRATA_RAG_TOKENS", "")
            .unwrap()
            .is_empty()
    );
    let nil = Uuid::nil();
    for (raw, pos) in [
        (format!("{nil}:tok-secret-1"), "entry 1"),
        (format!("{a}:tok-secret-1,{b}:tok-secret-1"), "entry 2"),
        (format!("{a}:tok-secret-1,{a}:tok-secret-2"), "entry 2"),
        (format!("{a}:tok-secret-1,garbage-secret"), "entry 2"),
        ("*:tok-secret-1".to_string(), "entry 1"),
        (format!("{a}:"), "entry 1"),
        (format!("{a}:tok secret"), "entry 1"),
    ] {
        let e = TenantTokens::parse("STRATA_INGEST_TOKENS", &raw).unwrap_err();
        assert!(
            e.contains("STRATA_INGEST_TOKENS") && e.contains(pos),
            "{raw}: {e}"
        );
        assert!(!e.contains("secret"), "never the value: {e}");
    }
    // through the config: a URL needs its tokens; tokens are parsed and pinned per tenant
    let none = config_with(&[]).unwrap();
    assert!(none.strata.rag_url.is_none() && none.strata.rag_tokens.is_empty());
    let e = config_with(&[("STRATA_RAG_URL", "http://strata-rag.strata:8080")])
        .unwrap_err()
        .to_string();
    assert!(e.contains("STRATA_RAG_TOKENS"), "{e}");
    let rag_tokens = format!("{a}:tok-a");
    let ingest_tokens = format!("{b}:tok-b");
    let cfg = config_with(&[
        ("STRATA_RAG_URL", "http://strata-rag.strata:8080/"),
        ("STRATA_RAG_TOKENS", rag_tokens.as_str()),
        ("STRATA_INGEST_URL", "http://strata-ingest.strata:8080"),
        ("STRATA_INGEST_TOKENS", ingest_tokens.as_str()),
    ])
    .unwrap();
    assert_eq!(
        cfg.strata.rag_url.as_deref(),
        Some("http://strata-rag.strata:8080")
    );
    assert_eq!(cfg.strata.rag_tokens.get(a), Some("tok-a"));
    assert_eq!(cfg.strata.rag_tokens.get(b), None, "pinned per tenant");
    assert_eq!(cfg.strata.ingest_tokens.get(b), Some("tok-b"));
    let e = config_with(&[
        ("STRATA_INGEST_URL", "strata-ingest:8080"),
        ("STRATA_INGEST_TOKENS", ingest_tokens.as_str()),
    ])
    .unwrap_err()
    .to_string();
    assert!(e.contains("STRATA_INGEST_URL"), "{e}");
    // internal providers and the platform tenant
    assert_eq!(
        none.internal_providers.llm_url,
        agent_api::config::DEFAULT_INTERNAL_LLM_URL
    );
    assert_eq!(none.internal_providers.llm_model, "local-llm");
    assert!(none.internal_providers.llm_key.is_none() && none.platform_tenant_id.is_none());
    let e = config_with(&[("AGENT_PLATFORM_TENANT_ID", nil.to_string().as_str())])
        .unwrap_err()
        .to_string();
    assert!(e.contains("AGENT_PLATFORM_TENANT_ID"), "{e}");
    let cfg = config_with(&[("AGENT_PLATFORM_TENANT_ID", a.to_string().as_str())]).unwrap();
    assert_eq!(cfg.platform_tenant_id, Some(a));
}

#[test]
fn debug_never_prints_tokens_or_keys() {
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    let keys = keys_json(0xAB);
    let key_b64 = keys.split('"').nth(3).unwrap().to_string();
    let rag_tokens = format!("{a}:tok-rag-secret");
    let ingest_tokens = format!("{b}:tok-ingest-secret");
    let cfg = config_with(&[
        ("PROVIDER_SECRET_KEYS", keys.as_str()),
        ("PROVIDER_SECRET_KEY_CURRENT", "p1"),
        ("STRATA_RAG_URL", "http://strata-rag.strata:8080"),
        ("STRATA_RAG_TOKENS", rag_tokens.as_str()),
        ("STRATA_INGEST_URL", "http://strata-ingest.strata:8080"),
        ("STRATA_INGEST_TOKENS", ingest_tokens.as_str()),
        ("INTERNAL_LLM_KEY", "int-key-9"),
    ])
    .unwrap();
    let kr = provider_ring(0xAB);
    let sealed = secret::seal(Some(&kr), a, SecretKind::LlmKey, b, "sk-debug-secret").unwrap();
    let opened = secret::open(Some(&kr), a, SecretKind::LlmKey, b, &sealed.sealed).unwrap();
    let auth = Auth {
        token: Some("tok-auth-secret"),
        tenant: Some(a),
        actor: None,
        app: None,
    };
    let shown = [
        format!("{cfg:?}"),
        format!("{:?}", cfg.strata),
        format!("{:?}", cfg.internal_providers),
        format!("{kr:?}"),
        format!("{sealed:?}"),
        format!("{opened:?}"),
        format!("{auth:?}"),
    ];
    for s in &shown {
        for secret in [
            key_b64.as_str(),
            "tok-rag-secret",
            "tok-ingest-secret",
            "int-key-9",
            "sk-debug-secret",
            "tok-auth-secret",
            sealed.sealed.as_str(),
        ] {
            assert!(!s.contains(secret), "{secret} printed in {s}");
        }
        assert!(s.contains("[redacted]"), "{s}");
    }
    // what is not secret is still visible
    assert!(shown[1].contains(&a.to_string()), "{}", shown[1]);
    assert!(shown[4].contains(&sealed.fingerprint), "{}", shown[4]);
}

/// I4: a passed-through refusal carries agent's message per code; strata's text (store ids, paths, strata routes)
/// never reaches the body. A store a 404 names is kept as the extra field `store` (rag::unreadable_store); a report
/// strata puts beside the error object (`empty_sync_run`) is kept as extra fields.
#[tokio::test]
async fn upstream_message_is_agents_own_never_stratas() {
    let base = fake_upstream().await;
    let http = reqwest::Client::new();
    for (path, want) in [
        ("/leaky-store", (StatusCode::NOT_FOUND, "not_found")),
        (
            "/leaky-provider",
            (StatusCode::CONFLICT, "provider_not_configured"),
        ),
        ("/empty-sync", (StatusCode::CONFLICT, "empty_sync_run")),
    ] {
        let e = get_path(&http, &base, path).await.unwrap_err();
        assert_eq!(e.status_and_code(), want, "{path}");
        let (_, body) = error_body(e).await;
        let msg = body["error"]["message"].as_str().unwrap();
        assert_eq!(Some(msg), upstream::message_for(want.1), "{path}: {body}");
        for leak in [LEAKED_STORE, "/api/tenant", "/srv/strata", "PUT "] {
            assert!(!msg.contains(leak), "{path}: {leak} in {msg}");
        }
    }
    let e = get_path(&http, &base, "/leaky-store").await.unwrap_err();
    assert_eq!(
        agent_api::strata::rag::unreadable_store(&e).map(|s| s.0.to_string()),
        Some(LEAKED_STORE.to_string()),
        "the store id survives as data"
    );
    let e = get_path(&http, &base, "/leaky-provider").await.unwrap_err();
    assert_eq!(
        agent_api::strata::rag::unreadable_store(&e),
        None,
        "only a not_found names an unreadable store"
    );
    let e = get_path(&http, &base, "/empty-sync").await.unwrap_err();
    let (_, body) = error_body(e).await;
    assert_eq!(
        (
            body["error"]["kept"].clone(),
            body["error"]["pruned"].clone()
        ),
        (json!(3), json!(0)),
        "the report beside the error is kept: {body}"
    );
}
