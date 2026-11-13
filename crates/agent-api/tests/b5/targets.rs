//! B5 T5 — B5's design targets (crate::job_design::targets, plan "Snapshot targets registered with B2 (D62)"):
//! the whole design contract (`common::design_contract`, items 1–7: fixture keys / scope / projection flag, get/set
//! round trip, `set(None)` removes the place, places cover every row, a write back to live leaves no ledger row,
//! snapshots, apply / rollback / re-apply) for each of the ten kinds B5 registers now, then the B5-specific rules:
//! `space_required` projects B2's `overlay` place, no B5 value holds overlay data, the procedure `published` hook,
//! the entity collapse rule, `proj` ignores what is not design, and the registry / `PENDING` partition. Real
//! Postgres 18 (one container for the `b5` binary, a fresh database per test), through `agent_app`.
use std::collections::BTreeSet;
use std::sync::Arc;

use agent_api::design::{self, DraftRef, JobRef, KINDS, PENDING, Place, Registry};
use agent_api::error::ApiError;
use agent_api::job_design::targets::entity_write_kind;
use agent_api::release::compile::{LayerCompiler, TemplateCompiler};
use agent_api::release::overlay::{self, GateOptionPatch};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv};

/// The ten kinds B5 registers in T5 (`gate_setting` / `role_setting` follow in T8a / T11a).
const B5_KINDS: [&str; 10] = [
    "entity",
    "entity_hidden",
    "entity_synonyms",
    "entity_attributes",
    "relation",
    "space_required",
    "procedure",
    "gate",
    "gate_question",
    "narrowing",
];

fn r(v: Value) -> DraftRef {
    v.as_object().cloned().expect("a ref is a JSON object")
}

fn registry() -> Registry {
    let compiler: Arc<dyn TemplateCompiler> = Arc::new(LayerCompiler::new());
    design::registry(compiler)
}

/// A tenant entity's value (the `entity` place of a tenant entity).
fn warranty() -> Value {
    json!({
        "name": "Warranty",
        "label": "Warranty claim",
        "scope": "generic",
        "industry": null,
        "department": null,
        "synonyms": ["guarantee"],
        "attributes": [{"name": "serial", "data_type": "string", "pii": false, "synonyms": []}],
        "hidden": false,
        "object_key": null,
    })
}

/// The seeded rows are the job's design as provisioned: drop the creation baseline so the next tracked write
/// (`design::ensure_job`) takes it again with them.
async fn rebaseline(e: &TestEnv, f: &ContractJob) {
    sqlx::query("DELETE FROM release_baseline WHERE job_id = $1")
        .bind(f.job.job_id)
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET baseline_kinds = NULL WHERE id = $1")
        .bind(f.job.job_id)
        .execute(&e.owner)
        .await
        .unwrap();
}

/// A tenant entity `Warranty` with no attributes.
fn seed_tenant_entity<'a>(e: &'a TestEnv, f: &'a ContractJob) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        sqlx::query(
            "INSERT INTO ontology_entity (tenant_id, job_id, name, label, scope, created_by) \
             VALUES ($1, $2, 'Warranty', 'Warranty claim', 'generic', 'u-designer')",
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .execute(&e.owner)
        .await
        .unwrap();
        rebaseline(e, f).await;
    })
}

/// The tenant's override row of context space `billing` (required `account_id`).
fn seed_space<'a>(e: &'a TestEnv, f: &'a ContractJob) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        sqlx::query(
            "INSERT INTO job_overlay_row \
                    (tenant_id, job_id, object, key, kind, item, base_hash, content_hash, updated_by) \
             VALUES ($1, $2, 'context_space', 'billing', 'override', $3, 'sha256:0a', 'sha256:0b', 'u-designer')",
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .bind(json!({"key": "billing", "label": "Billing", "required": ["account_id"]}))
        .execute(&e.owner)
        .await
        .unwrap();
        rebaseline(e, f).await;
    })
}

// ---- the contract, one test per kind --------------------------------------------------------------------------------

#[tokio::test]
async fn entity_full_contract() {
    let e = common::env().await;
    let base = json!({"hidden": true, "synonyms_added": ["client"], "base_hash": "sha256:0a"});
    design_contract::run_all(
        &e,
        "entity",
        &[
            (r(json!({"name": "Warranty"})), warranty()),
            (r(json!({"name": "Customer"})), base),
        ],
        Some("not in the ontology"),
    )
    .await;
}

