//! B6b T9b + T12a + the T10 KH bucket: the P5 compile routes (`/api/compile*`), `departmentArchetypes` and
//! `platformDimensionUsage` over the booted platform seed (v1 + the v2/v3 deltas) — super_admin only, the head
//! compile (D106/D111/D115 refusals, `compile_failed` with every error, never a write), compile as of chosen tags
//! (F-07), the references (seeds + B2 releases, R10), KH's archetype fold (D107) and the cross-tenant usage read (R3).
use std::collections::BTreeSet;

use agent_api::audit::Actor;
use agent_api::platform::db::rows;
use agent_api::platform::templates::{self, TemplatePin};
use agent_api::platform_db::begin_platform;
use agent_model::compile::Selection;
use agent_model::kinds::{Kind, Op};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{
    TestEnv, get, insert_tenant, insert_user, json as json_req, person, send, super_admin,
};

const CCA: &str = "job/customer_care_agent";

/// The compile references of the regenerated KH goldens (`agent-model/tests/golden/compile`).
const GOLDEN_REFS: [(&str, &str); 5] = [
    (
        "base-only",
        include_str!("../../../agent-model/tests/golden/compile/base-only.json"),
    ),
    (
        "demo-retail-approved",
        include_str!("../../../agent-model/tests/golden/compile/demo-retail-approved.json"),
    ),
    (
        "demo-retail-live",
        include_str!("../../../agent-model/tests/golden/compile/demo-retail-live.json"),
    ),
    (
        "retail-ontology",
        include_str!("../../../agent-model/tests/golden/compile/retail-ontology.json"),
    ),
    (
        "rivervale-live",
        include_str!("../../../agent-model/tests/golden/compile/rivervale-live.json"),
    ),
];

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

fn error_codes(b: &Value) -> Vec<String> {
    b["error"]["errors"]
        .as_array()
        .map(|l| {
            l.iter()
                .map(|e| e["code"].as_str().unwrap_or("").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

async fn sa_get(e: &TestEnv, uri: &str) -> (StatusCode, Value) {
    send(&e.app, get(uri, Some(&super_admin()))).await
}

async fn ok(e: &TestEnv, uri: &str) -> Value {
    let (s, b) = sa_get(e, uri).await;
    assert_eq!(s, StatusCode::OK, "{uri}: {b}");
    b
}

async fn compile(e: &TestEnv, body: Value) -> (StatusCode, Value) {
    send(
        &e.app,
        json_req("POST", "/api/compile", Some(&super_admin()), body),
    )
    .await
}

fn golden(name: &str) -> Value {
    let (_, text) = GOLDEN_REFS
        .iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("golden {name}"));
    serde_json::from_str(text).unwrap_or_else(|e| panic!("golden {name}: {e}"))
}

fn target(name: &str) -> Value {
    let g = golden(name);
    json!({"selection": g["selection"], "job": g["job"]})
}

/// The pins of the latest tagged `job/customer_care_agent` template (its tag stack ∪ the tag): what a B2 cut pins.
async fn cca_pins(e: &TestEnv) -> Vec<TemplatePin> {
    let pins: Value = sqlx::query_scalar(
        "SELECT pins FROM job_template_tag WHERE layer = $1 AND status = 'tagged' \
          ORDER BY version DESC LIMIT 1",
    )
    .bind(CCA)
    .fetch_one(&e.owner)
    .await
    .expect("a tagged customer_care_agent template");
    serde_json::from_value(pins).expect("template pins")
}

/// Re-label the base `Party` entity (an untagged head edit that changes every compile's hash).
async fn edit_base_party(e: &TestEnv) {
    let mut fields: Value = sqlx::query_scalar(
        "SELECT fields FROM layer_head WHERE layer_key = 'base' AND kind = 'entity' AND key = 'Party'",
    )
    .fetch_one(&e.owner)
    .await
    .expect("base Party");
    fields["label"] = json!("Customer (edited after the tag)");
    let mut tx = begin_platform(&e.owner).await.expect("begin_platform");
    rows::write_revision(
        &mut tx,
        &Actor::system("service:agent-api-b6b-t9b-test"),
        "base",
        Kind::Entity,
        "Party",
        Op::Add,
        fields,
        "t9b-author",
    )
    .await
    .expect("write_revision");
    tx.commit().await.unwrap();
}

async fn insert_job(e: &TestEnv, tenant: Uuid, key: &str, label: &str, pins: &Value) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, $3, $4, 'test') RETURNING id",
    )
    .bind(tenant)
    .bind(key)
    .bind(label)
    .bind(pins)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn insert_release(
    e: &TestEnv,
    tenant: Uuid,
    job: Uuid,
    id: &str,
    state: &str,
    selection: &Value,
    pins: &Value,
) {
    let sha = |c: char| format!("sha256:{}", c.to_string().repeat(64));
    sqlx::query(
        r#"INSERT INTO job_release (tenant_id, job_id, release_id, hash, bundle_hash, bundle, selection, pins,
               provenance, requires, causes, state, approved_by, approved_at, released_by, released_at,
               snapshot_kinds)
           VALUES ($1, $2, $3, $4, $5, '{"sections": []}', $6, $7, '{}', '{}', '[]', $8::text, 'u-rm', now(),
               CASE WHEN $8::text = 'approved' THEN NULL ELSE 'u-rm' END,
               CASE WHEN $8::text = 'approved' THEN NULL ELSE now() END,
               ARRAY['languages', 'setting'])"#,
    )
    .bind(tenant)
    .bind(job)
    .bind(id)
    .bind(sha('a'))
    .bind(sha('b'))
    .bind(selection)
    .bind(pins)
    .bind(state)
    .execute(&e.owner)
    .await
    .unwrap();
}

