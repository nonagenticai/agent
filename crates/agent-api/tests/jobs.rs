//! B1 T11: jobs, the job board, templates and job creation (crate::routes::{jobs,templates}). Real Postgres 18 (one
//! container for this binary); the template tags are the test offering's (`common::platform`), authored through the platform routes.
use crate::common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{get, insert_tenant, insert_user, json, person, send, super_admin};
use serde_json::{Value, json};
use uuid::Uuid;

fn in_tenant(mut r: Request<Body>, t: Uuid) -> Request<Body> {
    r.headers_mut()
        .insert("x-active-tenant-id", t.to_string().parse().unwrap());
    r
}

/// A tenant with a tenant_admin `u-admin` and a tenant_user `u-user`.
async fn tenant(e: &common::TestEnv) -> Uuid {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    insert_user(&e.owner, "u-user", t, "tenant_user").await;
    t
}

/// `POST /api/jobs` as the super_admin acting in `t` (a super_admin in the tenant holds tenant_admin). A body in the
/// pre-D123 shape `{layer, version, label?}` is sent as `{template: "layer@vN", label?}`.
async fn create(e: &common::TestEnv, t: Uuid, body: Value) -> (StatusCode, Value) {
    let body = match (body["layer"].as_str(), body["version"].as_i64()) {
        (Some(l), Some(v)) => {
            let mut b = json!({"template": format!("{l}@v{v}")});
            if let Some(label) = body.get("label") {
                b["label"] = label.clone();
            }
            b
        }
        _ => body,
    };
    send(
        &e.app,
        in_tenant(json("POST", "/api/jobs", Some(&super_admin()), body), t),
    )
    .await
}

/// `POST /api/jobs` as `sub` (a member of the tenant; the tenant is the caller's own).
async fn create_as(e: &common::TestEnv, sub: &str, body: Value) -> (StatusCode, Value) {
    send(
        &e.app,
        json("POST", "/api/jobs", Some(&person(sub, &[])), body),
    )
    .await
}

