//! B2 T6: B2's six design targets (`release::targets`: languages, setting, overlay, rule_values, rule_passage,
//! selection) and the job-creation hook (`design::on_job_created`, `design::ensure_job`). Each target runs the
//! design contract (`common::design_contract`, items 1–4 and 7) on a job created through `POST /api/jobs`; the hook
//! tests read what creation stored. Real Postgres 18 (one container for this binary, a fresh database per test),
//! through the RLS runtime role `agent_app`.
use crate::common;
use crate::rules_support;

use std::collections::BTreeSet;
use std::sync::Arc;

use agent_api::design::{self, DesignTarget, DraftRef, Registry};
use agent_api::platform::templates;
use agent_api::release::compile::{LayerCompiler, TemplateCompiler};
use common::design_contract::{self, TEMPLATE};
use common::{TestEnv, insert_tenant};
use serde_json::{Value, json};
use uuid::Uuid;

const B2_KINDS: [&str; 6] = [
    "languages",
    "setting",
    "overlay",
    "rule_values",
    "rule_passage",
    "selection",
];

fn r(pairs: &[(&str, &str)]) -> DraftRef {
    let mut m = DraftRef::new();
    for (k, v) in pairs {
        m.insert((*k).to_string(), Value::String((*v).to_string()));
    }
    m
}

fn target<'r>(reg: &'r Registry, kind: &str) -> &'r dyn DesignTarget {
    reg.target(kind).unwrap()
}

fn registry() -> Registry {
    let compiler: Arc<dyn TemplateCompiler> = Arc::new(LayerCompiler::new());
    design::registry(compiler)
}

// ---- the contract, one test per kind --------------------------------------------------------------------------------

#[tokio::test]
async fn languages_contract() {
    let e = common::env().await;
    design_contract::run(
        &e,
        "languages",
        &[(r(&[]), json!(["en", "nl"]))],
        Some("none"),
    )
    .await;
}

#[tokio::test]
async fn setting_contract() {
    let e = common::env().await;
    design_contract::run(
        &e,
        "setting",
        &[
            (
                r(&[("key", "actor")]),
                json!({"value": "Lumen & Loom Home customers", "base_hash": "sha256:0a"}),
            ),
            // an override of null hides the base value: a different value from "no override"
            (
                r(&[("key", "stance")]),
                json!({"value": null, "base_hash": null}),
            ),
        ],
        Some("release value"),
    )
    .await;
}

/// T6 review M3: a restore (`set`) writes `base_hash` back with the value, and `get` reads it.
#[tokio::test]
async fn setting_restore_keeps_base_hash() {
    let e = common::env().await;
    let f = design_contract::new_job(&e).await;
    let at = r(&[("key", "actor")]);
    let v = json!({"value": "Lumen care", "base_hash": "sha256:base-1"});
    design_contract::tracked_set(&e, &f, "setting", &at, Some(v)).await;
    let row: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT value, base_hash FROM job_setting_override WHERE job_id = $1 AND key = 'actor'",
    )
    .bind(f.job.job_id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            Some("Lumen care".to_string()),
            Some("sha256:base-1".to_string())
        )
    );
    // a restore onto the same value over another base rewrites base_hash too
    let moved = json!({"value": "Lumen care", "base_hash": "sha256:base-2"});
    let out = design_contract::tracked_set(&e, &f, "setting", &at, Some(moved.clone())).await;
    assert!(out.change.is_some(), "a new base is a draft change");
    assert_eq!(
        design_contract::get(&e, &f, "setting", &at).await,
        Some(moved)
    );
}