#[tokio::test]
async fn entity_hidden_full_contract() {
    let e = common::env().await;
    design_contract::run_all(
        &e,
        "entity_hidden",
        &[(r(json!({"name": "Customer"})), json!(true))],
        Some("shown"),
    )
    .await;
}

#[tokio::test]
async fn entity_synonyms_full_contract() {
    let e = common::env().await;
    design_contract::run_all(
        &e,
        "entity_synonyms",
        &[(r(json!({"name": "Customer"})), json!(["client", "buyer"]))],
        Some("none"),
    )
    .await;
}

#[tokio::test]
async fn entity_attributes_full_contract() {
    let e = common::env().await;
    let attrs = json!([{"name": "serial", "data_type": "string", "pii": false, "synonyms": []}]);
    design_contract::run_all_seeded(
        &e,
        "entity_attributes",
        &[(r(json!({"name": "Warranty"})), attrs)],
        Some("none"),
        Some(seed_tenant_entity),
    )
    .await;
}

#[tokio::test]
async fn relation_full_contract() {
    let e = common::env().await;
    let at = r(json!({"subject": "Order", "predicate": "hasReturn", "object": "ReturnCase"}));
    design_contract::run_all(
        &e,
        "relation",
        &[(at, json!({"cardinality": "1:n"}))],
        Some("no relation"),
    )
    .await;
}

#[tokio::test]
async fn space_required_full_contract() {
    let e = common::env().await;
    design_contract::run_all_seeded(
        &e,
        "space_required",
        &[(
            r(json!({"key": "billing"})),
            json!({"required": ["account_id", "invoice_id"]}),
        )],
        Some("none"),
        Some(seed_space),
    )
    .await;
}

#[tokio::test]
async fn procedure_full_contract() {
    let e = common::env().await;
    let versions = json!([{
        "version": 1,
        "status": "DRAFT",
        "context_space": "retail:order-support:returns",
        "description": "Refund a small order",
        "reviewers": [],
        "steps": [{"op": "order.read"}],
        "released_in": null,
        "success_criteria": [{"id": "read", "fact": "steps.order.outcome", "op": "eq", "value": "ok"}],
        "tool_requirements": ["order.read"],
        "pddl_domain": "(define (domain refund))",
    }]);
    design_contract::run_all(
        &e,
        "procedure",
        &[(r(json!({"name": "refund"})), versions)],
        Some("none"),
    )
    .await;
}

#[tokio::test]
async fn gate_full_contract() {
    let e = common::env().await;
    design_contract::run_all(
        &e,
        "gate",
        &[
            (
                r(json!({"gate": "B6", "option": "nl"})),
                json!({"label": "Dutch", "description": null, "hidden": false, "base_hash": "sha256:0a"}),
            ),
            (
                r(json!({"gate": "J5", "option": null})),
                json!({"threshold_value": 0.65, "rule": null, "base_hash": null}),
            ),
        ],
        Some("template value"),
    )
    .await;
}

#[tokio::test]
async fn gate_question_full_contract() {
    let e = common::env().await;
    let q = json!({
        "key": "XQ1",
        "point": "P1",
        "type": "noul",
        "question": "Is the customer angry about the delay?",
        "options": [{"key": "yes"}, {"key": "no"}],
        "threshold": null,
        "threshold_value": 0.7,
        "fail": "open",
        "effect": null,
        "rule": {"fire_when": "yes"},
        "action": "handover",
        "tools": null,
    });
    design_contract::run_all(
        &e,
        "gate_question",
        &[(r(json!({"gate": "XQ1"})), q)],
        Some("not asked"),
    )
    .await;
}

#[tokio::test]
async fn narrowing_full_contract() {
    let e = common::env().await;
    let at =
        r(json!({"role": "assistant", "activity": "support:start_return", "op": "order.read"}));
    design_contract::run_all(
        &e,
        "narrowing",
        &[(at, json!(true))],
        Some("in the toolset"),
    )
    .await;
}

// ---- B5 rules -------------------------------------------------------------------------------------------------------

