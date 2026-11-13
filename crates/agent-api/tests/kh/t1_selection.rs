//! KH T1 (D106/D111/D115): the selection hierarchy at every entry point — template registration (`tag_family` →
//! `project_job_template_tags`), job creation (`POST /api/jobs`), the compile (`compile_pinned`), `PUT …/target`
//! and the cut (`POST …/releases`) — plus the stored-selection migration 0132 (`kh_selection_v2`, the SQL twin of the
//! seeder's `selection_v2`). A job has one industry and one or more of its departments (`industry_dept/<i>/<d>`,
//! ordered, primary first, no duplicates); only the base-only compile preview may have neither.
use agent_api::audit::Actor;
use agent_api::design::selection_of_value;
use agent_api::error::ApiError;
use agent_api::platform::db::{rows, tags};
use agent_api::platform::hooks::NoHooks;
use agent_api::platform::selection::selection_v2;
use agent_api::platform::templates::{self, TemplatePin};
use agent_api::platform_db::begin_platform;
use agent_model::compile::{self, Selection};
use agent_model::kinds::{Kind, Op};
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob};
use crate::common::{
    TestEnv, get, insert_tenant, insert_user, json as req, person, send, super_admin,
};

const CS: &str = "industry_dept/retail/customer_service";
const BILLING: &str = "industry_dept/utilities/billing";
const CCA: &str = "job/customer_care_agent";

fn actor() -> Actor {
    Actor::system("service:agent-api-kh-t1-test")
}

fn pin(layer: &str, version: i32) -> TemplatePin {
    TemplatePin {
        layer: layer.to_owned(),
        version,
    }
}