#[tokio::test]
async fn overlay_contract() {
    let e = common::env().await;
    design_contract::run(
        &e,
        "overlay",
        &[
            (
                r(&[("object", "context_space"), ("key", "billing")]),
                json!({
                    "key": "billing",
                    "kind": "override",
                    "item": {"key": "billing", "label": "Billing and invoices"},
                    "base_hash": "sha256:0a",
                    "content_hash": "sha256:0b",
                    "updated_by": "u-designer",
                    "updated_at": "2026-10-20T10:00:00Z",
                }),
            ),
            (
                // Activity refinements belong to the dedicated `activity` target (B8 T7).
                r(&[("object", "context_space"), ("key", "returns")]),
                json!({
                    "key": "returns",
                    "kind": "tombstone",
                    "item": null,
                    "base_hash": "sha256:0c",
                    "content_hash": null,
                    "updated_by": "u-designer",
                    "updated_at": "2026-10-20T11:30:00.250Z",
                }),
            ),
        ],
        Some("release value"),
    )
    .await;
}

#[tokio::test]
async fn rule_values_contract() {
    let e = common::env().await;
    design_contract::run(
        &e,
        "rule_values",
        &[
            (
                r(&[("id", "RET-01")]),
                json!({"max_days": "30", "fee": null}),
            ),
            (r(&[("id", "ZZZ-99")]), json!({"limit": "250"})),
        ],
        None,
    )
    .await;
}

#[tokio::test]
async fn rule_passage_contract() {
    let e = common::env().await;
    design_contract::run(
        &e,
        "rule_passage",
        &[(
            r(&[("id", "RET-01")]),
            json!({"title": "Returns policy §2", "text": "Items may be returned within 30 days."}),
        )],
        Some("no passage"),
    )
    .await;
}

#[tokio::test]
async fn selection_contract() {
    let e = common::env().await;
    design_contract::run(
        &e,
        "selection",
        &[(
            r(&[]),
            json!({
                "profile": "retail_ecommerce",
                "industry": "retail",
                "departments": ["industry_dept/retail/customer_service"],
                "areas": ["returns"],
                "dimensions": {"channel": ["chat"]}, "region": "eu",
            }),
        )],
        None,
    )
    .await;
}

// ---- texts and sameness (pure) --------------------------------------------------------------------------------------

#[test]
fn texts_and_sameness_mirror_the_fixture() {
    let reg = registry();
    let t = |k: &str| target(&reg, k);
    let none = DraftRef::new();

    assert_eq!(
        t("languages")
            .text(&Some(json!(["en", "nl"])), &none)
            .as_deref(),
        Some("en, nl")
    );
    assert_eq!(
        t("languages").text(&Some(json!([])), &none).as_deref(),
        Some("none")
    );

    let setting = t("setting");
    let hidden = json!({"value": null, "base_hash": "h"});
    assert_eq!(
        setting.text(&Some(hidden.clone()), &none).as_deref(),
        Some("hidden")
    );
    assert_eq!(
        setting
            .text(&Some(json!({"value": "Be brief", "base_hash": "h"})), &none)
            .as_deref(),
        Some("Be brief")
    );
    // no override ≠ an override of null; the base it was made over counts (a rebase is a change)
    assert!(!design::same(setting, &None, &Some(hidden)));
    let on = |h: &str| Some(json!({"value": "x", "base_hash": h}));
    assert!(design::same(setting, &on("h1"), &on("h1")));
    assert!(!design::same(setting, &on("h1"), &on("h2")));

    let overlay = t("overlay");
    let row = |label: Value, updated_by: &str| {
        json!({"key": "b", "kind": "override", "item": {"key": "b", "label": label}, "base_hash": "h",
               "content_hash": "c", "updated_by": updated_by, "updated_at": "2026-10-20T10:00:00Z"})
    };
    assert_eq!(
        overlay
            .text(&Some(row(json!("Billing"), "u-a")), &none)
            .as_deref(),
        Some("override “Billing”")
    );
    assert_eq!(
        overlay.text(&Some(row(json!(""), "u-a")), &none).as_deref(),
        Some("override")
    );
    assert_eq!(
        overlay
            .text(&Some(json!({"kind": "tombstone", "item": null})), &none)
            .as_deref(),
        Some("hidden")
    );
    // fixture proj = {kind, item, base_hash}: who wrote it and when is not part of the value
    assert!(design::same(
        overlay,
        &Some(row(json!("Billing"), "u-a")),
        &Some(row(json!("Billing"), "u-b"))
    ));
    assert!(!design::same(
        overlay,
        &Some(row(json!("Billing"), "u-a")),
        &Some(row(json!("Bills"), "u-a"))
    ));

    assert_eq!(
        t("rule_values")
            .text(&Some(json!({"max_days": "30", "fee": null})), &none)
            .as_deref(),
        Some("fee = not set · max_days = 30")
    );
    assert_eq!(
        t("rule_passage")
            .text(&Some(json!({"title": "Returns"})), &none)
            .as_deref(),
        Some("Returns")
    );

    let sel = json!({"profile": "retail_ecommerce", "industry": "retail", "departments": [],
                     "areas": ["returns", "orders"], "dimensions": {"channel": []}, "region": "eu"});
    assert_eq!(
        t("selection").text(&Some(sel), &none).as_deref(),
        Some("retail_ecommerce · area: returns, orders · region: eu")
    );
    let bare = json!({"profile": null, "areas": [], "dimensions": {"channel": []}});
    assert_eq!(
        t("selection").text(&Some(bare), &none).as_deref(),
        Some("null · ")
    );
}

