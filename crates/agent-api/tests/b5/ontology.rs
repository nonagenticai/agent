//! B5 T6 — the J11 ontology routes (`routes::job_design::ontology`): one test per route and its error rows, the D54
//! draft contract (one ledger row + one audit row, `draft_rev` bumped, a write back to live drops the ledger row),
//! the collapse rule, base vs tenant rows (a base relation cannot be deleted, an attribute on a base entity is B2's
//! `ontology_attribute` extension) and route coexistence with B2's `GET /api/jobs/{job}/{object}`. Real Postgres 18,
//! a Demo Retail job created through `POST /api/jobs`, through `agent_app`.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::common::design_contract::{ContractJob, DESIGNER};
use crate::common::{self, TestEnv, get, insert_user, person, send};

const OBSERVER: &str = "u-observer";

struct Fx {
    e: TestEnv,
    f: ContractJob,
    designer: String,
    observer: String,
}

async fn setup() -> Fx {
    let e = common::env().await;
    let f = crate::b5::support::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    insert_user(&e.owner, OBSERVER, f.tenant, "").await;
    Fx {
        e,
        f,
        designer: person(DESIGNER, &[]),
        observer: person(OBSERVER, &[]),
    }
}

fn uri(x: &Fx, rest: &str) -> String {
    format!("/api/jobs/{}{rest}", x.f.job.job_key)
}

async fn call(x: &Fx, method: &str, rest: &str, body: Value) -> (StatusCode, Value) {
    send(
        &x.e.app,
        common::json(method, &uri(x, rest), Some(&x.designer), body),
    )
    .await
}

async fn read(x: &Fx, rest: &str) -> (StatusCode, Value) {
    send(&x.e.app, get(&uri(x, rest), Some(&x.observer))).await
}

async fn entities(x: &Fx, include_hidden: bool) -> Vec<Value> {
    let q = if include_hidden {
        "?include_hidden=true"
    } else {
        ""
    };
    let (s, b) = read(x, &format!("/ontology/entities{q}")).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["next"], Value::Null, "{b}");
    b["items"].as_array().expect("Paged.items").clone()
}

async fn relations(x: &Fx) -> Vec<Value> {
    let (s, b) = read(x, "/ontology/relations").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    b["items"].as_array().expect("Paged.items").clone()
}

fn named<'a>(list: &'a [Value], name: &str) -> Option<&'a Value> {
    list.iter().find(|e| e["name"] == json!(name))
}

/// A base entity of the Demo Retail draft that has at least one base attribute.
async fn base_entity(x: &Fx) -> Value {
    entities(x, true)
        .await
        .into_iter()
        .find(|e| {
            e["origin"] == json!("base")
                && e["attributes"]
                    .as_array()
                    .is_some_and(|a| a.iter().any(|a| a["origin"] == json!("base")))
        })
        .expect("Demo Retail has a base entity with attributes")
}

fn warranty() -> Value {
    json!({
        "name": "Warranty",
        "label": "Warranty claim",
        "scope": "generic",
        "industry": null,
        "department": null,
        "synonyms": ["guarantee"],
        "object_key": null,
        "attributes": [{"name": "serial", "data_type": "string", "pii": false, "synonyms": []}],
    })
}

async fn create_warranty(x: &Fx) -> Value {
    let (s, b) = call(x, "POST", "/ontology/entities", warranty()).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    b
}

#[track_caller]
fn refused(got: (StatusCode, Value), status: StatusCode, code: &str) {
    assert_eq!(got.0, status, "{}", got.1);
    assert_eq!(got.1["error"]["code"], json!(code), "{}", got.1);
}

/// The job's ledger rows as (key, kind, page, label).
async fn ledger(x: &Fx) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT key, kind, page, label FROM draft_ledger WHERE job_id = $1 ORDER BY at, id",
    )
    .bind(x.f.job.job_id)
    .fetch_all(&x.e.owner)
    .await
    .unwrap()
}

async fn draft_rev(x: &Fx) -> i64 {
    sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(x.f.job.job_id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap()
}

async fn audits(x: &Fx) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM audit_event WHERE action = 'job.draft_change' AND job = $1",
    )
    .bind(&x.f.job.job_key)
    .fetch_one(&x.e.owner)
    .await
    .unwrap()
}