/// `(status, code, [(code, path)])` of a refusal: the `errors[{code, path}]` a D106/D115 refusal carries.
fn refused<T: std::fmt::Debug>(
    r: Result<T, ApiError>,
) -> (u16, &'static str, Vec<(String, String)>) {
    let e = r.expect_err("expected a refusal");
    let (status, code) = e.status_and_code();
    let errors = match &e {
        ApiError::Detailed { extra, .. } => extra
            .get("errors")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|x| {
                        (
                            x["code"].as_str().unwrap_or("").to_owned(),
                            x["path"].as_str().unwrap_or("").to_owned(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    (status.as_u16(), code, errors)
}

fn pairs(v: &[(&str, &str)]) -> Vec<(String, String)> {
    v.iter()
        .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
        .collect()
}

/// A layer head inserted through the owner pool (its own family).
async fn head(owner: &PgPool, key: &str, scope: &str, selection: Option<Value>) {
    let parts: Vec<&str> = key.split('/').collect();
    let (industry, department, job) = match scope {
        "industry_dept" => (Some(parts[1]), Some(parts[2]), None),
        "job" => (None, None, Some(parts[1])),
        _ => (None, None, None),
    };
    sqlx::query(
        "INSERT INTO layer (key, scope, industry, department, job, family, label, template_selection) \
         VALUES ($1, $2, $3, $4, $5, $1, $6, $7)",
    )
    .bind(key)
    .bind(scope)
    .bind(industry)
    .bind(department)
    .bind(job)
    .bind(format!("KH {key}"))
    .bind(selection)
    .execute(owner)
    .await
    .unwrap_or_else(|e| panic!("insert layer {key}: {e}"));
}

/// One entity row in `layer`, committed (something to tag).
async fn some_row(db: &PgPool, layer: &str, entity: &str) {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    rows::write_revision(
        &mut tx,
        &actor(),
        layer,
        Kind::Entity,
        entity,
        Op::Add,
        json!({"label": entity, "synonyms": ["kh"]}),
        "kh-t1-author",
    )
    .await
    .unwrap_or_else(|e| panic!("write {layer}: {e:?}"));
    tx.commit().await.expect("commit the row");
}

/// `tag_family` in its own platform transaction, committed only when it succeeds.
async fn tag(db: &PgPool, family: &str) -> Result<tags::TemplateTag, ApiError> {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    let r = tags::tag_family(
        &mut tx,
        &actor(),
        family,
        "kh t1",
        "kh-t1-owner",
        &NoHooks,
        false,
    )
    .await;
    if r.is_ok() {
        tx.commit().await.expect("commit the tag");
    }
    r
}

async fn registered(owner: &PgPool, layer: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM job_template_tag WHERE layer = $1")
        .bind(layer)
        .fetch_one(owner)
        .await
        .unwrap()
}

/// `POST /api/jobs` `{layer, version}` as the super_admin acting in a new tenant.
async fn create_job(e: &TestEnv, layer: &str, version: i32) -> (Uuid, StatusCode, Value) {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, &format!("KH {t}")).await;
    let mut r = req(
        "POST",
        "/api/jobs",
        Some(&super_admin()),
        json!({"template": format!("{}@v{}", layer, version)}),
    );
    r.headers_mut()
        .insert("x-active-tenant-id", t.to_string().parse().unwrap());
    let (s, b) = send(&e.app, r).await;
    (t, s, b)
}

fn body_errors(b: &Value) -> Vec<(String, String)> {
    b["error"]["errors"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|x| {
                    (
                        x["code"].as_str().unwrap_or("").to_owned(),
                        x["path"].as_str().unwrap_or("").to_owned(),
                    )
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---- template registration -------------------------------------------------------------------------------------

#[tokio::test]
async fn registration_refuses_a_template_without_the_hierarchy() {
    let e = crate::common::env().await;
    let db = &e.state.db;
    let cases = [
        (
            "job/kh_none",
            None,
            "hierarchy_incomplete",
            vec![
                ("hierarchy_incomplete", "selection.industry"),
                ("hierarchy_incomplete", "selection.departments"),
            ],
        ),
        (
            "job/kh_no_dept",
            Some(json!({"industry": "retail", "departments": [], "profile": null})),
            "hierarchy_incomplete",
            vec![("hierarchy_incomplete", "selection.departments")],
        ),
        (
            "job/kh_mismatch",
            Some(json!({"industry": "retail", "departments": [BILLING], "profile": null})),
            "department_industry_mismatch",
            vec![("department_industry_mismatch", "selection.departments[0]")],
        ),
        (
            "job/kh_twice",
            Some(json!({"industry": "retail", "departments": [CS, CS], "profile": null})),
            "duplicate_department",
            vec![("duplicate_department", "selection.departments[1]")],
        ),
    ];
    for (layer, selection, code, errors) in cases {
        head(&e.owner, layer, "job", selection).await;
        some_row(db, layer, "KhThing").await;
        let (status, got, list) = refused(tag(db, layer).await);
        assert_eq!((status, got), (422, code), "{layer}");
        assert_eq!(list, pairs(&errors), "{layer}");
        assert_eq!(
            registered(&e.owner, layer).await,
            0,
            "{layer}: the refused tag rolled back, nothing registered"
        );
    }

    // the same template with one industry and its department registers
    head(
        &e.owner,
        "job/kh_ok",
        "job",
        Some(json!({"industry": "retail", "departments": [CS], "profile": "retail_ecommerce"})),
    )
    .await;
    some_row(db, "job/kh_ok", "KhThing").await;
    tag(db, "job/kh_ok")
        .await
        .expect("a hierarchical template registers");
    assert_eq!(registered(&e.owner, "job/kh_ok").await, 1);
}

// ---- job creation ----------------------------------------------------------------------------------------------

#[tokio::test]
async fn job_creation_refuses_a_template_without_the_hierarchy() {
    let e = crate::common::env().await;
    // a template whose selection lost its industry and department after registration (pre-KH data)
    sqlx::query("UPDATE layer SET template_selection = '{\"profile\": \"retail_ecommerce\"}' WHERE key = $1")
        .bind(CCA)
        .execute(&e.owner)
        .await
        .unwrap();
    let (t, s, b) = create_job(&e, CCA, 4).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "hierarchy_incomplete", "{b}");
    assert_eq!(
        body_errors(&b),
        pairs(&[
            ("hierarchy_incomplete", "selection.industry"),
            ("hierarchy_incomplete", "selection.departments"),
        ])
    );
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs WHERE tenant_id = $1")
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(jobs, 0, "no job was created");

    // a department of another industry
    sqlx::query("UPDATE layer SET template_selection = $2 WHERE key = $1")
        .bind(CCA)
        .bind(
            json!({"profile": "retail_ecommerce", "industry": "retail", "departments": [BILLING]}),
        )
        .execute(&e.owner)
        .await
        .unwrap();
    let (_, s, b) = create_job(&e, CCA, 4).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "department_industry_mismatch", "{b}");

    // a selected department the template does not pin
    sqlx::query("UPDATE layer SET template_selection = $2 WHERE key = $1")
        .bind(CCA)
        .bind(json!({"profile": "retail_ecommerce", "industry": "retail",
                     "departments": [CS, "industry_dept/retail/kh_unpinned"]}))
        .execute(&e.owner)
        .await
        .unwrap();
    let (_, s, b) = create_job(&e, CCA, 4).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "hierarchy_incomplete", "{b}");
    assert_eq!(body_errors(&b), pairs(&[("hierarchy_incomplete", "pins")]));

    // the seeded selection (rewritten to the hierarchy shape by the seeder) creates the job
    sqlx::query("UPDATE layer SET template_selection = $2 WHERE key = $1")
        .bind(CCA)
        .bind(json!({"profile": "retail_ecommerce", "industry": "retail", "departments": [CS]}))
        .execute(&e.owner)
        .await
        .unwrap();
    let (_, s, b) = create_job(&e, CCA, 4).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
}

#[tokio::test]
async fn pin_issues_name_the_missing_and_the_foreign_pins() {
    let sel = Selection {
        industry: "retail".into(),
        departments: vec![CS.into()],
        ..Selection::default()
    };
    assert!(
        templates::pin_issues(
            &sel,
            &[pin("base", 3), pin("industry/retail", 2), pin(CS, 2)]
        )
        .is_empty()
    );
    let issues = templates::pin_issues(&sel, &[pin("base", 3), pin(BILLING, 1)]);
    let got: Vec<(&str, &str)> = issues
        .iter()
        .map(|i| (i.code.as_str(), i.message.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "hierarchy_incomplete",
                "The template pins no industry/retail."
            ),
            (
                "hierarchy_incomplete",
                "The template pins no industry_dept/retail/customer_service."
            ),
            (
                "department_industry_mismatch",
                "The template pins industry_dept/utilities/billing of utilities, not of retail."
            ),
        ]
    );
}

// ---- two departments, in order -----------------------------------------------------------------------------------

#[tokio::test]
async fn a_two_department_job_round_trips_with_its_order() {
    let e = crate::common::env().await;
    let db = &e.state.db;
    let returns = "industry_dept/retail/kh_returns";
    head(&e.owner, returns, "industry_dept", None).await;
    some_row(db, returns, "KhReturn").await;
    tag(db, returns)
        .await
        .expect("the second retail department tags");
    // primary = returns, then customer service
    let order = json!([returns, CS]);
    head(
        &e.owner,
        "job/kh_two",
        "job",
        Some(json!({"profile": "retail_ecommerce", "industry": "retail", "departments": order})),
    )
    .await;
    some_row(db, "job/kh_two", "KhTwo").await;
    let t = tag(db, "job/kh_two")
        .await
        .expect("a two-department template registers");

    let mut c = db.acquire().await.unwrap();
    let r = templates::resolve_template(&mut c, "job/kh_two", t.version)
        .await
        .expect("the template resolves");
    drop(c);
    assert_eq!(r.default_selection.departments, [returns, CS]);
    assert_eq!(r.default_selection.primary_department(), Some(returns));
    assert!(r.pins.iter().any(|p| p.layer == returns) && r.pins.iter().any(|p| p.layer == CS));

    let (tenant, s, b) = create_job(&e, "job/kh_two", t.version).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let stored: Value = sqlx::query_scalar(
        "SELECT t.selection FROM job_target t JOIN jobs j ON j.id = t.job_id \
          WHERE j.tenant_id = $1 AND j.key = 'kh_two'",
    )
    .bind(tenant)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        stored["departments"], order,
        "the stored selection keeps the declared order"
    );
    let back: Selection = serde_json::from_value(stored).expect("a selection");
    assert_eq!(back.departments, [returns, CS]);
}

// ---- the compile ---------------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_pinned_compile_refuses_a_broken_hierarchy_and_labels_the_base_only_preview() {
    let e = crate::common::env().await;
    let mut c = e.state.db.acquire().await.unwrap();
    let r = templates::resolve_template(&mut c, CCA, 4).await.unwrap();
    assert_eq!(
        r.default_selection.departments,
        [CS],
        "the seeder applied v1 in the new shape"
    );

    // Ruling R15: a legacy selection with its departments (what a pre-0132 release snapshot holds, immutable, D62)
    // is upgraded in memory by every reader (`design::read::selection_of_value`) and compiles
    let legacy = selection_of_value(&json!({
        "profile": "retail_ecommerce", "industry": "retail", "industry_depts": ["customer_service"],
        "areas": [], "dimensions": {"channel": [], "region": []},
    }))
    .unwrap();
    assert_eq!(legacy.departments, [CS]);
    templates::compile_pinned(&mut c, &r.pins, &legacy, Some("customer_care_agent"))
        .await
        .expect("every pin resolves")
        .unwrap_or_else(|e| panic!("a complete legacy selection compiles: {e:#?}"));
    // only a genuinely empty hierarchy is refused (R2): a legacy selection without departments stays without them
    // after the upgrade — refused, never base only
    let empty = selection_of_value(&json!({
        "profile": "retail_ecommerce", "industry": "retail", "industry_depts": [],
        "areas": [], "dimensions": {"channel": [], "region": []},
    }))
    .unwrap();
    assert!(empty.departments.is_empty());
    let errors = templates::compile_pinned(&mut c, &r.pins, &empty, Some("customer_care_agent"))
        .await
        .expect("every pin resolves")
        .expect_err("no department");
    let got: Vec<(&str, &str)> = errors
        .iter()
        .map(|e| (e.code.as_str(), e.key.as_str()))
        .collect();
    assert_eq!(got, vec![("hierarchy_incomplete", "selection.departments")]);
    let refusal = compile::refusal(&errors).expect("a refusal");
    assert_eq!(
        (refusal.status, refusal.code),
        (422, "hierarchy_incomplete")
    );

    // no industry, a job: refused at both paths
    let errors = templates::compile_pinned(
        &mut c,
        &r.pins,
        &Selection::default(),
        Some("customer_care_agent"),
    )
    .await
    .unwrap()
    .expect_err("no hierarchy");
    let got: Vec<&str> = errors.iter().map(|e| e.key.as_str()).collect();
    assert_eq!(got, vec!["selection.industry", "selection.departments"]);

    // a department of another industry
    let foreign = Selection {
        departments: vec![BILLING.into()],
        ..r.default_selection.clone()
    };
    let errors = templates::compile_pinned(&mut c, &r.pins, &foreign, Some("customer_care_agent"))
        .await
        .unwrap()
        .expect_err("mismatch");
    assert_eq!(
        compile::refusal(&errors).map(|r| r.code),
        Some("department_industry_mismatch")
    );

    // the template's own selection compiles, unlabelled
    let ok = templates::compile_pinned(
        &mut c,
        &r.pins,
        &r.default_selection,
        Some("customer_care_agent"),
    )
    .await
    .unwrap()
    .unwrap_or_else(|e| panic!("{e:#?}"));
    assert!(
        ok.effective
            .warnings
            .iter()
            .all(|w| w.code != "base_only_preview")
    );

    // the base-only preview (no profile, industry, department or job) still compiles, labelled
    let base =
        templates::compile_pinned_preview(&mut c, &[pin("base", 1)], &Selection::default(), None)
            .await
            .unwrap()
            .unwrap_or_else(|e| panic!("{e:#?}"));
    assert_eq!(base.effective.layers, ["base"]);
    assert!(
        base.effective
            .warnings
            .iter()
            .any(|w| w.code == "base_only_preview")
    );
}