#[test]
fn registry_and_pending_partition_kinds() {
    let reg = registry();
    let registered: BTreeSet<&str> = reg.kinds().into_iter().collect();
    for k in B5_KINDS {
        assert!(registered.contains(k), "{k} is registered");
        assert!(!PENDING.contains(&k), "{k} left PENDING");
    }
    // T8a registered `gate_setting`, T11a `role_setting`, B8 T7 `activity`: nothing is PENDING any more.
    assert!(PENDING.is_empty(), "{PENDING:?}");
    let pending: BTreeSet<&str> = PENDING.iter().copied().collect();
    let union: BTreeSet<&str> = registered.union(&pending).copied().collect();
    assert_eq!(union, KINDS.iter().copied().collect::<BTreeSet<_>>());
    let projections: Vec<&str> = B5_KINDS
        .into_iter()
        .filter(|k| reg.target(k).unwrap().is_projection())
        .collect();
    assert_eq!(
        projections,
        [
            "entity_hidden",
            "entity_synonyms",
            "entity_attributes",
            "space_required"
        ]
    );
}

#[test]
fn proj_ignores_non_design_fields() {
    let reg = registry();
    let same = |kind: &str, a: Value, b: Value| {
        let t = reg.target(kind).unwrap();
        t.proj(&Some(a)) == t.proj(&Some(b))
    };
    // tenant entity: scope, industry, object_key are not compared
    let mut other = warranty();
    other["scope"] = json!("industry");
    other["object_key"] = json!("warranty_claim");
    assert!(same("entity", warranty(), other));
    let mut relabelled = warranty();
    relabelled["label"] = json!("Guarantee");
    assert!(!same("entity", warranty(), relabelled));
    // base refinement: the base hash is not design
    assert!(same(
        "entity",
        json!({"hidden": true, "synonyms_added": [], "base_hash": "a"}),
        json!({"hidden": true, "synonyms_added": [], "base_hash": "b"}),
    ));
    // gate: base_hash ignored, label compared
    assert!(same(
        "gate",
        json!({"label": "Dutch", "description": null, "hidden": false, "base_hash": "a"}),
        json!({"label": "Dutch", "description": null, "hidden": false, "base_hash": "b"}),
    ));
    assert!(!same(
        "gate",
        json!({"label": "Dutch", "hidden": false, "base_hash": "a"}),
        json!({"label": "Nederlands", "hidden": false, "base_hash": "a"}),
    ));
    // procedure: description and released_in ignored; order of versions ignored
    let v = |n: i64, desc: &str, rel: Value| {
        json!({"version": n, "status": "DRAFT", "context_space": "x", "description": desc, "reviewers": [],
               "steps": [], "released_in": rel})
    };
    assert!(same(
        "procedure",
        json!([v(1, "a", Value::Null), v(2, "a", Value::Null)]),
        json!([v(2, "b", Value::Null), v(1, "b", json!("cfg-1"))]),
    ));
    // space_required: only the slots
    assert!(same(
        "space_required",
        json!({"required": ["a"]}),
        json!({"required": ["a"], "note": "x"}),
    ));
    // narrowing: there or not
    assert!(same("narrowing", json!(true), json!(false)));
}

#[test]
fn space_required_projects_overlay_required() {
    let reg = registry();
    let t = reg.target("space_required").unwrap();
    let at = r(json!({"key": "billing"}));
    assert_eq!(t.key(&at), "space:billing");
    let (pkind, pref) = t.parent(&at).expect("a projection has a parent");
    assert_eq!(pkind, "overlay");
    assert_eq!(
        pref,
        r(json!({"object": "context_space", "key": "billing"}))
    );
    assert_eq!(
        reg.target("overlay").unwrap().key(&pref),
        "overlay:context_space:billing"
    );
    let row = json!({"key": "billing", "kind": "override", "item": {"key": "billing", "required": ["account_id"]},
                     "base_hash": "h"});
    assert_eq!(
        t.project(&Some(row), &at),
        Some(json!({"required": ["account_id"]}))
    );
    let tomb = json!({"key": "billing", "kind": "tombstone", "item": null, "base_hash": "h"});
    assert_eq!(t.project(&Some(tomb), &at), None);
    let label_only =
        json!({"key": "billing", "kind": "override", "item": {"key": "billing", "label": "B"}});
    assert_eq!(t.project(&Some(label_only), &at), None);
}