// ---- reads + route coexistence ------------------------------------------------------------------------------------

#[tokio::test]
async fn ontology_entities_list_base_entities_for_a_member() {
    let x = setup().await;
    let es = entities(&x, false).await;
    assert!(!es.is_empty());
    assert!(es.iter().all(|e| e["origin"] == json!("base")), "{es:?}");
    assert!(es.iter().all(|e| e["job"] == json!(x.f.job.job_key)));
    let rs = relations(&x).await;
    assert!(rs.iter().all(|r| r["origin"] == json!("base")), "{rs:?}");
}

#[tokio::test]
async fn b5_routes_win_over_b2_object_catch_all() {
    let x = setup().await;
    // B5's static paths answer B5's shapes
    let (s, b) = read(&x, "/ontology/entities").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["items"].is_array(), "{b}");
    let (s, b) = read(&x, "/ontology/relations").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["items"].is_array(), "{b}");
    // B2's catch-all still serves its objects (`_` names) …
    let (s, b) = read(&x, "/context_space").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["base"].is_array(), "{b}");
    // … and `ontology` alone is not a B2 object
    refused(
        read(&x, "/ontology").await,
        StatusCode::NOT_FOUND,
        "unknown_object",
    );
}

#[tokio::test]
async fn list_hides_hidden_unless_include_hidden() {
    let x = setup().await;
    let name = base_entity(&x).await["name"].as_str().unwrap().to_string();
    let (s, b) = call(
        &x,
        "PUT",
        &format!("/ontology/entities/{name}/hidden"),
        json!({"hidden": true}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["hidden"], json!(true));
    assert!(named(&entities(&x, false).await, &name).is_none());
    let all = entities(&x, true).await;
    assert_eq!(named(&all, &name).unwrap()["hidden"], json!(true));
}

// ---- createOntologyEntity -----------------------------------------------------------------------------------------

#[tokio::test]
async fn create_entity_is_a_tenant_entity_draft() {
    let x = setup().await;
    let rev = draft_rev(&x).await;
    let b = create_warranty(&x).await;
    assert_eq!(b["origin"], json!("tenant"));
    assert_eq!(b["label"], json!("Warranty claim"));
    assert_eq!(b["synonyms"], json!(["guarantee"]));
    assert_eq!(b["attributes"][0]["name"], json!("serial"));
    assert_eq!(b["attributes"][0]["origin"], json!("tenant"));
    assert_eq!(b["created_by"], json!(DESIGNER));
    assert!(named(&entities(&x, false).await, "Warranty").is_some());
    assert_eq!(
        ledger(&x).await,
        [(
            "entity:Warranty".to_string(),
            "entity".to_string(),
            "J11".to_string(),
            "Entity Warranty added".to_string()
        )]
    );
    assert_eq!(draft_rev(&x).await, rev + 1);
}

#[tokio::test]
async fn create_entity_refusals() {
    let x = setup().await;
    let mut bad = warranty();
    bad["name"] = json!("warranty");
    refused(
        call(&x, "POST", "/ontology/entities", bad).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_name",
    );
    let mut bad = warranty();
    bad["label"] = json!("   ");
    refused(
        call(&x, "POST", "/ontology/entities", bad).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_label",
    );
    let base = base_entity(&x).await;
    let mut dup = warranty();
    dup["name"] = base["name"].clone();
    refused(
        call(&x, "POST", "/ontology/entities", dup).await,
        StatusCode::CONFLICT,
        "entity_exists",
    );
    create_warranty(&x).await;
    refused(
        call(&x, "POST", "/ontology/entities", warranty()).await,
        StatusCode::CONFLICT,
        "entity_exists",
    );
    // a refused write records nothing
    assert_eq!(ledger(&x).await.len(), 1);
}

#[tokio::test]
async fn unknown_field_is_422_invalid_body() {
    let x = setup().await;
    let mut extra = warranty();
    extra["origin"] = json!("base");
    refused(
        call(&x, "POST", "/ontology/entities", extra).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_body",
    );
    let name = base_entity(&x).await["name"].as_str().unwrap().to_string();
    refused(
        call(
            &x,
            "POST",
            &format!("/ontology/entities/{name}/synonyms"),
            json!({"term": "x", "lang": "en"}),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_body",
    );
    refused(
        call(
            &x,
            "PUT",
            &format!("/ontology/entities/{name}/hidden"),
            json!({}),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_body",
    );
    assert!(ledger(&x).await.is_empty());
}

// ---- the D54 draft contract ---------------------------------------------------------------------------------------

#[tokio::test]
async fn observer_write_is_403() {
    let x = setup().await;
    let (s, b) = send(
        &x.e.app,
        common::json(
            "POST",
            &uri(&x, "/ontology/entities"),
            Some(&x.observer),
            warranty(),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], json!("forbidden"));
    assert_eq!(b["error"]["missing_role"], json!("job_designer"), "{b}");
    assert!(ledger(&x).await.is_empty());
}

#[tokio::test]
async fn write_records_one_ledger_row_and_one_audit_row() {
    let x = setup().await;
    let before = audits(&x).await;
    let name = base_entity(&x).await["name"].as_str().unwrap().to_string();
    let (s, b) = call(
        &x,
        "POST",
        &format!("/ontology/entities/{name}/synonyms"),
        json!({"term": "  thingamajig "}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(
        b["synonyms"]
            .as_array()
            .unwrap()
            .contains(&json!("thingamajig")),
        "{b}"
    );
    assert_eq!(
        ledger(&x).await,
        [(
            format!("synonyms:{name}"),
            "entity_synonyms".to_string(),
            "J11".to_string(),
            format!("{name}: synonym “thingamajig” added")
        )]
    );
    assert_eq!(audits(&x).await, before + 1);
    let stored: Vec<String> = sqlx::query_scalar(
        "SELECT unnest(synonyms_added) FROM ontology_entity_refinement WHERE job_id = $1 AND name = $2",
    )
    .bind(x.f.job.job_id)
    .bind(&name)
    .fetch_all(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(stored, ["thingamajig"]);
}

#[tokio::test]
async fn write_bumps_draft_rev() {
    let x = setup().await;
    let rev = draft_rev(&x).await;
    let base = base_entity(&x).await;
    let name = base["name"].as_str().unwrap();
    let (s, b) = call(
        &x,
        "PUT",
        &format!("/ontology/entities/{name}/hidden"),
        json!({"hidden": true}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(draft_rev(&x).await, rev + 1);
    let (s, b) = call(
        &x,
        "POST",
        "/ontology/relations",
        json!({
            "subject": name, "predicate": "testedBy", "object": name, "cardinality": "1:n"
        }),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(draft_rev(&x).await, rev + 2);
}

#[tokio::test]
async fn revert_to_live_drops_ledger_row() {
    let x = setup().await;
    let name = base_entity(&x).await["name"].as_str().unwrap().to_string();
    let path = format!("/ontology/entities/{name}/hidden");
    let (s, b) = call(&x, "PUT", &path, json!({"hidden": true})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ledger(&x).await.len(), 1);
    let (s, b) = call(&x, "PUT", &path, json!({"hidden": false})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["hidden"], json!(false));
    assert!(ledger(&x).await.is_empty());
    // both fields back to the base: no refinement row
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM ontology_entity_refinement WHERE job_id = $1")
            .bind(x.f.job.job_id)
            .fetch_one(&x.e.owner)
            .await
            .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn entity_edits_on_a_draft_created_entity_collapse_to_entity() {
    let x = setup().await;
    create_warranty(&x).await;
    let (s, b) = call(
        &x,
        "POST",
        "/ontology/entities/Warranty/synonyms",
        json!({"term": "claim"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = call(
        &x,
        "PUT",
        "/ontology/entities/Warranty/hidden",
        json!({"hidden": true}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let kinds: Vec<(String, String)> = ledger(&x)
        .await
        .into_iter()
        .map(|(k, kind, _, _)| (k, kind))
        .collect();
    assert_eq!(
        kinds,
        [("entity:Warranty".to_string(), "entity".to_string())]
    );
}

// ---- addOntologySynonym -------------------------------------------------------------------------------------------

#[tokio::test]
async fn add_synonym_refusals() {
    let x = setup().await;
    refused(
        call(
            &x,
            "POST",
            "/ontology/entities/Nope/synonyms",
            json!({"term": "x"}),
        )
        .await,
        StatusCode::NOT_FOUND,
        "entity_not_found",
    );
    let base = base_entity(&x).await;
    let name = base["name"].as_str().unwrap();
    let path = format!("/ontology/entities/{name}/synonyms");
    refused(
        call(&x, "POST", &path, json!({"term": "  "})).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_synonym",
    );
    create_warranty(&x).await;
    refused(
        call(
            &x,
            "POST",
            "/ontology/entities/Warranty/synonyms",
            json!({"term": "GUARANTEE"}),
        )
        .await,
        StatusCode::CONFLICT,
        "synonym_exists",
    );
    if let Some(s) = base["synonyms"].as_array().and_then(|a| a.first()) {
        refused(
            call(&x, "POST", &path, json!({"term": s})).await,
            StatusCode::CONFLICT,
            "synonym_exists",
        );
    }
}

// ---- addOntologyAttribute -----------------------------------------------------------------------------------------

#[tokio::test]
async fn attribute_on_tenant_entity_is_stored_on_the_entity() {
    let x = setup().await;
    create_warranty(&x).await;
    let (s, b) = call(
        &x,
        "POST",
        "/ontology/entities/Warranty/attributes",
        json!({"name": "purchase_date", "data_type": "date", "pii": false, "synonyms": []}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let names: Vec<&str> = b["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["serial", "purchase_date"]);
    refused(
        call(
            &x,
            "POST",
            "/ontology/entities/Warranty/attributes",
            json!({"name": "serial", "data_type": "string", "pii": false, "synonyms": []}),
        )
        .await,
        StatusCode::CONFLICT,
        "attribute_exists",
    );
    refused(
        call(
            &x,
            "POST",
            "/ontology/entities/Warranty/attributes",
            json!({"name": "PurchaseDate", "data_type": "date", "pii": false, "synonyms": []}),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_attribute",
    );
    refused(
        call(
            &x,
            "POST",
            "/ontology/entities/Nope/attributes",
            json!({"name": "x", "data_type": "string", "pii": false, "synonyms": []}),
        )
        .await,
        StatusCode::NOT_FOUND,
        "entity_not_found",
    );
}

#[tokio::test]
async fn attribute_on_base_entity_is_overlay_extension() {
    let x = setup().await;
    let base = base_entity(&x).await;
    let name = base["name"].as_str().unwrap();
    let path = format!("/ontology/entities/{name}/attributes");
    let (s, b) = call(
        &x,
        "POST",
        &path,
        json!({"name": "loyalty_tier", "data_type": "string", "pii": false, "synonyms": ["tier"]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let added = b["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == json!("loyalty_tier"))
        .expect("the new attribute is shown")
        .clone();
    assert_eq!(added["origin"], json!("tenant"));
    assert_eq!(added["synonyms"], json!(["tier"]));
    let key = format!("{name}.loyalty_tier");
    assert_eq!(
        ledger(&x).await,
        [(
            format!("overlay:ontology_attribute:{key}"),
            "overlay".to_string(),
            "J11".to_string(),
            format!("{name}: attribute loyalty_tier added")
        )]
    );
    let row: (String, Value) = sqlx::query_as(
        "SELECT kind, item FROM job_overlay_row WHERE job_id = $1 AND object = 'ontology_attribute' AND key = $2",
    )
    .bind(x.f.job.job_id)
    .bind(&key)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(row.0, "extension");
    assert_eq!(row.1["key"], json!(key));
    // no B5 row of any kind
    for table in [
        "ontology_entity",
        "ontology_entity_refinement",
        "ontology_relation",
    ] {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE job_id = $1"
        )))
        .bind(x.f.job.job_id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
        assert_eq!(n, 0, "{table}");
    }
    // a base attribute's name, and the one just added, are taken
    let base_attr = base["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["origin"] == json!("base"))
        .unwrap()["name"]
        .clone();
    for taken in [base_attr, json!("loyalty_tier")] {
        refused(
            call(
                &x,
                "POST",
                &path,
                json!({"name": taken, "data_type": "string", "pii": false, "synonyms": []}),
            )
            .await,
            StatusCode::CONFLICT,
            "attribute_exists",
        );
    }
}

// ---- setOntologyEntityHidden --------------------------------------------------------------------------------------

#[tokio::test]
async fn set_hidden_unknown_entity_is_404() {
    let x = setup().await;
    refused(
        call(
            &x,
            "PUT",
            "/ontology/entities/Nope/hidden",
            json!({"hidden": true}),
        )
        .await,
        StatusCode::NOT_FOUND,
        "entity_not_found",
    );
}

// ---- relations ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn create_and_delete_a_tenant_relation() {
    let x = setup().await;
    create_warranty(&x).await;
    let base = base_entity(&x).await;
    let other = base["name"].as_str().unwrap();
    let rel = json!({"subject": "Warranty", "predicate": "covers", "object": other, "cardinality": "n:1"});
    let (s, b) = call(&x, "POST", "/ontology/relations", rel.clone()).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["origin"], json!("tenant"));
    assert_eq!(b["cardinality"], json!("n:1"));
    // the base ontology has its own `covers` (WarrantyClaim → Product): match the whole triple
    let ours = |r: &Value| {
        r["subject"] == json!("Warranty")
            && r["predicate"] == json!("covers")
            && r["object"] == json!(other)
    };
    assert!(relations(&x).await.iter().any(ours));
    refused(
        call(&x, "POST", "/ontology/relations", rel.clone()).await,
        StatusCode::CONFLICT,
        "relation_exists",
    );
    let (s, b) = call(&x, "DELETE", "/ontology/relations", rel.clone()).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
    assert!(!relations(&x).await.iter().any(ours));
    refused(
        call(&x, "DELETE", "/ontology/relations", rel).await,
        StatusCode::NOT_FOUND,
        "relation_not_found",
    );
    // added then removed in the same draft: only the entity's ledger row is left
    let keys: Vec<String> = ledger(&x).await.into_iter().map(|r| r.0).collect();
    assert_eq!(keys, ["entity:Warranty"]);
}

#[tokio::test]
async fn create_relation_refusals() {
    let x = setup().await;
    let base = base_entity(&x).await;
    let known = base["name"].as_str().unwrap();
    refused(
        call(
            &x,
            "POST",
            "/ontology/relations",
            json!({
                "subject": "Nope", "predicate": "has", "object": known, "cardinality": "1:n"
            }),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown_subject",
    );
    refused(
        call(
            &x,
            "POST",
            "/ontology/relations",
            json!({
                "subject": known, "predicate": "has", "object": "Nope", "cardinality": "1:n"
            }),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown_object",
    );
    refused(
        call(
            &x,
            "POST",
            "/ontology/relations",
            json!({
                "subject": known, "predicate": "Has_x", "object": known, "cardinality": "1:n"
            }),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_predicate",
    );
    refused(
        call(
            &x,
            "POST",
            "/ontology/relations",
            json!({
                "subject": known, "predicate": "has", "object": known, "cardinality": "many"
            }),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_body",
    );
    if let Some(r) = relations(&x).await.first() {
        let dup = json!({
            "subject": r["subject"], "predicate": r["predicate"],
            "object": r["object"], "cardinality": r["cardinality"],
        });
        refused(
            call(&x, "POST", "/ontology/relations", dup).await,
            StatusCode::CONFLICT,
            "relation_exists",
        );
    }
    assert!(ledger(&x).await.is_empty());
}

#[tokio::test]
async fn base_relation_delete_is_409_base_row() {
    let x = setup().await;
    let r = relations(&x)
        .await
        .into_iter()
        .find(|r| r["origin"] == json!("base"))
        .expect("Demo Retail has a base relation");
    let body = json!({
        "subject": r["subject"], "predicate": r["predicate"], "object": r["object"],
        "cardinality": r["cardinality"],
    });
    refused(
        call(&x, "DELETE", "/ontology/relations", body).await,
        StatusCode::CONFLICT,
        "base_row",
    );
    assert!(ledger(&x).await.is_empty());
}
