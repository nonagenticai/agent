//! Retire / restore a published template (migration 0228): a job template tag and a connector template version leave
//! the tenant choices for NEW jobs / connections and come back on restore; what already pins them keeps working.
//! super_admin only; one platform audit row per write.
use crate::common;

use axum::http::StatusCode;
use common::platform::{Op, offering, publish_template};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send, super_admin};
use serde_json::{Value, json};
use uuid::Uuid;

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn tenant(e: &TestEnv) -> Uuid {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    insert_user(&e.owner, "u-user", t, "tenant_user").await;
    t
}

async fn sa_post(e: &TestEnv, uri: &str) -> (StatusCode, Value) {
    send(&e.app, json("POST", uri, Some(&super_admin()), json!({}))).await
}

async fn audit(e: &TestEnv, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM platform_audit WHERE action = $1")
        .bind(action)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn offered(e: &TestEnv, template: &str) -> bool {
    let (s, b) = send(
        &e.app,
        get("/api/job-templates", Some(&person("u-user", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["templates"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["template"] == template)
}

async fn create_job(e: &TestEnv, template: &str) -> (StatusCode, Value) {
    send(
        &e.app,
        json(
            "POST",
            "/api/jobs",
            Some(&person("u-admin", &[])),
            json!({"template": template}),
        ),
    )
    .await
}

#[tokio::test]
async fn job_template_retire_hides_and_restore_returns() {
    let e = common::env().await;
    let o = offering(&e).await;
    tenant(&e).await;
    let template = o.template();
    let slug = o.job_layer.replace('/', ".");
    let base = format!("/api/platform/templates/{slug}/tags/v{}", o.version);

    // an existing job pins the template before it is retired
    let (s, b) = create_job(&e, &template).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert!(offered(&e, &template).await);

    // not a super_admin: 403
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &format!("{base}/retire"),
            Some(&person("u-admin", &[])),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let (s, b) = sa_post(&e, &format!("{base}/retire")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["retired"], true);
    assert_eq!(audit(&e, "job_template.retire").await, 1);
    let (s, b) = sa_post(&e, &format!("{base}/retire")).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "no_change");

    assert!(!offered(&e, &template).await);
    let (s, b) = create_job(&e, &template).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "template_retired");
    // the job that pins it still serves
    let (s, b) = send(
        &e.app,
        get(
            &format!("/api/jobs/{}", o.job),
            Some(&person("u-user", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");

    let (s, b) = sa_post(&e, &format!("{base}/restore")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["retired"], false);
    assert_eq!(audit(&e, "job_template.restore").await, 1);
    assert!(offered(&e, &template).await);
}

#[tokio::test]
async fn job_template_retire_unknown_is_404() {
    let e = common::env().await;
    let o = offering(&e).await;
    let slug = o.job_layer.replace('/', ".");
    let (s, b) = sa_post(
        &e,
        &format!("/api/platform/templates/{slug}/tags/v999/retire"),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "template_not_found");
}

async fn catalogue_has(e: &TestEnv, key: &str) -> bool {
    let (s, b) = send(
        &e.app,
        get("/api/connections/catalogue", Some(&person("u-user", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|x| x["key"] == key)
}

#[tokio::test]
async fn connector_template_retire_hides_and_restore_returns() {
    let e = common::env().await;
    tenant(&e).await;
    publish_template(&e, "retire_shop", "1", &[Op::star("order", "read")])
        .await
        .unwrap();
    let base = "/api/platform/connector-templates/retire_shop/1";
    assert!(catalogue_has(&e, "retire_shop").await);

    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &format!("{base}/retire"),
            Some(&person("u-admin", &[])),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let (s, b) = sa_post(&e, &format!("{base}/retire")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "retired");
    assert_eq!(audit(&e, "connector_template.retire").await, 1);
    let (s, b) = sa_post(&e, &format!("{base}/retire")).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "no_change");
    assert!(!catalogue_has(&e, "retire_shop").await);

    // a new connection on it is refused
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/connections/systems",
            Some(&person("u-admin", &[])),
            json!({"template_key": "retire_shop", "template_version": "1", "id": "shop1", "label": "Shop",
                   "env": "test", "stub": true}),
        ),
    )
    .await;
    assert_ne!(s, StatusCode::CREATED, "{b}");

    let (s, b) = sa_post(&e, &format!("{base}/restore")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "active");
    assert_eq!(audit(&e, "connector_template.restore").await, 1);
    assert!(catalogue_has(&e, "retire_shop").await);

    // a draft cannot be retired
    let (s, b) = sa_post(&e, "/api/platform/connector-templates/retire_shop/2/retire").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}
