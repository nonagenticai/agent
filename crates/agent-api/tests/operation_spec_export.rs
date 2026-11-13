//! B7 T13a (F-36): `GET /api/jobs/{job}/operations/spec?format=csv|md&of=draft|live` on a real Postgres 18: the
//! attachment headers, the body is the golden-tested renderer (crate::connectors::spec_export, parity in
//! `tests/connector_route_golden.rs`) over the job's loaded spec, no URL / env id / credential ever leaks, CSV
//! formula cells are escaped, any member (an observer) may export, refusals.
//!
//! `of=live` reads the job's current release snapshot (B2): 409 `no_release` for a job never released, and after an
//! unpublished route change the draft and live exports differ.
use crate::common;
use crate::ops_world;

use std::collections::HashMap;

use agent_api::connectors::routes_store::DesignOf;
use agent_api::connectors::spec_export;
use agent_api::tenancy::begin_scoped;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use common::design_contract;
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

const JOB: &str = "cca";
const ENV_ID_VALUE: &str = "env-id-7c1e";

fn with_keys(vars: &mut HashMap<String, String>) {
    let key = base64::engine::general_purpose::STANDARD.encode([9u8; 32]);
    vars.insert(
        "TENANT_DATA_KEYS".to_string(),
        format!(r#"{{"k1":"{key}"}}"#),
    );
    vars.insert("TENANT_DATA_KEY_CURRENT".to_string(), "k1".to_string());
}

struct World {
    e: TestEnv,
    t: Uuid,
    job: Uuid,
    im: String,
    obs: String,
}

/// Job `cca` (latest tagged customer-care template), system `shop-prod` (test_shop@3, prod, base URL, an env id, a
/// stored credential with a fingerprint), routed for order.read and order.cancel on prod, with one override.
async fn world() -> World {
    let e = common::env_with(with_keys).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-im-a", t, "integrations_manager").await;
    insert_user(&e.owner, "u-obs-a", t, "tenant_user").await;
    ops_world::template(&e).await;
    let job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, template_layer, template_version, created_by) \
         SELECT $1, $2, 'Customer care', pins, layer, version, 'test' FROM job_template_tag \
          WHERE layer = 'job/customer_care_agent' AND status = 'tagged' ORDER BY version DESC LIMIT 1 \
         RETURNING id",
    )
    .bind(t)
    .bind(JOB)
    .fetch_one(&e.owner)
    .await
    .expect("a tagged customer-care template");
    common::platform::publish_template(
        &e,
        "test_shop",
        "3",
        &[
            common::platform::Op::star("order", "read"),
            common::platform::Op::star("order", "cancel"),
        ],
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO connector_instance (tenant_id, id, label, template_scope, template_key, template_version, env, \
                                         base_url, env_ids, status, created_by) \
         VALUES ($1, 'shop-prod', 'Shop', 'platform', 'test_shop', '3', 'prod', \
                 'https://secret-shop.example.com', jsonb_build_object('shop_id', $2::text), 'ok', 'test')",
    )
    .bind(t)
    .bind(ENV_ID_VALUE)
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO connector_credential (tenant_id, instance_id, ciphertext, nonce, credential_version, \
                                           dek_version, fingerprint, updated_by) \
         VALUES ($1, 'shop-prod', $2, $3, 1, 1, 'deadbeef', 'test')",
    )
    .bind(t)
    .bind(vec![7u8; 32])
    .bind(vec![1u8; 12])
    .execute(&e.owner)
    .await
    .unwrap();
    let w = World {
        e,
        t,
        job,
        im: person("u-im-a", &[]),
        obs: person("u-obs-a", &[]),
    };
    for op in ["order.read", "order.cancel"] {
        let (s, b) = send(
            &w.e.app,
            json(
                "PUT",
                &format!("/api/jobs/{JOB}/operations/{op}/routes/prod"),
                Some(&w.im),
                json!({"target": {"kind": "instance", "instance_id": "shop-prod"}}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{op}: {b}");
    }
    w
}

async fn set_override(w: &World, path: &str) {
    sqlx::query(
        "INSERT INTO field_mapping_override (tenant_id, instance_id, op, field, default_path, path, updated_by) \
         VALUES ($1, 'shop-prod', 'order.read', 'status', '$.status', $2, 'test')",
    )
    .bind(w.t)
    .bind(path)
    .execute(&w.e.owner)
    .await
    .unwrap();
}

/// (status, content-type, content-disposition, body text).
async fn export(w: &World, token: &str, query: &str) -> (StatusCode, String, String, String) {
    let req = Request::get(format!("/api/jobs/{JOB}/operations/spec?{query}"))
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let resp = w.e.app.clone().oneshot(req).await.expect("infallible");
    let status = resp.status();
    let header = |n: &str| {
        resp.headers()
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned()
    };
    let (ct, cd) = (header("content-type"), header("content-disposition"));
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    (status, ct, cd, String::from_utf8_lossy(&bytes).into_owned())
}

fn code(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["error"]["code"].as_str().map(str::to_owned))
        .unwrap_or_default()
}

#[tokio::test]
async fn spec_csv_and_md_are_the_rendered_spec() {
    let w = world().await;
    let mut tx = begin_scoped(&w.e.state.db, w.t).await.unwrap();
    let input = spec_export::load(&mut tx, &w.e.state.design, w.job, DesignOf::Draft)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(input.of, "draft");
    assert_eq!(input.instances.len(), 1, "only routed systems");

    let (s, ct, cd, body) = export(&w, &w.im, "format=csv&of=draft").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(ct, "text/csv; charset=utf-8");
    assert_eq!(
        cd,
        "attachment; filename=\"cca-integration-spec-draft.csv\""
    );
    assert_eq!(body, spec_export::spec_csv(&input));
    assert!(body.starts_with(
        "operation,side_effect,identity_level,direction,field,type,required,description,env,system,template,\
         mapping_path,mapping_source,status\r\n"
    ));
    assert!(
        body.contains(",prod,shop-prod (Shop),test_shop@3,$.status,default,"),
        "{body}"
    );

    let (s, ct, cd, body) = export(&w, &w.im, "format=md").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(ct, "text/markdown; charset=utf-8");
    assert_eq!(cd, "attachment; filename=\"cca-integration-spec-draft.md\"");
    assert_eq!(body, spec_export::spec_md(&input));
    assert!(body.starts_with("# Integration spec: cca (draft)\n"));
    assert!(body.contains("\n## order.read\n"));
}

#[tokio::test]
async fn spec_never_contains_url_or_secret() {
    let w = world().await;
    for q in ["format=csv", "format=md"] {
        let (s, _, _, body) = export(&w, &w.im, q).await;
        assert_eq!(s, StatusCode::OK, "{body}");
        for leak in [
            "https://",
            "http://",
            "secret-shop",
            ENV_ID_VALUE,
            "deadbeef",
            "service_token",
        ] {
            assert!(!body.contains(leak), "{q}: {leak} leaked");
        }
    }
}

#[tokio::test]
async fn csv_formula_cells_are_escaped() {
    let w = world().await;
    // the one tenant-typed cell, the mapping path, cannot start a formula: 0094 requires it to start with `$`
    // (the `'` prefix itself is unit-tested in spec_export::tests::csv_guards_formulas_and_quotes)
    let err = sqlx::query(
        "INSERT INTO field_mapping_override (tenant_id, instance_id, op, field, default_path, path, updated_by) \
         VALUES ($1, 'shop-prod', 'order.read', 'status', '$.status', '=HYPERLINK(\"x\")', 'test')",
    )
    .bind(w.t)
    .execute(&w.e.owner)
    .await
    .expect_err("path CHECK");
    assert_eq!(common::sqlstate(&err).as_deref(), Some("23514"), "{err}");
    set_override(&w, "$.state").await;
    let (s, _, _, body) = export(&w, &w.im, "format=csv").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(body.contains(",$.state,override,"), "{body}");
    assert!(
        !body.contains(",=") && !body.contains(",\"="),
        "an unescaped formula cell: {body}"
    );
}

#[tokio::test]
async fn live_without_release_409() {
    let w = world().await;
    let (s, _, _, body) = export(&w, &w.im, "format=csv&of=live").await;
    assert_eq!(
        (s, code(&body).as_str()),
        (StatusCode::CONFLICT, "no_release"),
        "{body}"
    );
}

#[tokio::test]
async fn draft_and_live_differ_after_unpublished_route_change() {
    let w = world().await;
    let f = design_contract::contract_job(w.t, w.job, JOB);
    design_contract::publishable(&w.e, &f).await;
    design_contract::publish(&w.e, &f).await;
    let (s, _, cd, live0) = export(&w, &w.im, "format=csv&of=live").await;
    assert_eq!(s, StatusCode::OK, "{live0}");
    assert!(cd.contains("integration-spec-live.csv"), "{cd}");
    let (_, _, _, draft0) = export(&w, &w.im, "format=csv&of=draft").await;
    assert_eq!(draft0, live0, "just published: Production = the draft");

    // unpublished: order.cancel loses its prod route in the draft only
    let (s, b) = send(
        &w.e.app,
        json(
            "DELETE",
            &format!("/api/jobs/{JOB}/operations/order.cancel/routes/prod"),
            Some(&w.im),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (_, _, _, draft) = export(&w, &w.im, "format=csv&of=draft").await;
    let (_, _, _, live) = export(&w, &w.im, "format=csv&of=live").await;
    assert_eq!(live, live0, "Production keeps the release's routes");
    assert_ne!(draft, live, "the draft moved");

    // each body is the renderer over its own design
    let reg = w.e.state.design.clone();
    let mut tx = begin_scoped(&w.e.state.db, w.t).await.unwrap();
    let d = spec_export::load(&mut tx, &reg, w.job, DesignOf::Draft)
        .await
        .unwrap();
    let l = spec_export::load(&mut tx, &reg, w.job, DesignOf::Live)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(draft, spec_export::spec_csv(&d));
    assert_eq!(live, spec_export::spec_csv(&l));
}

#[tokio::test]
async fn observer_can_export() {
    let w = world().await;
    let (s, _, _, body) = export(&w, &w.obs, "format=csv").await;
    assert_eq!(s, StatusCode::OK, "{body}");
    // a read: no audit row
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action NOT IN ('connector.route.set', 'job.draft_change')",
    )
    .fetch_one(&w.e.owner)
    .await
    .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn bad_format_422() {
    let w = world().await;
    for (q, want) in [
        ("format=xls", "invalid_format"),
        ("", "invalid_format"),
        ("format=csv&of=yesterday", "invalid_of"),
    ] {
        let (s, _, _, body) = export(&w, &w.im, q).await;
        assert_eq!(
            (s, code(&body).as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, want),
            "{q}: {body}"
        );
    }
}

#[tokio::test]
async fn other_tenant_job_404() {
    let w = world().await;
    let other = Uuid::new_v4();
    insert_tenant(&w.e.owner, other, "Tenant b").await;
    insert_user(&w.e.owner, "u-obs-b", other, "tenant_user").await;
    let (s, _, _, body) = export(&w, &person("u-obs-b", &[]), "format=csv").await;
    assert_eq!(
        (s, code(&body).as_str()),
        (StatusCode::NOT_FOUND, "job_not_found"),
        "{body}"
    );
    // and the owner's own read still works
    let (s, b) = send(
        &w.e.app,
        get(&format!("/api/jobs/{JOB}/operations"), Some(&w.obs)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}