// ---- the creation hook ----------------------------------------------------------------------------------------------

async fn default_selection(e: &TestEnv) -> (Value, Vec<String>) {
    default_selection_on(e, TEMPLATE.0, TEMPLATE.1).await
}

/// [`default_selection`] of the template `layer@version`, and the rule ids its default selection compiles to.
async fn default_selection_on(e: &TestEnv, layer: &str, version: i32) -> (Value, Vec<String>) {
    let mut c = e.state.db.acquire().await.unwrap();
    let t = templates::resolve_template(&mut c, layer, version)
        .await
        .expect("the offering's tag resolves");
    let compiled =
        TemplateCompiler::compile(&*e.state.compiler, &mut c, &t.default_selection, &t.pins)
            .await
            .expect("the template's default selection compiles");
    let rules = compiled.bundle.rules.iter().map(|x| x.id.clone()).collect();
    (serde_json::to_value(&t.default_selection).unwrap(), rules)
}

async fn target_selection(e: &TestEnv, job: Uuid) -> Option<Option<Value>> {
    sqlx::query_scalar("SELECT selection FROM job_target WHERE job_id = $1")
        .bind(job)
        .fetch_optional(&e.owner)
        .await
        .unwrap()
}

async fn baseline(e: &TestEnv, job: Uuid) -> (Option<Vec<String>>, Vec<(String, Option<Value>)>) {
    let kinds: Option<Vec<String>> =
        sqlx::query_scalar("SELECT baseline_kinds FROM jobs WHERE id = $1")
            .bind(job)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    let rows: Vec<(String, Option<Value>)> =
        sqlx::query_as("SELECT key, snap FROM release_baseline WHERE job_id = $1 ORDER BY key")
            .bind(job)
            .fetch_all(&e.owner)
            .await
            .unwrap();
    (kinds, rows)
}

fn snap_at(rows: &[(String, Option<Value>)], key: &str) -> Option<Option<Value>> {
    rows.iter().find(|(k, _)| k == key).map(|(_, s)| s.clone())
}