#[tokio::test]
async fn board_shows_newer_tag_and_request() {
    let e = common::env().await;
    let t = tenant(&e).await;
    let (s, body) = create(
        &e,
        t,
        json!({"layer": "job/customer_care_agent", "version": 3}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(body["key"], "customer_care_agent");
    let admin = person("u-admin", &[]);
    let (s, body) = send(&e.app, get("/api/jobs?view=board", Some(&admin))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let row = &body["items"][0];
    assert_eq!(row["job"]["key"], "customer_care_agent");
    // the job was created at the offering's v3; the newest tag (v4) differs only in the job layer's own pin
    assert_eq!(
        row["newer"],
        json!([{"layer": "job/customer_care_agent", "version": 4}]),
        "only the pins the job does not have yet"
    );
    assert!(row["upgrade_requested_at"].is_null());
    // the test offering requires no operation, knowledge or slot: nothing blocks (ready = no missing subject)
    assert_eq!(row["readiness"]["ready"], true, "{row}");
    assert_eq!(row["readiness"]["missing"], json!([]), "{row}");
    assert_eq!(row["cases_7d"], 0);

    let (s, body) = send(
        &e.app,
        json(
            "POST",
            "/api/jobs/customer_care_agent/upgrade-request",
            Some(&admin),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert!(body["upgrade_requested_at"].is_string(), "{body}");
    let (_, body) = send(&e.app, get("/api/jobs?view=board", Some(&admin))).await;
    assert!(body["items"][0]["upgrade_requested_at"].is_string());
    let (from, to): (Value, Value) = sqlx::query_as(
        "SELECT from_pins, to_pins FROM template_upgrade_request WHERE tenant_id = $1",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(from.as_array().unwrap().len(), 4);
    assert!(
        to.to_string()
            .contains("{\"layer\":\"job/customer_care_agent\",\"version\":4}"),
        "the newest pins (the job layer at v4): {to}"
    );
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = 'job.upgrade_request' \
           AND actor_id = 'u-admin' AND job = 'customer_care_agent'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(n, 1);
    // the plain list is JobSummary rows
    let (_, body) = send(&e.app, get("/api/jobs", Some(&admin))).await;
    assert_eq!(body["items"][0]["key"], "customer_care_agent");
    assert!(body["items"][0].get("newer").is_none());
    let (s, body) = send(&e.app, get("/api/jobs?view=nope", Some(&admin))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "invalid_view");
}

/// A second industry's job template, authored through the platform routes: `industry/healthcare`,
/// `industry_dept/healthcare/nursing` and `job/triage_agent` (created before any tag so the department projects no
/// template of its own), each tagged once. Returns the template as `(layer, version)`.
async fn healthcare_template(e: &common::TestEnv) -> (&'static str, i64) {
    let ind = layer(
        e,
        json!({"scope": "industry", "industry": "healthcare", "label": "Healthcare"}),
        "industry/healthcare",
    )
    .await;
    let dept = layer(
        e,
        json!({"scope": "industry_dept", "industry": "healthcare", "department": "nursing", "label": "Nursing"}),
        "industry_dept/healthcare/nursing",
    )
    .await;
    let job = layer(
        e,
        json!({"scope": "job", "job": "triage_agent", "label": "Triage agent",
               "template_selection": {"profile": null, "industry": "healthcare",
                                      "departments": ["industry_dept/healthcare/nursing"]}}),
        "job/triage_agent",
    )
    .await;
    publish(e, "industry/healthcare", "industry.healthcare", &ind).await;
    publish(
        e,
        "industry_dept/healthcare/nursing",
        "industry_dept.healthcare.nursing",
        &dept,
    )
    .await;
    publish(e, "job/triage_agent", "job.triage_agent", &job).await;
    ("job/triage_agent", 1)
}

#[tokio::test]
async fn create_from_template_pins_the_tag() {
    let e = common::env().await;
    let t = tenant(&e).await;
    let (s, body) = create(
        &e,
        t,
        json!({"layer": "job/customer_care_agent", "version": 4, "label": "Care desk"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    assert_eq!(body["key"], "customer_care_agent");
    assert_eq!(body["label"], "Care desk");
    assert_eq!(body["tenant_id"], t.to_string());
    assert_eq!(body["on_bus"], false);
    assert_eq!(body["draft_rev"], 0);
    let tag_pins: Value = sqlx::query_scalar(
        "SELECT pins FROM job_template_tag WHERE layer = 'job/customer_care_agent' AND version = 4",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(body["pins"], tag_pins);
    let industry: String = sqlx::query_scalar("SELECT industry FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(industry, "retail", "the tag's industry, while unassigned");
    let (layer, version): (Option<String>, Option<i32>) = sqlx::query_as(
        "SELECT template_layer, template_version FROM jobs WHERE tenant_id = $1 AND key = 'customer_care_agent'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(layer.as_deref(), Some("job/customer_care_agent"));
    assert_eq!(version, Some(4));
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'job.create'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(details["template"], "job/customer_care_agent@v4");
    let (s, body) = send(
        &e.app,
        get(
            "/api/jobs/customer_care_agent",
            Some(&person("u-user", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["key"], "customer_care_agent");
    let (s, body) = send(&e.app, get("/api/jobs/nope", Some(&person("u-user", &[])))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "job_not_found");
    // a template of another industry than the company's (D123.3) is refused; the industry stays
    let (other, other_version) = healthcare_template(&e).await;
    let (s, b) = create(&e, t, json!({"layer": other, "version": other_version})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "industry_mismatch");
    let industry: String = sqlx::query_scalar("SELECT industry FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(industry, "retail");
}

/// A draft template tag (a generator run in progress): only the projection writes these, so the owner pool does.
async fn insert_draft_tag(e: &common::TestEnv) {
    // the draft tag is the provisional v0 of a job family the platform authored (the tag row's FK)
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/layers",
            Some(&super_admin()),
            json!({"scope": "job", "job": "triage_agent", "label": "Triage agent",
                   "template_selection": {"profile": "retail_ecommerce", "industry": "retail",
                                          "departments": ["industry_dept/retail/customer_service"]}}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    sqlx::query(
        "INSERT INTO layer_tag (family, version, content_hash, provisional, tagged_by) \
         VALUES ('job/triage_agent', 0, 'sha256:' || repeat('0', 64), true, 'test')",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO job_template_tag (layer, version, key, label, industry, pins, status, created_by) \
         VALUES ('job/triage_agent', 0, 'triage_agent', 'Triage agent (generator run in progress)', 'healthcare', \
                 '[]', 'draft', 'test')",
    )
    .execute(&e.owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn draft_tag_is_409_template_not_tagged() {
    let e = common::env().await;
    let t = tenant(&e).await;
    insert_draft_tag(&e).await;
    let (s, body) = create(&e, t, json!({"layer": "job/triage_agent", "version": 0})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "template_not_tagged");
    let (s, body) = create(&e, t, json!({"layer": "job/none", "version": 1})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "template_not_found");
    let (s, body) = create(
        &e,
        t,
        json!({"layer": "job/customer_care_agent", "version": 4, "label": "  "}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "invalid_label");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn duplicate_key_409_job_exists() {
    let e = common::env().await;
    let t = tenant(&e).await;
    let body = json!({"layer": "job/customer_care_agent", "version": 4});
    assert_eq!(create(&e, t, body.clone()).await.0, StatusCode::CREATED);
    // D123.1: the default key is taken, so the next free one is chosen; an explicit taken key is 409
    let (s, b) = create(&e, t, body).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["key"], "customer_care_agent_2");
    let (s, b) = create(&e, t, json!({"template": "job/customer_care_agent@v4"})).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["key"], "customer_care_agent_3");
    let (s, b) = create(
        &e,
        t,
        json!({"template": "job/customer_care_agent@v4", "key": "customer_care_agent"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "job_exists");
    let (s, b) = create(
        &e,
        t,
        json!({"template": "job/customer_care_agent@v4", "key": "Bad Key"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_job_key");
    // the same key in another tenant is fine
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, "Beta").await;
    let (s, b) = create(
        &e,
        other,
        json!({"layer": "job/customer_care_agent", "version": 3}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["key"], "customer_care_agent");
}

#[tokio::test]
async fn platform_create_bad_template_form_is_422() {
    let e = common::env().await;
    let t = tenant(&e).await;
    let uri = format!("/api/tenants/{t}/jobs");
    let post = |body: Value| json("POST", &uri, Some(&super_admin()), body);
    for template in [
        "job/customer_care_agent",
        "job/customer_care_agent@4",
        "@v4",
    ] {
        let (s, b) = send(
            &e.app,
            post(json!({"template": template, "key": "care", "label": "Care"})),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{template}: {b}");
        assert_eq!(b["error"]["code"], "invalid_template");
    }
    let (s, b) = send(
        &e.app,
        post(json!({"template": "job/customer_care_agent@v4", "key": "Care!", "label": "Care"})),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b["error"]["code"], "invalid_job_key");
    let (s, b) = send(
        &e.app,
        post(
            json!({"template": "job/customer_care_agent@v4", "key": "care_desk", "label": "Care"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["key"], "care_desk");
    let actor: (String, bool) = sqlx::query_as(
        "SELECT actor_id, platform_actor FROM audit_event WHERE tenant_id = $1 AND action = 'job.create'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(actor, ("u-super".to_string(), true));
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenants/{}/jobs", Uuid::new_v4()),
            Some(&super_admin()),
            json!({"template": "job/customer_care_agent@v4", "key": "care_desk", "label": "Care"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(b["error"]["code"], "tenant_not_found");
    let (s, _) = send(
        &e.app,
        json(
            "POST",
            &uri,
            Some(&person("u-admin", &[])),
            json!({"template": "job/customer_care_agent@v4", "key": "x_y", "label": "X"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn archived_tenant_create_job_409() {
    let e = common::env().await;
    let t = tenant(&e).await;
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenants/{t}/jobs"),
            Some(&super_admin()),
            json!({"template": "job/customer_care_agent@v4", "key": "care", "label": "Care"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "tenant_archived");
}

#[tokio::test]
async fn upgrade_request_twice_409() {
    let e = common::env().await;
    let t = tenant(&e).await;
    create(
        &e,
        t,
        json!({"layer": "job/customer_care_agent", "version": 3}),
    )
    .await;
    let req = || {
        json(
            "POST",
            "/api/jobs/customer_care_agent/upgrade-request",
            Some(&person("u-admin", &[])),
            json!({}),
        )
    };
    assert_eq!(send(&e.app, req()).await.0, StatusCode::OK);
    let (s, b) = send(&e.app, req()).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "upgrade_already_requested");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM template_upgrade_request")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn no_newer_409() {
    let e = common::env().await;
    let t = tenant(&e).await;
    // the newest customer-care template (seed v8's ccag@v8, queried so the next seed does not break this)
    let (layer, newest) = common::design_contract::latest_template(&e).await;
    create(&e, t, json!({"layer": layer, "version": newest})).await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/jobs/customer_care_agent/upgrade-request",
            Some(&person("u-admin", &[])),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "no_newer_template");
    let (_, body) = send(
        &e.app,
        get("/api/jobs?view=board", Some(&person("u-admin", &[]))),
    )
    .await;
    assert_eq!(body["items"][0]["newer"], json!([]));
}

#[tokio::test]
async fn tenant_user_cannot_request_upgrade() {
    let e = common::env().await;
    let t = tenant(&e).await;
    create(
        &e,
        t,
        json!({"layer": "job/customer_care_agent", "version": 3}),
    )
    .await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/jobs/customer_care_agent/upgrade-request",
            Some(&person("u-user", &[])),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "forbidden");
    // creating a job is a tenant_admin's (D123): a job_designer alone and an observer are refused
    insert_user(&e.owner, "u-designer", t, "job_designer").await;
    for sub in ["u-designer", "u-user"] {
        let (s, b) = create_as(&e, sub, json!({"template": "job/customer_care_agent@v4"})).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{sub}: {b}");
        assert_eq!(b["error"]["code"], "forbidden");
        assert_eq!(b["error"]["missing_role"], "tenant_admin");
    }
}

#[tokio::test]
async fn tenant_admin_creates_jobs_and_narrows_departments() {
    let e = common::env().await;
    let t = tenant(&e).await;
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"template": "job/customer_care_agent@v4", "label": "Care"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["key"], "customer_care_agent");
    assert_eq!(b["label"], "Care");
    // the first job sets the unassigned industry
    let industry: String = sqlx::query_scalar("SELECT industry FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(industry, "retail");
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"template": "job/customer_care_agent@v4"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["key"], "customer_care_agent_2");
    // the audit row records template, key, departments and the role
    let (details,): (Value,) = sqlx::query_as(
        "SELECT details FROM audit_event WHERE action = 'job.create' AND resource_id = 'customer_care_agent_2'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(details["template"], "job/customer_care_agent@v4");
    assert_eq!(details["key"], "customer_care_agent_2");
    assert_eq!(details["by_role"], "tenant_admin");
    assert!(details["departments"].is_array(), "{details}");
    // departments must be a non-empty subset of the template's
    for bad in [json!([]), json!(["industry_dept/retail/nope"])] {
        let (s, b) = create_as(
            &e,
            "u-admin",
            json!({"template": "job/customer_care_agent@v4", "departments": bad}),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(b["error"]["code"], "departments_not_in_template");
    }
    let dept = "industry_dept/retail/customer_service";
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"template": "job/customer_care_agent@v4", "key": "care_cs", "departments": [dept]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let selection: Value = sqlx::query_scalar(
        "SELECT t.selection FROM job_target t JOIN jobs j ON j.id = t.job_id WHERE j.key = 'care_cs'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(selection["departments"], json!([dept]));
}

#[tokio::test]
async fn job_templates_catalog_for_a_member() {
    let e = common::env().await;
    let t = tenant(&e).await;
    let (s, b) = send(
        &e.app,
        get("/api/job-templates", Some(&person("u-user", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let list = b["templates"].as_array().unwrap();
    assert!(!list.is_empty());
    assert!(
        list.iter().all(|x| x["recommended"] == false),
        "no company profile yet"
    );
    let ccag = list
        .iter()
        .find(|x| x["template"] == "job/customer_care_agent@v4")
        .expect("a tagged template is listed");
    assert_eq!(ccag["key"], "customer_care_agent");
    assert_eq!(ccag["industry"], "retail");
    assert!(
        ccag["departments"]
            .as_array()
            .is_some_and(|d| !d.is_empty())
    );
    assert!(
        !list
            .iter()
            .any(|x| x["template"] == "job/it_service_desk_agent@v0"),
        "a draft is not offered"
    );
    // the company profile (industry + a department) recommends and sorts first
    sqlx::query(
        "UPDATE tenants SET industry = 'retail', departments = '{industry_dept/retail/customer_service}' WHERE id = $1",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = send(
        &e.app,
        get("/api/job-templates", Some(&person("u-user", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let list = b["templates"].as_array().unwrap();
    assert_eq!(list[0]["recommended"], true, "{}", list[0]);
    let first_other = list.iter().position(|x| x["recommended"] == false);
    assert!(
        first_other.is_none_or(|i| list[i..].iter().all(|x| x["recommended"] == false)),
        "recommended sort first"
    );
    assert!(
        list.iter()
            .filter(|x| x["recommended"] == true)
            .all(|x| x["industry"] == "retail")
    );
}

#[tokio::test]
async fn patch_job_label() {
    let e = common::env().await;
    let t = tenant(&e).await;
    create(
        &e,
        t,
        json!({"layer": "job/customer_care_agent", "version": 4}),
    )
    .await;
    let patch = |sub: &str, body: Value| {
        json(
            "PATCH",
            "/api/jobs/customer_care_agent",
            Some(&person(sub, &[])),
            body,
        )
    };
    let (s, b) = send(&e.app, patch("u-user", json!({"label": "x"}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "forbidden");
    let (s, b) = send(&e.app, patch("u-admin", json!({}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "nothing_to_update");
    let (s, b) = send(&e.app, patch("u-admin", json!({"label": "  "}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_label");
    let (s, b) = send(&e.app, patch("u-admin", json!({"label": " Front desk "}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["label"], "Front desk");
    let (s, b) = send(
        &e.app,
        get(
            "/api/jobs/customer_care_agent",
            Some(&person("u-user", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["label"], "Front desk");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'job.update' AND tenant_id = $1",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn job_audience_defaults_and_is_settable() {
    let e = common::env().await;
    tenant(&e).await;
    publish_offering(&e).await;
    let dept = "industry_dept/tjd_ind/tjd_dept";
    // default: customers
    let body = json!({"key": "front_desk", "label": "Front desk", "industry": "tjd_ind", "departments": [dept]});
    let (s, b) = create_as(&e, "u-admin", body).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["audience"], "customers");
    // blank create with employees
    let body = json!({"key": "it_desk", "label": "IT desk", "industry": "tjd_ind", "departments": [dept], "audience": "employees"});
    let (s, b) = create_as(&e, "u-admin", body).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["audience"], "employees");
    let stored: String = sqlx::query_scalar("SELECT audience FROM jobs WHERE key = 'it_desk'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(stored, "employees");
    // invalid on create
    let body = json!({"key": "bad_aud", "label": "Bad", "industry": "tjd_ind", "departments": [dept], "audience": "staff"});
    let (s, b) = create_as(&e, "u-admin", body).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_audience");
    // PATCH: audience alone, then invalid
    let patch = |body: Value| {
        json(
            "PATCH",
            "/api/jobs/front_desk",
            Some(&person("u-admin", &[])),
            body,
        )
    };
    let (s, b) = send(&e.app, patch(json!({"audience": "employees"}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["audience"], "employees");
    let (s, b) = send(&e.app, patch(json!({"audience": "nobody"}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_audience");
}

#[tokio::test]
async fn templates_need_super_admin() {
    let e = common::env().await;
    tenant(&e).await;
    insert_draft_tag(&e).await;
    let (s, b) = send(
        &e.app,
        get("/api/templates?kind=job", Some(&person("u-admin", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(b["error"]["code"], "super_admin_required");
    let (s, b) = send(&e.app, get("/api/templates?kind=job", Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let tags = b.as_array().unwrap();
    assert_eq!(
        tags.len(),
        5,
        "the offering's four tagged versions of the customer care job layer + the draft inserted above"
    );
    let draft = tags
        .iter()
        .find(|t| t["layer"] == "job/triage_agent" && t["version"] == 0)
        .unwrap();
    assert_eq!(draft["status"], "draft");
    for t in tags {
        assert!(t["key"].is_string() && t["pins"].is_array() && t["label"].is_string());
    }
    let (s, b) = send(
        &e.app,
        get("/api/templates?kind=layer", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(b["error"]["code"], "invalid_kind");
}

// ---- D128: a blank job on the platform's published layers (no job template) ----------------------------------------

/// `POST` as the super_admin (platform authoring).
async fn sa_post(e: &common::TestEnv, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, json("POST", uri, Some(&super_admin()), body)).await
}

/// Tag `family` (dotted slug `slug`) once: one setting row, then the tag. Skips a family that already has a tag.
async fn publish(e: &common::TestEnv, family: &str, slug: &str, layer_id: &str) {
    let tagged: i64 =
        sqlx::query_scalar("SELECT count(*) FROM layer_tag WHERE family = $1 AND NOT provisional")
            .bind(family)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    if tagged > 0 {
        return;
    }
    let (s, b) = sa_post(
        e,
        &format!("/api/layers/{layer_id}/rows"),
        json!({"kind": "job_setting", "key": format!("tjd_{}", slug.replace('.', "_")), "op": "add",
               "fields": {"label": "Published offering"}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{family}: {b}");
    let (s, b) = sa_post(
        e,
        &format!("/api/platform/templates/{slug}/tags"),
        json!({"note": "tjd"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{family}: {b}");
}

/// A layer by key, created through `POST /api/layers` (an existing one is reused: 409 `layer_exists`).
async fn layer(e: &common::TestEnv, body: Value, key: &str) -> String {
    let (s, b) = sa_post(e, "/api/layers", body).await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::CONFLICT,
        "{key}: {s} {b}"
    );
    sqlx::query_scalar::<_, uuid::Uuid>("SELECT id FROM layer WHERE key = $1")
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap()
        .to_string()
}

/// Publishes base, `industry/tjd_ind` and `industry_dept/tjd_ind/tjd_dept` (tagged) plus the untagged
/// `industry_dept/tjd_ind/tjd_draft`, all through the authoring routes.
async fn publish_offering(e: &common::TestEnv) {
    let base = layer(e, json!({"scope": "base", "label": "Base"}), "base").await;
    // the profile is part of the published base (a compile reads tagged rows only)
    profile(e, &base, "tjd_profile", Some("tjd_ind")).await;
    // common::env() already tagged base before this profile was authored.
    retag(e, "base").await;
    let ind = layer(
        e,
        json!({"scope": "industry", "industry": "tjd_ind", "label": "Tjd industry"}),
        "industry/tjd_ind",
    )
    .await;
    publish(e, "industry/tjd_ind", "industry.tjd_ind", &ind).await;
    let dept = layer(
        e,
        json!({"scope": "industry_dept", "industry": "tjd_ind", "department": "tjd_dept", "label": "Tjd department"}),
        "industry_dept/tjd_ind/tjd_dept",
    )
    .await;
    publish(
        e,
        "industry_dept/tjd_ind/tjd_dept",
        "industry_dept.tjd_ind.tjd_dept",
        &dept,
    )
    .await;
    layer(
        e,
        json!({"scope": "industry_dept", "industry": "tjd_ind", "department": "tjd_draft", "label": "Tjd draft"}),
        "industry_dept/tjd_ind/tjd_draft",
    )
    .await;
}

/// A `profile` row in the base layer (D132.1); an existing one is reused (409 `duplicate_add`).
async fn profile(e: &common::TestEnv, base: &str, key: &str, industry: Option<&str>) {
    let (s, b) = sa_post(
        e,
        &format!("/api/layers/{base}/rows"),
        json!({"kind": "profile", "key": key, "op": "add",
               "fields": {"label": key, "industry": industry, "selection": {"order_type": ["discrete"]}}}),
    )
    .await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::CONFLICT,
        "{key}: {s} {b}"
    );
}

/// Tags `slug` again (a new version holding the rows added since its last tag), e.g. profiles authored after the
/// first publish: options offer only tagged profiles.
async fn retag(e: &common::TestEnv, slug: &str) {
    let (s, b) = sa_post(
        e,
        &format!("/api/platform/templates/{slug}/tags"),
        json!({"note": "tjd retag"}),
    )
    .await;
    assert!(s.is_success(), "retag {slug}: {s} {b}");
}

/// Publishes industry `ind` with one department `<ind>_dept`; returns the base layer id and the department key.
async fn publish_industry(e: &common::TestEnv, ind: &str) -> (String, String) {
    let base = layer(e, json!({"scope": "base", "label": "Base"}), "base").await;
    publish(e, "base", "base", &base).await;
    let il = layer(
        e,
        json!({"scope": "industry", "industry": ind, "label": ind}),
        &format!("industry/{ind}"),
    )
    .await;
    publish(
        e,
        &format!("industry/{ind}"),
        &format!("industry.{ind}"),
        &il,
    )
    .await;
    let d = format!("{ind}_dept");
    let dl = layer(
        e,
        json!({"scope": "industry_dept", "industry": ind, "department": d, "label": d}),
        &format!("industry_dept/{ind}/{d}"),
    )
    .await;
    publish(
        e,
        &format!("industry_dept/{ind}/{d}"),
        &format!("industry_dept.{ind}.{d}"),
        &dl,
    )
    .await;
    (base, format!("industry_dept/{ind}/{d}"))
}

async fn langs(e: &common::TestEnv, key: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT languages FROM jobs WHERE key = $1")
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn stored_selection(e: &common::TestEnv, job: &str) -> Value {
    sqlx::query_scalar(
        "SELECT t.selection FROM job_target t JOIN jobs j ON j.id = t.job_id WHERE j.key = $1",
    )
    .bind(job)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn blank_job_gets_the_industrys_single_profile() {
    let e = common::env().await;
    tenant(&e).await;
    publish_offering(&e).await;
    let dept = "industry_dept/tjd_ind/tjd_dept";
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "prof_default", "label": "P", "industry": "tjd_ind", "departments": [dept]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(
        stored_selection(&e, "prof_default").await["profile"],
        "tjd_profile"
    );
    // an explicit known profile is kept
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "prof_explicit", "label": "P", "industry": "tjd_ind", "departments": [dept],
               "profile": "tjd_profile"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(
        stored_selection(&e, "prof_explicit").await["profile"],
        "tjd_profile"
    );
    // an unknown one is refused
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "prof_unknown", "label": "P", "industry": "tjd_ind", "departments": [dept],
               "profile": "no_such_profile"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "unknown_profile");
}

#[tokio::test]
async fn blank_job_without_a_single_profile_is_profile_required() {
    let e = common::env().await;
    tenant(&e).await;
    // no profile for the industry (and no industry-less profile exists on the platform)
    let (_, bare_dept) = publish_industry(&e, "tjd_bare").await;
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "prof_none", "label": "N", "industry": "tjd_bare", "departments": [bare_dept]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "profile_required", "{b}");
    // two profiles of the industry: ambiguous, the message lists both
    let (base, amb_dept) = publish_industry(&e, "tjd_amb").await;
    profile(&e, &base, "tjd_amb_a", Some("tjd_amb")).await;
    profile(&e, &base, "tjd_amb_b", Some("tjd_amb")).await;
    retag(&e, "base").await;
    let body =
        json!({"key": "prof_amb", "label": "A", "industry": "tjd_amb", "departments": [amb_dept]});
    let (s, b) = create_as(&e, "u-admin", body.clone()).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "profile_required");
    let m = b["error"]["message"].as_str().unwrap();
    assert!(m.contains("tjd_amb_a") && m.contains("tjd_amb_b"), "{m}");
    // choosing one resolves it
    let mut chosen = body;
    chosen["profile"] = json!("tjd_amb_b");
    let (s, b) = create_as(&e, "u-admin", chosen).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(
        stored_selection(&e, "prof_amb").await["profile"],
        "tjd_amb_b"
    );
}

#[tokio::test]
async fn job_languages_default_from_the_company_and_can_be_chosen() {
    let e = common::env().await;
    let t = tenant(&e).await;
    publish_offering(&e).await;
    let dept = "industry_dept/tjd_ind/tjd_dept";
    // the column default is {en}
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "lang_default", "label": "L", "industry": "tjd_ind", "departments": [dept]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(langs(&e, "lang_default").await, ["en"]);
    // the company's languages apply when none are requested
    sqlx::query("UPDATE tenants SET languages = '{nl,en}' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "lang_company", "label": "L", "industry": "tjd_ind", "departments": [dept]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(langs(&e, "lang_company").await, ["nl", "en"]);
    // explicit languages win (deduplicated)
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "lang_explicit", "label": "L", "industry": "tjd_ind", "departments": [dept],
               "languages": ["de", "de", "fr"]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(langs(&e, "lang_explicit").await, ["de", "fr"]);
    // an unknown or empty list is refused
    for (bad, code) in [
        (json!(["xx"]), "unknown_language"),
        (json!([]), "invalid_body"),
    ] {
        let (s, b) = create_as(
            &e,
            "u-admin",
            json!({"key": "lang_bad", "label": "L", "industry": "tjd_ind", "departments": [dept],
                   "languages": bad}),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(b["error"]["code"], code);
    }
}

#[tokio::test]
async fn operations_of_a_blank_job_compile_from_its_selection() {
    let e = common::env().await;
    tenant(&e).await;
    publish_offering(&e).await;
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "ops_blank", "label": "O", "industry": "tjd_ind",
               "departments": ["industry_dept/tjd_ind/tjd_dept"]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = send(
        &e.app,
        get(
            "/api/jobs/ops_blank/operations",
            Some(&person("u-admin", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

/// Pins the SERVER side of the create drawer's template body (web `d132-web.test.ts` pins the keys the drawer sends):
/// template, key, label, departments, profile, region, languages, audience. Used to 400 on `region` (untagged
/// `CreateBody` matched neither variant).
#[tokio::test]
async fn drawer_template_body_with_region_is_created_and_shows_the_region() {
    let e = common::env().await;
    tenant(&e).await;
    sqlx::query("INSERT INTO dimension (key, label, kind) VALUES ('region', 'Region', 'placeholder') ON CONFLICT DO NOTHING")
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query("INSERT INTO dimension_value (dimension, key, label, added_by) VALUES ('region', 'eu', 'EU', 'sa') ON CONFLICT DO NOTHING")
        .execute(&e.owner)
        .await
        .unwrap();
    publish_offering(&e).await;
    let dept = "industry_dept/retail/customer_service";
    // EXACTLY the drawer's template body (web/src/routes/admin/jobs/+page.svelte `create`)
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({
            "template": "job/customer_care_agent@v4",
            "key": "drawer_job",
            "label": "Drawer job",
            "departments": [dept],
            "profile": "tjd_profile",
            "region": "eu",
            "languages": ["en"],
            "audience": "customers",
        }),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["region"], "eu", "{b}");
    let sel = stored_selection(&e, "drawer_job").await;
    assert_eq!(
        (&sel["region"], &sel["profile"]),
        (&json!("eu"), &json!("tjd_profile"))
    );
    // the drawer omits profile/key/region when empty: still 201, no region
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({
            "template": "job/customer_care_agent@v4",
            "label": "Drawer job 2",
            "departments": [dept],
            "languages": ["en"],
            "audience": "customers",
        }),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert!(b["region"].is_null(), "{b}");
    // an unknown region is refused like the blank create
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"template": "job/customer_care_agent@v4", "key": "drawer_job_3", "region": "atlantis"}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "unknown_region");
}

#[tokio::test]
async fn tenant_admin_creates_a_blank_job_on_published_layers() {
    let e = common::env().await;
    let t = tenant(&e).await;
    publish_offering(&e).await;
    let dept = "industry_dept/tjd_ind/tjd_dept";
    let body = json!({"key": "blank_one", "label": "Blank one", "industry": "tjd_ind", "departments": [dept]});
    let (s, b) = create_as(&e, "u-admin", body.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["key"], "blank_one");
    let (layer_col, version_col, pins): (Option<String>, Option<i32>, Value) = sqlx::query_as(
        "SELECT template_layer, template_version, pins FROM jobs WHERE key = 'blank_one'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!((layer_col, version_col), (None, None));
    let families: Vec<&str> = pins
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["layer"].as_str().unwrap())
        .collect();
    assert_eq!(families, ["base", "industry/tjd_ind", dept]);
    let selection: Value = sqlx::query_scalar(
        "SELECT t.selection FROM job_target t JOIN jobs j ON j.id = t.job_id WHERE j.key = 'blank_one'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(selection["industry"], "tjd_ind");
    assert_eq!(selection["departments"], json!([dept]));
    let industry: String = sqlx::query_scalar("SELECT industry FROM tenants WHERE id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(industry, "tjd_ind");
    let (details,): (Value,) = sqlx::query_as(
        "SELECT details FROM audit_event WHERE action = 'job.create' AND resource_id = 'blank_one'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(details["template"].is_null(), "{details}");
    assert_eq!(details["by_role"], "tenant_admin");
    // an explicit key that exists is 409, never auto-suffixed
    let (s, b) = create_as(&e, "u-admin", body).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "job_exists");
    // another industry than the company's is refused
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "blank_two", "label": "Two", "industry": "retail", "departments": ["industry_dept/retail/customer_service"]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "industry_mismatch");
}

/// D116: a department composes on its base department. A blank job pins it too, or its design never compiles (no
/// roles, no activities: the assistant can never run).
#[tokio::test]
async fn blank_job_pins_the_base_department_its_department_composes_on() {
    let e = common::env().await;
    tenant(&e).await;
    let bd = layer(
        &e,
        json!({"scope": "base_dept", "department": "tjd_dept", "label": "Tjd base department"}),
        "base_dept/tjd_dept",
    )
    .await;
    publish(&e, "base_dept/tjd_dept", "base_dept.tjd_dept", &bd).await;
    // the department extends the base department (as `industry_dept/retail/customer_service` extends
    // `customer_service`), and its tag is cut after that exists: it composes on it
    let base = layer(&e, json!({"scope": "base", "label": "Base"}), "base").await;
    let (s, b) = sa_post(
        &e,
        &format!("/api/layers/{base}/rows"),
        json!({"kind": "department_archetype", "key": "tjd_dept", "op": "add",
               "fields": {"label": "Tjd department"}}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    layer(
        &e,
        json!({"scope": "industry", "industry": "tjd_ind", "label": "Tjd industry"}),
        "industry/tjd_ind",
    )
    .await;
    layer(
        &e,
        json!({"scope": "industry_dept", "industry": "tjd_ind", "department": "tjd_dept",
               "extends": "tjd_dept", "label": "Tjd department"}),
        "industry_dept/tjd_ind/tjd_dept",
    )
    .await;
    publish_offering(&e).await;
    let dept = "industry_dept/tjd_ind/tjd_dept";
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "bd_blank", "label": "B", "industry": "tjd_ind", "departments": [dept]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE key = 'bd_blank'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let families: Vec<&str> = pins
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["layer"].as_str().unwrap())
        .collect();
    assert_eq!(
        families,
        ["base", "industry/tjd_ind", dept, "base_dept/tjd_dept"]
    );
    let (s, b) = send(
        &e.app,
        get(
            "/api/jobs/bd_blank/chat/profile",
            Some(&person("u-admin", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["test_readiness"]["template_errors"], json!([]), "{b}");

    // 0247 repairs a job created before the fix: the base department pin is missing, the design does not compile
    sqlx::query(
        "UPDATE jobs SET pins = (SELECT jsonb_agg(p) FROM jsonb_array_elements(pins) p \
                                  WHERE p->>'layer' <> 'base_dept/tjd_dept') WHERE key = 'bd_blank'",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    let profile = || async {
        send(
            &e.app,
            get(
                "/api/jobs/bd_blank/chat/profile",
                Some(&person("u-admin", &[])),
            ),
        )
        .await
        .1
    };
    assert_eq!(
        profile().await["test_readiness"]["template_errors"],
        json!(["unpinned_layer:base_dept/tjd_dept"])
    );
    sqlx::raw_sql(include_str!(
        "../migrations/0247_blank_job_base_dept_pins.sql"
    ))
    .execute(&e.owner)
    .await
    .unwrap();
    let repaired: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE key = 'bd_blank'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(repaired, pins);
    assert_eq!(
        profile().await["test_readiness"]["template_errors"],
        json!([])
    );
}

/// A hierarchy move on a job that was never released: `PUT /api/jobs/{job}/target` reconciles its pins to the new
/// offering (D115/D116), and the draft compiles, so a normal release can publish the move.
#[tokio::test]
async fn an_unreleased_job_moves_to_another_industry_and_its_pins_follow() {
    const A: (&str, &str) = ("mv_ind_a", "mv_dept_a");
    const B: (&str, &str) = ("mv_ind_b", "mv_dept_b");
    let e = common::env().await;
    tenant(&e).await;
    let base = layer(&e, json!({"scope": "base", "label": "Base"}), "base").await;
    profile(&e, &base, "mv_profile_a", Some(A.0)).await;
    profile(&e, &base, "mv_profile_b", Some(B.0)).await;
    retag(&e, "base").await;
    // each department composes on a base department (D116): its base_dept layer and its archetype row in base
    for (_, dept) in [A, B] {
        let bd = layer(
            &e,
            json!({"scope": "base_dept", "department": dept, "label": dept}),
            &format!("base_dept/{dept}"),
        )
        .await;
        publish(
            &e,
            &format!("base_dept/{dept}"),
            &format!("base_dept.{dept}"),
            &bd,
        )
        .await;
    }
    for (_, dept) in [A, B] {
        let (s, b) = sa_post(
            &e,
            &format!("/api/layers/{base}/rows"),
            json!({"kind": "department_archetype", "key": dept, "op": "add",
                   "fields": {"label": dept}}),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "{dept}: {b}");
    }
    // the archetype rows must be in a base tag the department tags record
    retag(&e, "base").await;
    for (ind, dept) in [A, B] {
        let il = layer(
            &e,
            json!({"scope": "industry", "industry": ind, "label": ind}),
            &format!("industry/{ind}"),
        )
        .await;
        publish(
            &e,
            &format!("industry/{ind}"),
            &format!("industry.{ind}"),
            &il,
        )
        .await;
        let dl = layer(
            &e,
            json!({"scope": "industry_dept", "industry": ind, "department": dept, "extends": dept,
                   "label": dept}),
            &format!("industry_dept/{ind}/{dept}"),
        )
        .await;
        publish(
            &e,
            &format!("industry_dept/{ind}/{dept}"),
            &format!("industry_dept.{ind}.{dept}"),
            &dl,
        )
        .await;
    }

    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "mv_job", "label": "Move", "industry": A.0,
               "departments": [format!("industry_dept/{}/{}", A.0, A.1)]}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let admin = person("u-admin", &[]);
    let (s, target) = send(&e.app, get("/api/jobs/mv_job/target", Some(&admin))).await;
    assert_eq!(s, StatusCode::OK, "{target}");
    let seen = target["selection_updated_at"].clone();

    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/jobs/mv_job/target",
            Some(&admin),
            json!({
                "selection": {
                    "profile": "mv_profile_b",
                    "industry": B.0,
                    "departments": [format!("industry_dept/{}/{}", B.0, B.1)],
                    "areas": [],
                    "dimensions": {"channel": []},
                    "region": null,
                },
                "expected_updated_at": seen,
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");

    let pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE key = 'mv_job'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let mut families: Vec<&str> = pins
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["layer"].as_str().unwrap())
        .collect();
    families.sort_unstable();
    let mut want = vec![
        "base",
        "base_dept/mv_dept_b",
        "industry/mv_ind_b",
        "industry_dept/mv_ind_b/mv_dept_b",
    ];
    want.sort_unstable();
    assert_eq!(families, want, "{pins}");
    let (s, profile) = send(&e.app, get("/api/jobs/mv_job/chat/profile", Some(&admin))).await;
    assert_eq!(s, StatusCode::OK, "{profile}");
    assert_eq!(
        profile["test_readiness"]["template_errors"],
        json!([]),
        "{profile}"
    );
}

#[tokio::test]
async fn blank_job_needs_published_layers() {
    let e = common::env().await;
    tenant(&e).await;
    publish_offering(&e).await;
    // a department with no tagged version
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "blank_draft", "label": "Draft", "industry": "tjd_ind",
               "departments": ["industry_dept/tjd_ind/tjd_draft"]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "layer_not_published");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains("industry_dept/tjd_ind/tjd_draft"),
        "{b}"
    );
    // an industry nobody published
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "blank_none", "label": "None", "industry": "tjd_nowhere",
               "departments": ["industry_dept/tjd_nowhere/x"]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "layer_not_published");
    // a department of another industry breaks the D115 hierarchy
    let (s, b) = create_as(
        &e,
        "u-admin",
        json!({"key": "blank_mix", "label": "Mix", "industry": "tjd_ind",
               "departments": ["industry_dept/retail/customer_service"]}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
}

#[tokio::test]
async fn selection_options_come_from_the_database() {
    let e = common::env().await;
    tenant(&e).await;
    publish_offering(&e).await;
    // drafted after base's tag: not part of any published version
    let base = layer(&e, json!({"scope": "base", "label": "Base"}), "base").await;
    profile(&e, &base, "tjd_draft_profile", Some("tjd_ind")).await;
    let (s, b) = send(
        &e.app,
        get("/api/tenant/profile", Some(&person("u-admin", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let keys = |v: &Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|c| c["key"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(
        keys(&b["options"]["industries"]).contains(&"tjd_ind".to_string()),
        "{b}"
    );
    // D132: the company profile offers the business profiles (publish_offering authors tjd_profile)
    let profile = b["options"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["key"] == "tjd_profile")
        .unwrap_or_else(|| panic!("tjd_profile offered: {b}"));
    assert_eq!(profile["industry"], "tjd_ind", "{b}");
    let depts = keys(&b["options"]["departments"]);
    assert!(
        depts.contains(&"industry_dept/tjd_ind/tjd_dept".to_string()),
        "{b}"
    );
    // a drafted, untagged profile is not offered (the compile could not find it)
    assert!(
        !b["options"]["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["key"] == "tjd_draft_profile"),
        "{b}"
    );
    // untagged departments are not offered
    assert!(
        !depts.contains(&"industry_dept/tjd_ind/tjd_draft".to_string()),
        "{b}"
    );
    // the catalog answers (possibly empty) without a seed
    let (s, b) = send(
        &e.app,
        get("/api/job-templates", Some(&person("u-admin", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["templates"].is_array(), "{b}");
}