// ---- PUT …/target and the cut ----------------------------------------------------------------------------------------

const ADMIN: &str = "u-kh-admin";

async fn contract(e: &TestEnv) -> (ContractJob, String) {
    let f = design_contract::new_job(e).await;
    insert_user(&e.owner, ADMIN, f.tenant, "tenant_admin").await;
    (f, person(ADMIN, &[]))
}

fn uri(f: &ContractJob, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", f.job.job_key)
}

/// A complete selection of the seed options (one area, `chat`, region eu).
fn sel() -> Value {
    json!({
        "profile": "retail_ecommerce", "industry": "retail", "departments": [CS],
        "areas": ["order-support"], "dimensions": {"channel": ["chat"]}, "region": "eu",
    })
}

async fn put_target(
    e: &TestEnv,
    f: &ContractJob,
    tok: &str,
    selection: Value,
    at: Value,
) -> (StatusCode, Value) {
    let body = json!({"selection": selection, "expected_updated_at": at});
    send(&e.app, req("PUT", &uri(f, "target"), Some(tok), body)).await
}

#[tokio::test]
async fn the_target_refuses_each_hierarchy_break_under_its_code() {
    let e = crate::common::env().await;
    let (f, tok) = contract(&e).await;
    let with = |departments: Value, industry: &str| {
        let mut s = sel();
        s["departments"] = departments;
        s["industry"] = json!(industry);
        s
    };
    for (bad, code, errors) in [
        (
            with(json!([]), "retail"),
            "hierarchy_incomplete",
            vec![("hierarchy_incomplete", "selection.departments")],
        ),
        (
            with(json!([CS]), ""),
            "hierarchy_incomplete",
            vec![("hierarchy_incomplete", "selection.industry")],
        ),
        (
            with(json!([BILLING]), "retail"),
            "department_industry_mismatch",
            vec![("department_industry_mismatch", "selection.departments[0]")],
        ),
        (
            with(json!([CS, CS]), "retail"),
            "duplicate_department",
            vec![("duplicate_department", "selection.departments[1]")],
        ),
    ] {
        let (s, b) = put_target(&e, &f, &tok, bad.clone(), Value::Null).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{bad}: {b}");
        assert_eq!(b["error"]["code"], code, "{bad}: {b}");
        assert_eq!(body_errors(&b), pairs(&errors), "{bad}");
    }
    let (s, b) = put_target(&e, &f, &tok, sel(), Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["selection"], sel(), "the saved selection round-trips");
    let (s, b) = send(&e.app, get(&uri(&f, "target"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["selection"]["departments"], json!([CS]));
    assert_eq!(b["selection"]["region"], "eu");
}

#[tokio::test]
async fn the_cut_refuses_a_stored_selection_without_departments() {
    let e = crate::common::env().await;
    let (f, tok) = contract(&e).await;
    let (s, b) = put_target(&e, &f, &tok, sel(), Value::Null).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let at = b["selection_updated_at"].clone();
    // what a pre-KH store held: the migration's CHECK refuses the legacy keys, so the incomplete new shape
    let mut broken = sel();
    broken["departments"] = json!([]);
    sqlx::query("UPDATE job_target SET selection = $2 WHERE job_id = $1")
        .bind(f.job.job_id)
        .bind(&broken)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        req(
            "POST",
            &uri(&f, "releases"),
            Some(&tok),
            json!({"apply": false, "selection_seen_at": at}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "hierarchy_incomplete", "{b}");
    assert_eq!(
        body_errors(&b),
        pairs(&[("hierarchy_incomplete", "selection.departments")])
    );
    let releases: i64 = sqlx::query_scalar("SELECT count(*) FROM job_release WHERE job_id = $1")
        .bind(f.job.job_id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(releases, 0, "nothing was cut");
}

// ---- migration 0132 ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn migration_0132_rewrites_like_the_seeder_and_refuses_the_old_shape() {
    let e = crate::common::env().await;
    for legacy in [
        json!({"profile": "retail_ecommerce", "industry": "retail", "industry_depts": ["customer_service", "returns"],
               "areas": ["faq"], "dimensions": {"channel": ["chat"], "region": ["nl", "be"]}}),
        json!({"industry": "it_services", "industry_depts": ["it_service_desk"], "profile": "internal_it"}),
        json!({"profile": null, "industry": null, "industry_depts": [], "areas": [],
               "dimensions": {"channel": [], "region": []}}),
        json!({"profile": "x", "industry": "retail", "departments": [CS]}),
        // KH-F3: only `dimensions.region` (no `industry_depts`) is rewritten too — the CHECKs refuse it
        json!({"profile": "x", "industry": "retail", "departments": [CS],
               "dimensions": {"channel": ["chat"], "region": ["nl", "be"]}}),
        json!({"industry": "retail", "departments": [CS], "region": "eu", "dimensions": {"region": []}}),
        json!({"industry": "retail", "departments": [CS], "dimensions": ["region"]}),
    ] {
        let sql: Value = sqlx::query_scalar("SELECT kh_selection_v2($1)")
            .bind(&legacy)
            .fetch_one(&e.owner)
            .await
            .unwrap();
        assert_eq!(sql, selection_v2(&legacy), "{legacy}");
    }
    // a design snapshot {"v": …} is rewritten inside
    let snap: Value = sqlx::query_scalar("SELECT kh_selections_v2($1)")
        .bind(json!({"v": {"industry": "retail", "industry_depts": ["customer_service"]}}))
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(
        snap,
        json!({"v": {"industry": "retail", "departments": [CS]}})
    );

    // the seeded stores hold no old shape, and refuse it
    let old: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM layer WHERE template_selection ? 'industry_depts') \
              + (SELECT count(*) FROM compile_reference_seed WHERE selection ? 'industry_depts')",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(old, 0);
    let err = sqlx::query(
        "UPDATE layer SET template_selection = '{\"industry_depts\": []}' WHERE key = $1",
    )
    .bind(CCA)
    .execute(&e.owner)
    .await
    .expect_err("the old shape is refused");
    assert_eq!(
        crate::common::sqlstate(&err).as_deref(),
        Some("23514"),
        "{err}"
    );
}

/// KH-F3: a stored selection with `dimensions.region` but no `industry_depts` is rewritten by 0132 like the legacy
/// ones, so its `…_selection_v2` CHECK (same predicate) never meets an unconverted row: the migration cannot fail
/// at boot on it.
#[tokio::test]
async fn migration_0132_rewrites_a_region_only_selection_so_its_check_holds() {
    let url = crate::common::fresh_database().await;
    let owner = PgPool::connect(&url).await.expect("owner pool");
    let mut before = sqlx::migrate!("./migrations");
    before.migrations =
        std::borrow::Cow::Owned(before.iter().filter(|m| m.version < 132).cloned().collect());
    before.run(&owner).await.expect("migrate up to 0131");
    let rows = [
        (
            "kh-f3-region-only",
            json!({"industry": "retail", "departments": [CS], "dimensions": {"channel": ["chat"], "region": ["nl"]}}),
            json!({"industry": "retail", "departments": [CS], "dimensions": {"channel": ["chat"]}, "region": "nl"}),
        ),
        (
            "kh-f3-region-array",
            json!({"industry": "retail", "departments": [CS], "dimensions": ["region"]}),
            json!({"industry": "retail", "departments": [CS]}),
        ),
        (
            "kh-f3-legacy",
            json!({"industry": "retail", "industry_depts": ["customer_service"], "dimensions": {"region": ["eu"]}}),
            json!({"industry": "retail", "departments": [CS], "dimensions": {}, "region": "eu"}),
        ),
    ];
    for (key, stored, _) in &rows {
        sqlx::query(
            "INSERT INTO compile_reference_seed (key, label, selection) VALUES ($1, 'KH-F3', $2)",
        )
        .bind(key)
        .bind(stored)
        .execute(&owner)
        .await
        .unwrap();
    }
    sqlx::migrate!("./migrations")
        .run(&owner)
        .await
        .expect("0132 converts every row its CHECK would refuse");
    for (key, stored, want) in &rows {
        let got: Value =
            sqlx::query_scalar("SELECT selection FROM compile_reference_seed WHERE key = $1")
                .bind(key)
                .fetch_one(&owner)
                .await
                .unwrap();
        assert_eq!(&got, want, "{key}");
        assert_eq!(
            &selection_v2(stored),
            want,
            "the seeder's twin agrees: {key}"
        );
    }
}

// ---- fix round 1 -------------------------------------------------------------------------------------------------

/// The job `key` of `tenant` as a [`ContractJob`] (write context with every role).
async fn job_of(e: &TestEnv, tenant: Uuid, key: &str) -> ContractJob {
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(tenant)
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    design_contract::contract_job(tenant, id, key)
}

/// Author `industry/utilities` and `industry_dept/utilities/billing` through the platform routes (one setting each,
/// the industry tagged first): a department-head tag with no job layer above it projects the template
/// `industry_dept/utilities/billing@v1` (key `billing`) that carries no `job/` pin.
async fn author_billing_department(e: &TestEnv) {
    let sa = |uri: String, body: Value| async move {
        let (s, b) = send(&e.app, req("POST", &uri, Some(&super_admin()), body)).await;
        assert_eq!(s, StatusCode::CREATED, "POST {uri}: {b}");
        b
    };
    // D132: a job is created with a profile, so the platform needs one for utilities
    // the test platform's base layer already exists (common::platform::offering)
    let base: uuid::Uuid = sqlx::query_scalar("SELECT id FROM layer WHERE key = 'base'")
        .fetch_one(&e.owner)
        .await
        .expect("the offering's base layer");
    sa(
        format!("/api/layers/{base}/rows"),
        json!({"kind": "profile", "key": "utilities_billing", "op": "add",
               "fields": {"label": "Utilities billing", "industry": "utilities",
                          "selection": {"order_type": ["discrete"]}}}),
    )
    .await;
    // common::env() published base before this profile was authored. Publish its new revision
    // before the industry and department tags capture their stack.
    sa("/api/platform/templates/base/tags".into(), json!({})).await;
    let industry = sa(
        "/api/layers".into(),
        json!({"scope": "industry", "industry": "utilities", "label": "Utilities"}),
    )
    .await;
    let dept = sa(
        "/api/layers".into(),
        json!({"scope": "industry_dept", "industry": "utilities", "department": "billing",
               "label": "Billing"}),
    )
    .await;
    for (layer, key, family) in [
        (&industry, "industry_default", "industry.utilities"),
        (
            &dept,
            "department_default",
            "industry_dept.utilities.billing",
        ),
    ] {
        let id = layer["id"].as_str().expect("layer id");
        sa(
            format!("/api/layers/{id}/rows"),
            json!({"kind": "job_setting", "key": key, "op": "add", "fields": {"label": key}}),
        )
        .await;
        sa(format!("/api/platform/templates/{family}/tags"), json!({})).await;
    }
}

#[tokio::test]
async fn a_job_without_a_job_pin_and_no_selection_is_refused_not_base_only() {
    // an industry_dept-head template projects without a `job/` pin: the runtime compile has no job key
    let e = crate::common::env().await;
    author_billing_department(&e).await;
    let (tenant, s, b) = create_job(&e, BILLING, 1).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let f = job_of(&e, tenant, "billing").await;
    sqlx::query("UPDATE job_target SET selection = NULL WHERE job_id = $1")
        .bind(f.job.job_id)
        .execute(&e.owner)
        .await
        .unwrap();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let d = agent_api::runtime::design::RuntimeDesign::load(
        &mut tx,
        &e.state.design,
        &f.job_ref(),
        &agent_api::design::DesignOf::Draft,
    )
    .await
    .expect("the design loads");
    tx.commit().await.unwrap();
    assert_eq!(
        d.template_errors,
        [
            "hierarchy_incomplete:selection.industry",
            "hierarchy_incomplete:selection.departments"
        ],
        "never compiled as base only"
    );
}

#[tokio::test]
async fn readiness_blocks_on_a_selection_without_the_hierarchy() {
    let e = crate::common::env().await;
    let (f, _) = contract(&e).await;
    let mut broken = sel();
    broken["departments"] = json!([]);
    sqlx::query("UPDATE job_target SET selection = $2 WHERE job_id = $1")
        .bind(f.job.job_id)
        .bind(&broken)
        .execute(&e.owner)
        .await
        .unwrap();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let items = e
        .state
        .readiness
        .draft_blocking(
            &mut tx,
            &e.state.design,
            e.state.compiler.as_ref(),
            &f.job_ref(),
        )
        .await
        .expect("readiness reads");
    tx.commit().await.unwrap();
    let got: Vec<Value> = items
        .iter()
        .map(|i| serde_json::to_value(i).unwrap())
        .filter(|i| i["code"] == "hierarchy_incomplete")
        .collect();
    assert_eq!(got.len(), 1, "{items:?}");
    assert_eq!(
        got[0]["subject"],
        "hierarchy_incomplete:selection.departments"
    );
}

#[tokio::test]
async fn the_target_keeps_the_pins_invariant() {
    let e = crate::common::env().await;
    let (f, tok) = contract(&e).await;
    let mut unpinned = sel();
    unpinned["departments"] = json!([CS, "industry_dept/retail/kh_unpinned"]);
    let (s, b) = put_target(&e, &f, &tok, unpinned, Value::Null).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "hierarchy_incomplete", "{b}");
    assert_eq!(body_errors(&b), pairs(&[("hierarchy_incomplete", "pins")]));
}

#[tokio::test]
async fn restoring_an_old_shape_selection_is_rewritten_or_refused_never_a_500() {
    use agent_api::design::{DraftRef, Place};
    let e = crate::common::env().await;
    let (f, _) = contract(&e).await;
    let jr = f.job_ref();
    let at = || Place {
        tenant: f.tenant,
        job: Some(&jr),
    };
    let target = e.state.design.target("selection").unwrap();
    // a complete pre-KH value (what a pre-0132 release snapshot holds) is rewritten into the hierarchy shape
    let old = json!({"profile": "retail_ecommerce", "industry": "retail", "industry_depts": ["customer_service"],
                     "areas": ["order-support"], "dimensions": {"channel": ["chat"], "region": ["eu"]}});
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    target
        .set(&mut tx, at(), &DraftRef::new(), &Some(old))
        .await
        .expect("a complete old shape is restored, rewritten");
    tx.commit().await.unwrap();
    let stored: Value = sqlx::query_scalar("SELECT selection FROM job_target WHERE job_id = $1")
        .bind(f.job.job_id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(stored, sel(), "rewritten into the hierarchy shape");
    // an old shape without departments is a 422, not the CHECK's 500
    let incomplete =
        json!({"profile": "retail_ecommerce", "industry": "retail", "industry_depts": []});
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let (status, code, errors) = refused(
        target
            .set(&mut tx, at(), &DraftRef::new(), &Some(incomplete))
            .await,
    );
    assert_eq!((status, code), (422, "hierarchy_incomplete"));
    assert_eq!(
        errors,
        pairs(&[("hierarchy_incomplete", "selection.departments")])
    );
}