async fn ensure_job(e: &TestEnv, f: &design_contract::ContractJob) {
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    design::ensure_job(&mut tx, &e.state.design, &f.job_ref())
        .await
        .expect("ensure_job");
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn job_created_has_target_and_baseline() {
    let e = common::env().await;
    let f = rules_support::new_job(&e).await;
    let job = f.job.job_id;
    let (layer, version) = rules_support::template(&e).await;
    let (want_sel, want_rules) = default_selection_on(&e, &layer, version).await;

    // job_target holds the template's default selection
    assert_eq!(
        target_selection(&e, job).await,
        Some(Some(want_sel.clone()))
    );

    // the baseline was taken at creation, over every registered snapshot kind
    let (kinds, rows) = baseline(&e, job).await;
    let kinds: BTreeSet<String> = kinds
        .expect("baseline_kinds is set at creation")
        .into_iter()
        .collect();
    let want_kinds: BTreeSet<String> = e
        .state
        .design
        .snapshot_kinds()
        .iter()
        .map(|k| k.to_string())
        .collect();
    assert_eq!(kinds, want_kinds);
    for k in B2_KINDS {
        assert!(kinds.contains(k), "{k} is in the baseline");
    }
    assert_eq!(
        snap_at(&rows, "languages"),
        Some(Some(json!({"v": ["en"]})))
    );
    assert_eq!(
        snap_at(&rows, "selection"),
        Some(Some(json!({"v": want_sel})))
    );
    for k in ["setting:actor", "setting:stance"] {
        assert_eq!(
            snap_at(&rows, k),
            Some(None),
            "{k}: a place with no override"
        );
    }
    assert!(
        !want_rules.is_empty(),
        "the template authors business rules (rules_support)"
    );
    for id in &want_rules {
        assert_eq!(
            snap_at(&rows, &format!("rule:{id}")),
            Some(None),
            "rule:{id}"
        );
        assert_eq!(
            snap_at(&rows, &format!("rule-passage:{id}")),
            Some(None),
            "rule-passage:{id}"
        );
    }

    // idempotent: running the fallback again changes nothing
    ensure_job(&e, &f).await;
    ensure_job(&e, &f).await;
    assert_eq!(target_selection(&e, job).await, Some(Some(want_sel)));
    let (again_kinds, again_rows) = baseline(&e, job).await;
    assert_eq!(
        again_kinds.map(|k| k.into_iter().collect::<BTreeSet<_>>()),
        Some(kinds)
    );
    assert_eq!(again_rows, rows);
}

#[tokio::test]
async fn ensure_job_backfills_a_job_created_before_b2() {
    let e = common::env().await;
    let tenant = Uuid::new_v4();
    insert_tenant(&e.owner, tenant, "Before").await;
    // a job row as B1 wrote it before B2: no job_target row, no baseline
    let old: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, template_layer, template_version, created_by) \
         SELECT $1, 'old_care', 'Old care', pins, layer, version, 'test' FROM job_template_tag \
          WHERE layer = $2 AND version = $3 RETURNING id",
    )
    .bind(tenant)
    .bind(TEMPLATE.0)
    .bind(TEMPLATE.1)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let bare: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'bare', 'Bare', '[]', 'test') RETURNING id",
    )
    .bind(tenant)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(target_selection(&e, old).await, None);
    assert_eq!(baseline(&e, old).await.0, None);

    let (want_sel, _) = default_selection(&e).await;

    for (id, key, sel) in [(old, "old_care", Some(want_sel)), (bare, "bare", None)] {
        let f = design_contract::contract_job(tenant, id, key);
        ensure_job(&e, &f).await;
        assert_eq!(
            target_selection(&e, id).await,
            Some(sel.clone()),
            "{key}: job_target row"
        );
        let (kinds, rows) = baseline(&e, id).await;
        assert!(kinds.is_some(), "{key}: baseline taken");
        assert_eq!(
            snap_at(&rows, "selection"),
            Some(sel.map(|s| json!({"v": s}))),
            "{key}"
        );
        ensure_job(&e, &f).await;
        assert_eq!(
            baseline(&e, id).await,
            (kinds, rows),
            "{key}: a second call changes nothing"
        );
    }
}