#[tokio::test]
async fn no_b5_value_contains_overlay_data() {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    // a refined base entity, and an attribute the tenant added to it (a B2 overlay extension)
    sqlx::query(
        "INSERT INTO ontology_entity_refinement (tenant_id, job_id, name, hidden, synonyms_added, base_hash, \
                                                 updated_by) \
         VALUES ($1, $2, 'Customer', false, '{client}', 'sha256:0a', 'u-designer')",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO job_overlay_row (tenant_id, job_id, object, key, kind, item, content_hash, updated_by) \
         VALUES ($1, $2, 'ontology_attribute', 'Customer.loyalty_tier', 'extension', $3, 'sha256:0c', 'u-designer')",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(json!({"key": "Customer.loyalty_tier", "data_type": "string"}))
    .execute(&e.owner)
    .await
    .unwrap();
    let at = r(json!({"name": "Customer"}));
    let v = design_contract::get(&e, &f, "entity", &at)
        .await
        .expect("the refined base entity is a place");
    let keys: BTreeSet<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        BTreeSet::from(["base_hash", "hidden", "synonyms_added"])
    );
    assert!(!v.to_string().contains("loyalty_tier"), "{v}");
    assert_eq!(
        design_contract::get(&e, &f, "entity_attributes", &at).await,
        None
    );
    // the attribute is the overlay place's, and only there
    let o = design_contract::get(
        &e,
        &f,
        "overlay",
        &r(json!({"object": "ontology_attribute", "key": "Customer.loyalty_tier"})),
    )
    .await;
    assert!(o.is_some());
}

#[tokio::test]
async fn procedure_published_hook_sets_released_in() {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    for (version, status) in [(1, "PUBLISHED"), (2, "DRAFT")] {
        sqlx::query(
            "INSERT INTO procedure_version (tenant_id, job_id, name, version, status, context_space, description, \
                                            steps, created_by) \
             VALUES ($1, $2, 'refund', $3, $4, 'x', 'Refund', '[]', 'u-designer')",
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .bind(version)
        .bind(status)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let jr = f.job_ref();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let at = Place {
        tenant: f.tenant,
        job: Some(&jr),
    };
    e.state
        .design
        .target("procedure")
        .unwrap()
        .published(
            &mut tx,
            at,
            &r(json!({"name": "refund"})),
            "cfg-2026-10-22.1",
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let rows: Vec<(i32, Option<String>)> = sqlx::query_as(
        "SELECT version, released_in FROM procedure_version WHERE job_id = $1 ORDER BY version",
    )
    .bind(f.job.job_id)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![(1, Some("cfg-2026-10-22.1".to_string())), (2, None)]
    );
}

#[tokio::test]
async fn entity_edit_on_draft_created_entity_collapses_to_entity_kind() {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    design_contract::tracked_set(
        &e,
        &f,
        "entity",
        &r(json!({"name": "Warranty"})),
        Some(warranty()),
    )
    .await;
    let jr: JobRef = f.job_ref();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    assert_eq!(
        entity_write_kind(&mut tx, &jr, "Warranty", "entity_hidden")
            .await
            .unwrap(),
        "entity"
    );
    assert_eq!(
        entity_write_kind(&mut tx, &jr, "Customer", "entity_synonyms")
            .await
            .unwrap(),
        "entity_synonyms"
    );
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn save_gate_option_refuses_before_writing() {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let empty = overlay::save_gate_option(
        &mut tx,
        &e.state.design,
        &*e.state.compiler,
        &f.job,
        "GS · exit",
        GateOptionPatch::default(),
    )
    .await;
    assert!(
        matches!(empty, Err(ApiError::Unprocessable("invalid_body", _))),
        "{empty:?}"
    );
    let missing = overlay::save_gate_option(
        &mut tx,
        &e.state.design,
        &*e.state.compiler,
        &f.job,
        "ZZ · nothing",
        GateOptionPatch {
            hidden: Some(true),
            ..GateOptionPatch::default()
        },
    )
    .await;
    assert!(
        matches!(missing, Err(ApiError::Missing("option_not_found", _))),
        "{missing:?}"
    );
    tx.rollback().await.unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM draft_ledger WHERE job_id = $1")
        .bind(f.job.job_id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}