async fn count(e: &TestEnv, table: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

// ---- access ------------------------------------------------------------------------------------------------------

#[tokio::test]
async fn compile_routes_are_super_admin_only() {
    let e = crate::common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    let admin = person("u-admin", &[]);
    for uri in [
        "/api/compile/options",
        "/api/compile/references",
        "/api/department-archetypes",
        "/api/dimensions/usage",
    ] {
        let (s, b) = send(&e.app, get(uri, Some(&admin))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{uri}: {b}");
        assert_eq!(code(&b), "super_admin_required", "{uri}");
        let (s, b) = send(&e.app, get(uri, None)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{uri}: {b}");
    }
    let (s, b) = send(
        &e.app,
        json_req("POST", "/api/compile", Some(&admin), target("base-only")),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(code(&b), "super_admin_required");
}

// ---- compileOptions ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn compile_options_lists_departments_regions_and_tags() {
    let e = crate::common::env().await;
    crate::b6b::support::row(
        &e,
        "base",
        "department_archetype",
        "returns_desk",
        json!({"label": "Returns desk"}),
    )
    .await;
    crate::b6b::support::ensure_layer(
        &e,
        "industry_dept/retail/returns",
        json!({"scope": "industry_dept", "industry": "retail", "department": "returns",
               "label": "Returns", "extends": "returns_desk"}),
    )
    .await;
    let b = ok(&e, "/api/compile/options").await;

    let depts = b["departments"].as_array().unwrap();
    let cs = depts
        .iter()
        .find(|d| d["key"] == "industry_dept/retail/customer_service")
        .unwrap_or_else(|| panic!("retail customer_service in {b}"));
    assert_eq!(cs["industry"], "retail");
    assert_eq!(cs["department"], "customer_service");
    let returns = depts
        .iter()
        .find(|d| d["key"] == "industry_dept/retail/returns")
        .unwrap_or_else(|| panic!("retail returns in {b}"));
    assert_eq!(returns["extends"], "returns_desk");
    for d in depts {
        let key = d["key"].as_str().unwrap();
        assert_eq!(
            key,
            format!(
                "industry_dept/{}/{}",
                d["industry"].as_str().unwrap(),
                d["department"].as_str().unwrap()
            ),
            "{d}"
        );
        assert!(
            !key.contains("/region/"),
            "a region sub-layer is no department: {d}"
        );
    }
    let industries: BTreeSet<&str> = b["industries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["key"].as_str().unwrap())
        .collect();
    assert!(industries.contains("retail"), "{b}");
    assert!(
        b["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|j| j["key"] == "customer_care_agent"),
        "{b}"
    );
    assert!(
        b["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["key"] == "retail_ecommerce"),
        "{b}"
    );
    assert!(
        b["dimensions"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["key"] != "area"),
        "areas come with their department: {b}"
    );

    // D111: the region choices are the `region` dimension's values
    let mut want: Vec<String> =
        sqlx::query_scalar("SELECT key FROM dimension_value WHERE dimension = 'region'")
            .fetch_all(&e.owner)
            .await
            .unwrap();
    want.sort();
    let got: Vec<String> = b["regions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(got, want);

    // T12a: tag versions per family, newest first, provisional v0 excluded
    let mut versions: Vec<i32> =
        sqlx::query_scalar("SELECT version FROM layer_tag WHERE family = $1 AND NOT provisional")
            .bind(CCA)
            .fetch_all(&e.owner)
            .await
            .unwrap();
    versions.sort_unstable_by(|a, b| b.cmp(a));
    let tag = b["tags"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["family"] == CCA)
        .unwrap_or_else(|| panic!("{CCA} tags in {b}"));
    assert_eq!(tag["versions"], json!(versions));
}

// ---- compilePreview (head) ---------------------------------------------------------------------------------------

/// The compile references of KH's goldens that name the offering's industry, through the route. The goldens were
/// compiled over a seeded platform; this database holds the offering alone, so the route is held to the golden's
/// applied layers that exist here (the profile and channel layers do not) and to no fired rule (none is authored),
/// plus a stable `sha256:` hash on repeat. The utilities reference (`rivervale-live`) names an industry nothing here
/// authors and is not compiled.
#[tokio::test]
async fn compile_preview_equals_golden() {
    let e = crate::common::env().await;
    let present: Vec<String> = sqlx::query_scalar("SELECT key FROM layer")
        .fetch_all(&e.owner)
        .await
        .unwrap();
    for (name, _) in GOLDEN_REFS {
        if name == "rivervale-live" {
            continue;
        }
        let g = golden(name);
        let expected = &g["expected"];
        assert_eq!(expected["ok"], true, "{name}: a compile reference compiles");
        let (s, b) = compile(&e, target(name)).await;
        assert_eq!(s, StatusCode::OK, "{name}: {b}");
        let want: Vec<&Value> = expected["config"]["layers"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|l| present.iter().any(|p| Some(p.as_str()) == l.as_str()))
            .collect();
        assert_eq!(b["layers"], json!(want), "{name}");
        assert_eq!(b["rules_fired"], json!([]), "{name}");
        let hash = b["hash"].as_str().unwrap_or("");
        assert!(hash.starts_with("sha256:"), "{name}: {hash}");
        let (_, again) = compile(&e, target(name)).await;
        assert_eq!(again["hash"], b["hash"], "{name}: stable on reload");
        assert!(b["knowledge"]["kinds"].is_array(), "{name}: the D107 fold");
    }
    // the base-only preview is labelled
    let (_, b) = compile(&e, target("base-only")).await;
    assert!(
        b["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["code"] == "base_only_preview"),
        "{b}"
    );
}

#[tokio::test]
async fn compile_failed_lists_every_error() {
    let e = crate::common::env().await;
    let (s, b) = compile(
        &e,
        json!({"selection": {
            "industry": "retail",
            "departments": ["industry_dept/retail/no_such_a", "industry_dept/retail/no_such_b"],
        }}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "compile_failed", "{b}");
    let keys: BTreeSet<&str> = b["error"]["errors"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|x| x["code"] == "invalid_selection")
        .map(|x| x["key"].as_str().unwrap())
        .collect();
    assert!(keys.contains("selection.departments[0]"), "{b}");
    assert!(keys.contains("selection.departments[1]"), "{b}");
}

#[tokio::test]
async fn unknown_profile_404() {
    let e = crate::common::env().await;
    let g = golden("demo-retail-live");
    let mut sel = g["selection"].clone();
    sel["profile"] = json!("no_such_profile");
    let (s, b) = compile(&e, json!({"selection": sel, "job": g["job"]})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "profile_not_found");
}

#[tokio::test]
async fn compile_hierarchy_refusals_422() {
    let e = crate::common::env().await;
    // D106: an industry without a department (and a job without either)
    let (s, b) = compile(
        &e,
        json!({"selection": {"industry": "retail", "departments": []}}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "hierarchy_incomplete");
    assert!(
        b["error"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["path"] == "selection.departments"),
        "{b}"
    );
    let (s, b) = compile(&e, target("base-only").tap_job("customer_care_agent")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "hierarchy_incomplete");
    // D115: a department of another industry, a department twice
    let (s, b) = compile(
        &e,
        json!({"selection": {"industry": "retail", "departments": ["industry_dept/utilities/billing"]}}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "department_industry_mismatch");
    let cs = "industry_dept/retail/customer_service";
    let (s, b) = compile(
        &e,
        json!({"selection": {"industry": "retail", "departments": [cs, cs]}}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "duplicate_department");
    // the legacy shape is not the wire shape
    let (s, b) = compile(
        &e,
        json!({"selection": {"industry": "retail", "industry_depts": ["customer_service"]}}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
}

#[tokio::test]
async fn compile_unknown_region_422() {
    let e = crate::common::env().await;
    let g = golden("demo-retail-live");
    let mut sel = g["selection"].clone();
    sel["region"] = json!("zz_no_such_region");
    let (s, b) = compile(&e, json!({"selection": sel, "job": g["job"]})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "unknown_region");
    assert_eq!(error_codes(&b), ["unknown_region"]);
}

#[tokio::test]
async fn compile_never_writes() {
    let e = crate::common::env().await;
    let tables = [
        "layer",
        "layer_row",
        "layer_tag",
        "job_template_tag",
        "platform_audit",
    ];
    let mut before = Vec::new();
    for t in tables {
        before.push(count(&e, t).await);
    }
    for name in ["base-only", "demo-retail-live"] {
        let (s, b) = compile(&e, target(name)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    let (s, _) = compile(
        &e,
        json!({"selection": {"industry": "retail", "departments": ["industry_dept/retail/nope"]}}),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let pins = cca_pins(&e).await;
    let mut body = target("demo-retail-live");
    body["as_of"] = json!(pins);
    let (s, b) = compile(&e, body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    for uri in [
        "/api/compile/options",
        "/api/compile/references",
        "/api/department-archetypes",
        "/api/dimensions/usage",
    ] {
        ok(&e, uri).await;
    }
    for (t, n) in tables.iter().zip(before) {
        assert_eq!(count(&e, t).await, n, "{t}");
    }
}

// ---- compilePreview as_of (T12a, F-07) -----------------------------------------------------------------------------

#[tokio::test]
async fn as_of_compiles_tag_members_not_head() {
    let e = crate::common::env().await;
    // the base `Party` entity is in the tag the job template pins: tag base, then re-tag the job on top of it
    crate::b6b::support::row(&e, "base", "entity", "Party", json!({"label": "Customer"})).await;
    crate::b6b::support::tag(&e, "base").await;
    crate::b6b::support::row(&e, CCA, "job_setting", "b6b_pin", json!({"label": "Pin"})).await;
    crate::b6b::support::tag(&e, CCA).await;
    let pins = cca_pins(&e).await;
    let head_before = compile(&e, target("demo-retail-live")).await.1["hash"].clone();
    let mut as_of = target("demo-retail-live");
    as_of["as_of"] = json!(pins);
    let (s, pinned_before) = compile(&e, as_of.clone()).await;
    assert_eq!(s, StatusCode::OK, "{pinned_before}");

    edit_base_party(&e).await;

    let (s, head_after) = compile(&e, target("demo-retail-live")).await;
    assert_eq!(s, StatusCode::OK, "{head_after}");
    assert_ne!(head_after["hash"], head_before, "head compiles the edit");
    let (s, pinned_after) = compile(&e, as_of).await;
    assert_eq!(s, StatusCode::OK, "{pinned_after}");
    assert_eq!(
        pinned_after["hash"], pinned_before["hash"],
        "as_of compiles tag members only"
    );
    assert_ne!(
        pinned_after["entities"]["Party"]["label"],
        json!("Customer (edited after the tag)")
    );
}

#[tokio::test]
async fn as_of_unknown_tag_404() {
    let e = crate::common::env().await;
    let mut body = target("demo-retail-live");
    body["as_of"] = json!([{"layer": CCA, "version": 9999}]);
    let (s, b) = compile(&e, body).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "tag_not_found");
}

#[tokio::test]
async fn as_of_missing_family_is_unpinned_layer() {
    let e = crate::common::env().await;
    let pins: Vec<TemplatePin> = cca_pins(&e)
        .await
        .into_iter()
        .filter(|p| p.layer != "base")
        .collect();
    let mut body = target("demo-retail-live");
    body["as_of"] = json!(pins);
    let (s, b) = compile(&e, body).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "compile_failed");
    assert!(
        b["error"]["errors"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["code"] == "unpinned_layer" && x["key"] == "base"),
        "{b}"
    );
}

#[tokio::test]
async fn as_of_equals_release_snapshot_compile() {
    let e = crate::common::env().await;
    let pins = cca_pins(&e).await;
    let g = golden("demo-retail-approved");
    let selection: Selection = serde_json::from_value(g["selection"].clone()).unwrap();
    let job = g["job"].as_str();
    // the B2 cut's path: compile_pinned (Mode::Job) over the same pins
    let mut conn = e.owner.acquire().await.unwrap();
    let snapshot = templates::compile_pinned(&mut conn, &pins, &selection, job)
        .await
        .unwrap_or_else(|e| panic!("compile_pinned: {e}"))
        .unwrap_or_else(|e| panic!("compile_pinned errors: {e:#?}"));
    drop(conn);
    let mut body = target("demo-retail-approved");
    body["as_of"] = json!(pins);
    let (s, b) = compile(&e, body).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["hash"], json!(snapshot.effective.hash));
}

// ---- compileReferences (R10) ----------------------------------------------------------------------------------------

#[tokio::test]
async fn compile_references_are_seeds_and_live_releases() {
    let e = crate::common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant ref").await;
    let pins = json!(cca_pins(&e).await);
    let job = insert_job(&e, t, "care", "Care", &pins).await;
    let live = golden("demo-retail-live")["selection"].clone();
    // R15: a release cut before KH keeps the legacy shape
    let legacy = json!({
        "profile": "retail_ecommerce", "industry": "retail", "industry_depts": ["customer_service"],
        "areas": [], "dimensions": {"channel": ["chat"], "region": []}
    });
    insert_release(
        &e,
        t,
        job,
        "cfg-2026-10-20.1",
        "applied_earlier",
        &live,
        &pins,
    )
    .await;
    insert_release(&e, t, job, "cfg-2026-10-21.1", "current", &live, &pins).await;
    insert_release(&e, t, job, "cfg-2026-10-22.1", "approved", &legacy, &pins).await;

    let list = ok(&e, "/api/compile/references").await;
    let list = list.as_array().unwrap();
    let mut seeds: Vec<String> = sqlx::query_scalar("SELECT key FROM compile_reference_seed")
        .fetch_all(&e.owner)
        .await
        .unwrap();
    seeds.sort();
    let got_seeds: Vec<String> = list
        .iter()
        .take(seeds.len())
        .map(|r| r["key"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(got_seeds, seeds, "seeds first, by key");
    assert!(
        list.iter()
            .take(seeds.len())
            .all(|r| r.get("release").is_none())
    );

    let ours: Vec<&Value> = list
        .iter()
        .filter(|r| r["release"]["tenant_id"] == json!(t.to_string()))
        .collect();
    let states: Vec<&str> = ours
        .iter()
        .map(|r| r["release"]["status"].as_str().unwrap())
        .collect();
    assert_eq!(
        states,
        ["current", "approved"],
        "applied_earlier is not a reference"
    );
    for r in &ours {
        assert_eq!(r["job"], "customer_care_agent", "{r}");
        assert_eq!(r["release"]["tenant"], "Tenant ref");
        assert_eq!(r["release"]["job"], "care");
        assert_eq!(
            r["selection"]["departments"],
            json!(["industry_dept/retail/customer_service"]),
            "{r}"
        );
    }
    // every reference compiles (T14 row `/platform/compile`)
    for r in list
        .iter()
        .filter(|r| r.get("release").is_none() || r["release"]["tenant_id"] == json!(t.to_string()))
    {
        let (s, b) = compile(&e, json!({"selection": r["selection"], "job": r["job"]})).await;
        assert_eq!(s, StatusCode::OK, "{}: {b}", r["key"]);
    }
}

// ---- departmentArchetypes (D60/D107) ---------------------------------------------------------------------------------

#[tokio::test]
async fn department_archetypes_list_compiled_kinds() {
    let e = crate::common::env().await;
    use crate::b6b::support::{ensure_layer, row};
    row(
        &e,
        "base",
        "department_archetype",
        "customer_service",
        json!({"label": "Customer service", "knowledge_kinds": ["faq.general"]}),
    )
    .await;
    ensure_layer(
        &e,
        "industry_dept/retail/returns",
        json!({"scope": "industry_dept", "industry": "retail", "department": "returns",
               "label": "Returns", "extends": "customer_service", "areas": ["order-support"]}),
    )
    .await;
    // D107/D116: the it_service_desk archetype's kinds and activities live in its base department
    row(
        &e,
        "base",
        "department_archetype",
        "it_service_desk",
        json!({"label": "IT service desk"}),
    )
    .await;
    ensure_layer(
        &e,
        "base_dept/it_service_desk",
        json!({"scope": "base_dept", "department": "it_service_desk", "label": "IT service desk"}),
    )
    .await;
    row(
        &e,
        "base_dept/it_service_desk",
        "stage_knowledge_kind",
        "*|kb.identity",
        json!({"label": "kb.identity"}),
    )
    .await;
    row(
        &e,
        "base_dept/it_service_desk",
        "activity",
        "identity:reset_password",
        json!({"label": "Reset a password"}),
    )
    .await;
    ensure_layer(
        &e,
        "industry_dept/retail/it_service_desk",
        json!({"scope": "industry_dept", "industry": "retail", "department": "it_service_desk",
               "label": "IT service desk", "extends": "it_service_desk"}),
    )
    .await;

    let list = ok(&e, "/api/department-archetypes").await;
    let list = list.as_array().unwrap();
    let keys: Vec<&str> = list.iter().map(|a| a["key"].as_str().unwrap()).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "by key");
    let arch = |key: &str| -> &Value {
        list.iter()
            .find(|a| a["key"] == key)
            .unwrap_or_else(|| panic!("archetype {key}"))
    };

    let cs = arch("customer_service");
    assert_eq!(cs["fold_step"], "department_archetype/customer_service");
    assert!(cs["knowledge_kinds"].is_array());
    let retail_cs = cs["specialised_by"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["layer"] == "industry_dept/retail/returns")
        .unwrap_or_else(|| panic!("retail returns specialises customer_service: {cs}"));
    assert_eq!(retail_cs["industry"], "retail");
    assert_eq!(retail_cs["key"], "returns");
    let areas: Vec<String> = sqlx::query_scalar(
        "SELECT unnest(areas) FROM layer WHERE key = 'industry_dept/retail/returns'",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(retail_cs["areas"], json!(areas));
    assert!(
        retail_cs["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k["kind"] == "faq.general"),
        "{retail_cs}"
    );

    let itsd = arch("it_service_desk");
    let retail_itsd = itsd["specialised_by"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["layer"] == "industry_dept/retail/it_service_desk")
        .unwrap_or_else(|| panic!("retail it_service_desk: {itsd}"));
    let step = "base_dept/it_service_desk";
    assert!(
        retail_itsd["kinds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|k| k["kind"] == "kb.identity" && k["layer"] == step),
        "{retail_itsd}"
    );
    assert!(
        retail_itsd["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["layer"] == step),
        "{retail_itsd}"
    );
}

// ---- platformDimensionUsage (R3) -------------------------------------------------------------------------------------

#[tokio::test]
async fn usage_names_layers_and_context_spaces() {
    let e = crate::common::env().await;
    use crate::b6b::support::{dimension, ensure_layer, row};
    dimension(&e, "channel", "dimension", &["chat", "email"]).await;
    dimension(&e, "area", "grouping", &["order-support"]).await;
    ensure_layer(
        &e,
        "job/customer_care_agent/channel=email",
        json!({"scope": "job", "job": "customer_care_agent", "dimension": ["channel", "email"],
               "label": "Email"}),
    )
    .await;
    row(
        &e,
        "job/customer_care_agent/channel=email",
        "job_setting",
        "email_tone",
        json!({"label": "Email tone"}),
    )
    .await;
    row(
        &e,
        "industry_dept/retail/customer_service",
        "context_space",
        "retail:order-support:track_order",
        json!({"label": "Track an order", "area": "order-support"}),
    )
    .await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant usage").await;
    let job = insert_job(&e, t, "care", "Care", &json!([])).await;
    let selection = json!({
        "profile": "retail_ecommerce", "industry": "retail",
        "departments": ["industry_dept/retail/customer_service"],
        "areas": ["order-support"], "dimensions": {"channel": ["email", "fax_not_standard"]}, "region": null
    });
    sqlx::query(
        "INSERT INTO job_target (tenant_id, job_id, selection, updated_by, updated_at) \
         VALUES ($1, $2, $3, 'test', now())",
    )
    .bind(t)
    .bind(job)
    .bind(&selection)
    .execute(&e.owner)
    .await
    .unwrap();
    insert_release(
        &e,
        t,
        job,
        "cfg-2026-10-22.1",
        "approved",
        &selection,
        &json!([]),
    )
    .await;

    let b = ok(&e, "/api/dimensions/usage").await;
    assert_eq!(
        b["selection_keys"],
        json!({"area": "areas", "channel": "dimensions.channel", "region": "region"})
    );
    let value = |dim: &str, v: &str| -> &Value {
        b["values"]
            .as_array()
            .unwrap()
            .iter()
            .find(|x| x["dimension"] == dim && x["value"] == v)
            .unwrap_or_else(|| panic!("{dim}={v} in usage"))
    };

    // channel=email → the job × channel layer and the row keys it adds
    let email = value("channel", "email");
    let path = "job/customer_care_agent/channel=email";
    let layer = email["layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["path"] == path)
        .unwrap_or_else(|| panic!("{path}: {email}"));
    assert_eq!(layer["job"], "customer_care_agent");
    let mut adds: Vec<(String, String)> =
        sqlx::query_as("SELECT kind, key FROM layer_head WHERE layer_key = $1")
            .bind(path)
            .fetch_all(&e.owner)
            .await
            .unwrap();
    adds.sort();
    let adds: Vec<String> = adds.into_iter().map(|(_, k)| k).collect();
    assert_eq!(layer["adds"], json!(adds));
    let ours = |list: &Value| -> Vec<Value> {
        list.as_array()
            .unwrap()
            .iter()
            .filter(|x| x["tenant_id"] == json!(t.to_string()))
            .cloned()
            .collect()
    };
    assert_eq!(
        ours(&email["selections"]),
        [
            json!({"tenant_id": t.to_string(), "tenant_name": "Tenant usage", "job": "care", "job_label": "Care"})
        ]
    );
    let releases = ours(&email["releases"]);
    assert_eq!(releases.len(), 1, "{email}");
    assert_eq!(releases[0]["id"], "cfg-2026-10-22.1");
    assert_eq!(releases[0]["state"], "approved");
    assert!(releases[0]["at"].is_string());

    // area=order-support → its context spaces (current, not tombstoned)
    let mut spaces: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT key FROM layer_head \
          WHERE kind = 'context_space' AND op <> 'tombstone' AND split_part(key, ':', 2) = 'order-support'",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    spaces.sort();
    assert!(
        !spaces.is_empty(),
        "the department groups a context space under order-support"
    );
    let area = value("area", "order-support");
    assert_eq!(area["context_spaces"], json!(spaces));
    assert_eq!(ours(&area["selections"]).len(), 1);

    // the selection row and the non-standard value
    let rows = ours(&b["selections"]);
    assert_eq!(rows.len(), 1, "{b}");
    assert_eq!(
        rows[0]["values"]["channel"],
        json!(["email", "fax_not_standard"])
    );
    assert_eq!(rows[0]["values"]["area"], json!(["order-support"]));
    assert!(
        b["unknown"].as_array().unwrap().contains(&json!({
            "dimension": "channel", "value": "fax_not_standard", "tenant_name": "Tenant usage", "job": "care"
        })),
        "{b}"
    );
}

/// `{selection, job}` with the job layer set.
trait TapJob {
    fn tap_job(self, job: &str) -> Value;
}

impl TapJob for Value {
    fn tap_job(mut self, job: &str) -> Value {
        self["job"] = json!(job);
        self
    }
}

/// An in-place edit of a base row — the authoring API stores it as a `patch` at the next revision — compiles at the
/// tag cut after it, with nothing earlier adding the key. `layer_head.rev` is what tells the fold a patch past its
/// first revision is the layer's own definition: without it, base@v18's `agent_role assistant` patch had no earlier
/// add and every head compile failed `patch_target_missing`.
#[tokio::test]
async fn a_base_row_edited_in_place_compiles_at_its_next_tag() {
    let e = crate::common::env().await;
    let layers = ok(&e, "/api/layers?limit=500").await;
    let base = layers["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["key"] == "base")
        .expect("the base layer");
    let id = base["id"].as_str().unwrap().to_owned();
    let fields = json!({
        "label": "Patch probe",
        "used_by": "interaction",
        "max_side_effect": "W",
        "enabled": true,
        "grants": [],
        "note": "A row edited in place.",
        "updated_by": "test",
        "updated_at": "2026-11-02T00:00:00Z",
    });
    let (s, b) = send(
        &e.app,
        json_req(
            "POST",
            &format!("/api/layers/{id}/rows"),
            Some(&super_admin()),
            json!({"kind": "agent_role", "key": "patch_probe", "op": "add", "fields": fields}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let mut edited = fields;
    edited["label"] = json!("Patch probe (edited)");
    let (s, b) = send(
        &e.app,
        json_req(
            "POST",
            &format!("/api/layers/{id}/rows"),
            Some(&super_admin()),
            json!({"kind": "agent_role", "key": "patch_probe", "op": "patch",
                   "expected_rev": 1, "fields": edited}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");

    let (s, b) = send(
        &e.app,
        json_req(
            "POST",
            "/api/platform/templates/base/tags",
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let version: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM layer_tag WHERE family = 'base' AND NOT provisional",
    )
    .fetch_one(&e.owner)
    .await
    .expect("the base tag just cut");

    let (s, b) = compile(
        &e,
        json!({"selection": {}, "as_of": [{"layer": "base", "version": version}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}
