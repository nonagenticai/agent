//! D124: the company profile (`GET|PUT /api/tenant/profile`, crate::routes::tenant_profile). Real Postgres 18.
use crate::common;

use axum::http::StatusCode;
use common::{get, insert_tenant, insert_user, json, person, send};
use serde_json::{Value, json};
use uuid::Uuid;

const URI: &str = "/api/tenant/profile";

/// Author a second industry (`healthcare` with department `nursing`) next to the offering's retail: the layers
/// and one setting each through the platform routes, then tagged so the selection options list them.
async fn second_industry(e: &common::TestEnv) {
    let sa = common::super_admin();
    let mut ids = Vec::new();
    for body in [
        json!({"scope": "industry", "industry": "healthcare", "label": "Healthcare"}),
        json!({"scope": "industry_dept", "industry": "healthcare", "department": "nursing",
               "label": "Nursing", "areas": ["triage"]}),
    ] {
        let (s, v) = send(&e.app, json("POST", "/api/layers", Some(&sa), body)).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        ids.push(v["id"].as_str().expect("layer id").to_string());
    }
    for (id, key, family) in [
        (&ids[0], "industry_default", "industry.healthcare"),
        (
            &ids[1],
            "department_default",
            "industry_dept.healthcare.nursing",
        ),
    ] {
        let (s, v) = send(
            &e.app,
            json(
                "POST",
                &format!("/api/layers/{id}/rows"),
                Some(&sa),
                json!({"kind": "job_setting", "key": key, "op": "add", "fields": {"label": key}}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        let (s, v) = send(
            &e.app,
            json(
                "POST",
                &format!("/api/platform/templates/{family}/tags"),
                Some(&sa),
                json!({}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
    }
}

/// A tenant with a tenant_admin `u-admin` and a tenant_user `u-user`; no job.
async fn tenant(e: &common::TestEnv) -> Uuid {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    insert_user(&e.owner, "u-user", t, "tenant_user").await;
    t
}

async fn put(e: &common::TestEnv, sub: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, json("PUT", URI, Some(&person(sub, &[])), body)).await
}

/// The first industry of `options` that is not `not`, its first two departments and the first region.
fn pick(opts: &Value, not: Option<&str>) -> (String, Vec<String>, String) {
    let industry = opts["industries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap())
        .find(|k| Some(*k) != not)
        .expect("an industry")
        .to_string();
    let departments: Vec<String> = opts["departments"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|d| d["industry"] == industry.as_str())
        .map(|d| d["key"].as_str().unwrap().to_string())
        .take(2)
        .collect();
    let region = opts["regions"][0]["key"]
        .as_str()
        .expect("a region")
        .to_string();
    (industry, departments, region)
}

#[tokio::test]
async fn member_reads_admin_sets_and_the_token_guards_the_write() {
    let e = common::env().await;
    let t = tenant(&e).await;
    let (s, v) = send(&e.app, get(URI, Some(&person("u-user", &[])))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["industry"], "unassigned");
    assert_eq!(v["departments"], json!([]));
    assert!(v["region"].is_null() && v["updated_at"].is_null(), "{v}");
    let (industry, departments, region) = pick(&v["options"], None);
    assert!(
        !departments.is_empty(),
        "the seed has departments for {industry}"
    );

    // a non-admin member is refused
    let body = json!({"industry": industry, "departments": departments, "region": region,
                      "description": "A shop", "expected_updated_at": null});
    let (s, w) = put(&e, "u-user", body.clone()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{w}");

    let (s, w) = put(&e, "u-admin", body.clone()).await;
    assert_eq!(s, StatusCode::OK, "{w}");
    assert_eq!(w["industry"], industry.as_str());
    assert_eq!(w["departments"], json!(departments));
    assert_eq!(w["region"], region.as_str());
    assert_eq!(w["description"], "A shop");
    let at = w["updated_at"].as_str().expect("updated_at").to_string();

    // a stale token (null: the profile was saved meanwhile) is 409; the stored one passes
    let (s, w) = put(&e, "u-admin", body.clone()).await;
    assert_eq!(s, StatusCode::CONFLICT, "{w}");
    assert_eq!(w["error"]["code"], "profile_changed", "{w}");
    let mut fresh = body;
    fresh["expected_updated_at"] = json!(at);
    fresh["region"] = Value::Null;
    let (s, w) = put(&e, "u-admin", fresh).await;
    assert_eq!(s, StatusCode::OK, "{w}");
    assert!(w["region"].is_null());

    // the member now reads it; the audit row is written
    let (_, v) = send(&e.app, get(URI, Some(&person("u-user", &[])))).await;
    assert_eq!(v["departments"], json!(departments));
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = 'tenant.profile_update'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn bad_department_industry_and_region_are_422() {
    let e = common::env().await;
    second_industry(&e).await;
    tenant(&e).await;
    let (_, v) = send(&e.app, get(URI, Some(&person("u-admin", &[])))).await;
    let (industry, departments, _) = pick(&v["options"], None);
    let (other, other_deps, _) = pick(&v["options"], Some(&industry));
    for (body, code) in [
        (
            json!({"industry": industry, "departments": ["industry_dept/nope/none"], "region": null}),
            "department_not_in_industry",
        ),
        (
            json!({"industry": other, "departments": departments, "region": null}),
            "department_not_in_industry",
        ),
        (
            json!({"industry": "unassigned", "departments": other_deps, "region": null}),
            "department_not_in_industry",
        ),
        (
            json!({"industry": "no_such_industry", "departments": [], "region": null}),
            "unknown_industry",
        ),
        (
            json!({"industry": industry, "departments": departments, "region": "atlantis"}),
            "unknown_region",
        ),
    ] {
        let (s, w) = put(&e, "u-admin", body).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{code}: {w}");
        assert_eq!(w["error"]["code"], code, "{w}");
    }
}

#[tokio::test]
async fn changing_the_industry_with_a_job_of_another_industry_is_409() {
    let e = common::env().await;
    second_industry(&e).await;
    let t = tenant(&e).await;
    let (_, v) = send(&e.app, get(URI, Some(&person("u-admin", &[])))).await;
    let (other, _, _) = pick(&v["options"], Some("retail"));
    // a job pinned to the retail industry layer (the pin every job carries, D106)
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) \
         VALUES ($1, 'care', 'Care', '[{\"layer\":\"industry/retail\",\"version\":1}]', 'test')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, w) = put(
        &e,
        "u-admin",
        json!({"industry": other, "departments": [], "region": null}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{w}");
    assert_eq!(w["error"]["code"], "industry_in_use", "{w}");
    // the same industry as the job's is fine
    let (s, w) = put(
        &e,
        "u-admin",
        json!({"industry": "retail", "departments": [], "region": null}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{w}");
}

#[tokio::test]
async fn languages_read_set_and_validated() {
    let e = common::env().await;
    let t = tenant(&e).await;
    let (_, v) = send(&e.app, get(URI, Some(&person("u-user", &[])))).await;
    assert_eq!(v["languages"], json!(["en"]), "{v}");
    let base = json!({"industry": "unassigned", "departments": [], "region": null,
                      "expected_updated_at": null});
    // set (trimmed, deduplicated, kept in order)
    let mut body = base.clone();
    body["languages"] = json!(["nl", " en", "nl"]);
    let (s, w) = put(&e, "u-admin", body).await;
    assert_eq!(s, StatusCode::OK, "{w}");
    assert_eq!(w["languages"], json!(["nl", "en"]));
    let stored: Vec<String> = sqlx::query_scalar("SELECT languages FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(stored, ["nl", "en"]);
    let at = w["updated_at"].as_str().expect("updated_at").to_string();
    // the audit row carries old and new languages
    let d: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'tenant.profile_update'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(d["old"]["languages"], json!(["en"]), "{d}");
    assert_eq!(d["new"]["languages"], json!(["nl", "en"]), "{d}");
    // absent keeps them; invalid ones are refused
    let mut keep = base.clone();
    keep["expected_updated_at"] = json!(at);
    let (s, w) = put(&e, "u-admin", keep.clone()).await;
    assert_eq!(s, StatusCode::OK, "{w}");
    assert_eq!(w["languages"], json!(["nl", "en"]));
    for (bad, code) in [
        (json!(["xx"]), "unknown_language"),
        (json!([]), "invalid_body"),
    ] {
        let mut b = keep.clone();
        b["languages"] = bad;
        let (s, w) = put(&e, "u-admin", b).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{w}");
        assert_eq!(w["error"]["code"], code);
    }
}