#[tokio::test]
async fn rule_places_are_the_draft_bundle_slots_and_stored_rows() {
    let e = common::env().await;
    let f = rules_support::new_job(&e).await;
    let (layer, version) = rules_support::template(&e).await;
    let (_, want) = default_selection_on(&e, &layer, version).await;
    assert!(
        !want.is_empty(),
        "the template authors business rules (rules_support)"
    );
    let ids = |ps: Option<Vec<DraftRef>>| -> Vec<String> {
        ps.expect("a snapshot kind")
            .iter()
            .map(|p| p["id"].as_str().unwrap().to_string())
            .collect()
    };
    for kind in ["rule_values", "rule_passage"] {
        assert_eq!(
            ids(design_contract::places(&e, &f, kind).await),
            want,
            "{kind}"
        );
    }

    // a stored row the template does not define is a place too
    let extra = r(&[("id", "ZZZ-99")]);
    design_contract::tracked_set(&e, &f, "rule_values", &extra, Some(json!({"limit": "250"})))
        .await;
    let mut with_extra = want;
    with_extra.push("ZZZ-99".to_string());
    for kind in ["rule_values", "rule_passage"] {
        assert_eq!(
            ids(design_contract::places(&e, &f, kind).await),
            with_extra,
            "{kind}"
        );
    }

    // a job whose selection does not compile has no template slots, only its stored rows
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    sqlx::query(
        "UPDATE job_target SET selection = '{\"profile\": \"no_such_profile\"}' WHERE job_id = $1",
    )
    .bind(f.job.job_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        ids(design_contract::places(&e, &f, "rule_values").await),
        ["ZZZ-99".to_string()]
    );

    // a rule row whose values go away but whose passage stays is still a rule_passage place, and no values = None
    let passage = json!({"title": "Limits"});
    design_contract::tracked_set(&e, &f, "rule_passage", &extra, Some(passage.clone())).await;
    design_contract::tracked_set(&e, &f, "rule_values", &extra, None).await;
    assert_eq!(
        design_contract::get(&e, &f, "rule_values", &extra).await,
        None
    );
    assert_eq!(
        design_contract::get(&e, &f, "rule_passage", &extra).await,
        Some(passage)
    );
    design_contract::tracked_set(&e, &f, "rule_passage", &extra, None).await;
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM business_rule WHERE job_id = $1")
        .bind(f.job.job_id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(
        left, 0,
        "a rule row with neither values nor passage is removed"
    );
}

#[tokio::test]
async fn a_tracked_write_as_first_touch_keeps_the_template_selection() {
    // T6 review I1: a pre-B2 job whose first design touch is a tracked write (not ensure_job) still gets its
    // job_target row with the template default selection, taken into the baseline BEFORE the write.
    let e = common::env().await;
    let tenant = Uuid::new_v4();
    insert_tenant(&e.owner, tenant, "First write").await;
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, template_layer, template_version, created_by) \
         SELECT $1, 'first_write', 'First write', pins, layer, version, 'test' FROM job_template_tag \
          WHERE layer = $2 AND version = $3 RETURNING id",
    )
    .bind(tenant)
    .bind(TEMPLATE.0)
    .bind(TEMPLATE.1)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(target_selection(&e, id).await, None);
    let (want_sel, _) = default_selection(&e).await;

    let f = design_contract::contract_job(tenant, id, "first_write");
    design_contract::tracked_set(&e, &f, "languages", &r(&[]), Some(json!(["en", "nl"]))).await;

    assert_eq!(
        target_selection(&e, id).await,
        Some(Some(want_sel.clone())),
        "the first tracked write created the job_target row with the template default"
    );
    let (kinds, rows) = baseline(&e, id).await;
    assert!(kinds.is_some(), "baseline taken by the first write");
    assert_eq!(
        snap_at(&rows, "selection"),
        Some(Some(json!({"v": want_sel})))
    );
    assert_eq!(
        snap_at(&rows, "languages"),
        Some(Some(json!({"v": ["en"]}))),
        "the baseline holds the value from before the write"
    );
    ensure_job(&e, &f).await;
    assert_eq!(
        baseline(&e, id).await,
        (kinds, rows),
        "ensure_job afterwards changes nothing"
    );
}
