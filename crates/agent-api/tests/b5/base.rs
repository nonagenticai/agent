//! B5 T4 — base readers (`job_design::base`: the job's bases at its design's pins, read, never stored) and the
//! upgrade conflict detector (`job_design::conflicts::JobDesignDetector`, registered in
//! `release::conflicts::registry()`). Real Postgres 18, a Demo Retail job created through `POST /api/jobs`.
use std::collections::BTreeSet;
use std::sync::Arc;

use agent_api::design::read::typed;
use agent_api::design::{DesignOf, DraftRef};
use agent_api::job_design::base::{self, Bases, apply_languages, gate_options_of, job_layer};
use agent_api::job_design::conflicts::JobDesignDetector;
use agent_api::platform::templates::{self, TemplatePin};
use agent_api::release::compile::job_key;
use agent_api::release::conflicts::{ConflictDetector, NewConflict};
use agent_model::compile::{Compiled, Selection};
use serde_json::{Value, json};

use crate::common::design_contract::{self, ContractJob, cuttable_selection};
use crate::common::{self, TestEnv};

/// `base::at(of)` in a job-scoped transaction (as `agent_app`, through RLS).
async fn bases(e: &TestEnv, f: &ContractJob, of: DesignOf) -> Arc<Bases> {
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let b = base::at(
        &mut tx,
        &e.state.design,
        e.state.compiler.as_ref(),
        &f.job_ref(),
        &of,
    )
    .await
    .unwrap_or_else(|err| panic!("base::at {of:?}: {err:?}"));
    tx.commit().await.unwrap();
    b
}

/// B6a's compile at `pins` / `sel` (`Err` = the template's error codes).
async fn compiled(
    e: &TestEnv,
    f: &ContractJob,
    pins: &[TemplatePin],
    sel: &Selection,
) -> Result<Compiled, Vec<String>> {
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let out = templates::compile_pinned(&mut tx, pins, sel, job_key(pins))
        .await
        .unwrap_or_else(|err| panic!("compile_pinned: {err:?}"))
        .map_err(|es| es.iter().map(|e| format!("{}:{}", e.code, e.key)).collect());
    tx.commit().await.unwrap();
    out
}

async fn job_pins(e: &TestEnv, f: &ContractJob) -> Vec<TemplatePin> {
    let v: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1")
        .bind(f.job.job_id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    serde_json::from_value(v).unwrap()
}

async fn release_row(e: &TestEnv, f: &ContractJob, id: &str) -> (Vec<TemplatePin>, Value) {
    let (pins, bundle): (Value, Value) = sqlx::query_as(
        "SELECT pins, bundle FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    (serde_json::from_value(pins).unwrap(), bundle)
}

fn cuttable() -> Selection {
    serde_json::from_value(cuttable_selection()).unwrap()
}

/// A Demo Retail job whose draft selection is [`cuttable_selection`].
async fn job_with_selection(e: &TestEnv) -> ContractJob {
    let f = crate::b5::support::new_job(e).await;
    select_cuttable(e, f).await
}

/// [`job_with_selection`] on the newest customer-care template: the stack that holds seed v8's activity rows
/// (their operations), which the older tags predate.
async fn job_with_selection_latest(e: &TestEnv) -> ContractJob {
    let f = crate::b5::support::new_job(e).await;
    select_cuttable(e, f).await
}

async fn select_cuttable(e: &TestEnv, f: ContractJob) -> ContractJob {
    design_contract::write(
        e,
        &f,
        "selection",
        &DraftRef::new(),
        Some(cuttable_selection()),
    )
    .await;
    f
}

/// The stack of `job/customer_care_agent@2` (seed `tags.json`): a pin set other than the job's `@4`.
fn older_pins() -> Vec<TemplatePin> {
    serde_json::from_value(json!([
        {"layer": "base", "version": 2},
        {"layer": "industry/retail", "version": 1},
        {"layer": "industry_dept/retail/customer_service", "version": 1},
        {"layer": "job/customer_care_agent", "version": 2},
    ]))
    .unwrap()
}

/// `items` without their `row_id`: a row id belongs to one database, and the process-wide compile memo
/// (`job_design::base`, keyed by pins, selection and table counts) can hand a test the compile of an identically
/// authored sibling database, so the ids are not comparable here.
fn sans_row_ids(
    items: &[agent_model::compile::CatalogueItem],
) -> Vec<agent_model::compile::CatalogueItem> {
    items
        .iter()
        .cloned()
        .map(|mut i| {
            i.row_id.clear();
            i
        })
        .collect()
}

/// `b`'s parts equal `c`'s (the compile at the same pins and selection), B6 rewritten to `b.languages`.
fn assert_same_as_compile(b: &Bases, c: &Compiled) {
    assert!(b.template_errors.is_empty(), "{:?}", b.template_errors);
    assert_eq!(b.effective_hash, c.effective.hash);
    assert_eq!(b.entities, c.effective.entities);
    assert_eq!(b.attributes, c.effective.attributes);
    assert_eq!(b.relations, c.effective.relations);
    assert_eq!(b.context_spaces, c.effective.job_content.context_spaces);
    assert_eq!(b.procedures, c.effective.job_content.procedures);
    assert_eq!(
        sans_row_ids(&b.agent_roles),
        sans_row_ids(&c.catalogue.agent_roles)
    );
    assert_eq!(b.operations, c.catalogue.operations);
    let mut gates = c.catalogue.gates.clone();
    apply_languages(&mut gates, &b.languages);
    assert_eq!(sans_row_ids(&b.gates), sans_row_ids(&gates));
    assert!(!b.entities.is_empty() && !b.gates.is_empty() && !b.agent_roles.is_empty());
    // options included: the library row's options travel with the gate
    assert!(b.gates.iter().any(|g| gate_options_of(g).next().is_some()));
}

#[tokio::test]
async fn draft_bases_equal_compile_pinned_at_job_pins() {
    let e = common::env().await;
    let f = job_with_selection(&e).await;
    let b = bases(&e, &f, DesignOf::Draft).await;
    let pins = job_pins(&e, &f).await;
    assert_eq!(b.pins, pins);
    assert_eq!(b.selection, cuttable());
    assert_eq!(b.release_id, None);
    let c = compiled(&e, &f, &pins, &cuttable())
        .await
        .expect("the Demo Retail template compiles");
    assert_same_as_compile(&b, &c);
}

#[tokio::test]
async fn live_bases_come_from_release_pins() {
    let e = common::env().await;
    let f = crate::b5::support::new_job(&e).await;
    let id = crate::b5::support::publish(&e, &f).await;
    let (release_pins, _) = release_row(&e, &f, &id).await;
    let live = bases(&e, &f, DesignOf::Live).await;
    assert_eq!(live.release_id.as_deref(), Some(id.as_str()));
    assert_eq!(live.pins, release_pins);
    let at_cut = compiled(&e, &f, &release_pins, &cuttable())
        .await
        .expect("compiles");
    assert_same_as_compile(&live, &at_cut);
    assert_eq!(*bases(&e, &f, DesignOf::Release(id.clone())).await, *live);

    // another template stack after the cut: Production keeps the release's bases, the draft follows the job's pins
    let other = older_pins();
    assert_ne!(other, release_pins);
    sqlx::query("UPDATE jobs SET pins = $2 WHERE id = $1")
        .bind(f.job.job_id)
        .bind(json!(other))
        .execute(&e.owner)
        .await
        .unwrap();
    let live_after = bases(&e, &f, DesignOf::Live).await;
    assert_eq!(*live_after, *live, "Live is the release's, not jobs.pins");
    let draft = bases(&e, &f, DesignOf::Draft).await;
    assert_eq!(draft.pins, other);
    match compiled(&e, &f, &other, &cuttable()).await {
        Ok(c) => assert_eq!(draft.effective_hash, c.effective.hash),
        Err(errors) => assert!(!draft.template_errors.is_empty(), "{errors:?}"),
    }
    assert_ne!(draft.effective_hash, live.effective_hash);
}

#[tokio::test]
async fn never_released_live_uses_job_pins_and_baseline_selection() {
    let e = common::env().await;
    let f = job_with_selection(&e).await;
    let jr = f.job_ref();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let baseline = typed::<Selection>(
        &mut tx,
        &e.state.design,
        &jr,
        &DesignOf::Live,
        "selection",
        &DraftRef::new(),
    )
    .await
    .unwrap()
    .unwrap_or_default();
    tx.commit().await.unwrap();
    let live = bases(&e, &f, DesignOf::Live).await;
    assert_eq!(live.release_id, None);
    assert_eq!(live.pins, job_pins(&e, &f).await);
    assert_eq!(
        live.selection, baseline,
        "Live of a job never released = its baseline selection"
    );
    let draft = bases(&e, &f, DesignOf::Draft).await;
    assert_eq!(draft.selection, cuttable(), "Draft = the working selection");
}

/// R2: the KH fold of the pinned `activity` rows is the one source of the activities and of their operations.
#[tokio::test]
async fn activities_and_ops_come_from_the_fold() {
    let e = common::env().await;
    let f = job_with_selection_latest(&e).await;
    let b = bases(&e, &f, DesignOf::Draft).await;
    assert!(!b.activities.is_empty(), "the fold has activities");
    assert_eq!(job_layer(&b.pins), Some("job/customer_care_agent"));
    let pins = job_pins(&e, &f).await;
    let c = compiled(&e, &f, &pins, &b.selection)
        .await
        .expect("the demo job's template compiles");
    // the draft's activities are the fold's, every one with a stage, in key order
    let want: Vec<&str> = c
        .knowledge
        .activities
        .iter()
        .filter(|a| a.stage != "*")
        .map(|a| a.key.as_str())
        .collect();
    let got: Vec<&str> = b.activities.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(got, want);
    // an activity whose row names operations has exactly those
    let mut with_ops = 0;
    for a in &b.activities {
        assert_eq!(a.item["key"], a.key.as_str());
        let folded = c
            .knowledge
            .activities
            .iter()
            .find(|x| x.key == a.key)
            .unwrap();
        if !folded.operations.is_empty() {
            let want: Vec<(&str, Option<&str>, bool)> = folded
                .operations
                .iter()
                .map(|o| (o.op.as_str(), o.variant.as_deref(), o.required))
                .collect();
            let got: Vec<(&str, Option<&str>, bool)> = a
                .ops
                .iter()
                .map(|o| (o.op.as_str(), o.variant.as_deref(), o.required))
                .collect();
            assert_eq!(got, want, "{}", a.key);
            with_ops += 1;
        }
    }
    assert!(
        with_ops > 0,
        "seed v8 gives Demo Retail activities operations"
    );
    let cancel = b
        .activity("change:cancel_order")
        .expect("the fold has change:cancel_order");
    assert!(
        cancel
            .ops
            .iter()
            .any(|o| o.op == "order.cancel" && !o.required)
    );
    let track = b
        .activity("delivery:where_is_my_order")
        .expect("the fold has delivery:where_is_my_order");
    assert!(
        track
            .ops
            .iter()
            .any(|o| o.op == "delivery.read" && o.variant.as_deref() == Some("by_tracking_ref"))
    );
    assert!(b.activity("nope:nope").is_none());
}

#[tokio::test]
async fn b6_language_options_are_job_languages_plus_other() {
    let e = common::env().await;
    let f = job_with_selection(&e).await;
    design_contract::write(
        &e,
        &f,
        "languages",
        &DraftRef::new(),
        Some(json!(["en", "de"])),
    )
    .await;
    let draft = bases(&e, &f, DesignOf::Draft).await;
    assert_eq!(draft.languages, ["en", "de"]);
    let b6 = draft.gate(base::LANGUAGE_GATE).expect("the library has B6");
    let keys: Vec<&str> = gate_options_of(b6)
        .filter_map(|o| o["key"].as_str())
        .collect();
    assert_eq!(keys, ["en", "de", "other"]);
    let de = draft.gate_option("B6", "de").unwrap();
    assert_eq!(de["label"], "German");
    // Live (the baseline) follows its own languages, not the draft's
    let live = bases(&e, &f, DesignOf::Live).await;
    let live_keys: Vec<&str> = gate_options_of(live.gate("B6").unwrap())
        .filter_map(|o| o["key"].as_str())
        .collect();
    if !live.languages.is_empty() {
        let mut want: Vec<&str> = live.languages.iter().map(String::as_str).collect();
        want.push("other");
        assert_eq!(live_keys, want);
    } else {
        assert!(live_keys.contains(&"other"), "{live_keys:?}");
    }
}

#[tokio::test]
async fn generated_gate_options_come_from_bundle_gate_option() {
    let e = common::env().await;
    let f = job_with_selection(&e).await;
    let draft = bases(&e, &f, DesignOf::Draft).await;
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let bundle = e
        .state
        .compiler
        .compile(&mut tx, &draft.selection, &draft.pins)
        .await
        .unwrap_or_else(|err| panic!("compile: {err:?}"))
        .bundle;
    tx.commit().await.unwrap();
    assert!(!draft.gate_options.is_empty());
    assert_eq!(draft.gate_options, bundle.objects.gate_option);
    for o in &draft.gate_options {
        assert!(o["key"].as_str().unwrap().contains(" · "), "{o:?}");
        assert_eq!(o["generated"], true);
    }
    // a released job reads them from the stored bundle
    let id = crate::b5::support::publish(&e, &f).await;
    let (_, stored) = release_row(&e, &f, &id).await;
    let live = bases(&e, &f, DesignOf::Live).await;
    let want: Vec<Value> = stored["objects"]["gate_option"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let got: Vec<Value> = live
        .gate_options
        .iter()
        .cloned()
        .map(Value::Object)
        .collect();
    assert_eq!(got, want);
}

// ---- the upgrade conflict detector ------------------------------------------------------------------------------

/// A published Demo Retail job, its release id and the bases at that release.
async fn released(e: &TestEnv) -> (ContractJob, String, Arc<Bases>) {
    let f = crate::b5::support::new_job(e).await;
    let id = crate::b5::support::publish(e, &f).await;
    let b = bases(e, &f, DesignOf::Release(id.clone())).await;
    assert!(b.template_errors.is_empty(), "{:?}", b.template_errors);
    (f, id, b)
}

async fn exec(e: &TestEnv, f: &ContractJob, sql: &str, binds: &[&str]) {
    let mut q = sqlx::query(sqlx::AssertSqlSafe(sql.to_string()))
        .bind(f.tenant)
        .bind(f.job.job_id);
    for b in binds {
        q = q.bind(*b);
    }
    q.execute(&e.owner)
        .await
        .unwrap_or_else(|err| panic!("{sql}: {err}"));
}

/// B5's detector on its own, and through the production registry (`AppState.conflict_detectors`).
async fn detect(e: &TestEnv, f: &ContractJob, id: &str) -> Vec<(String, String, String)> {
    let jr = f.job_ref();
    let (_, bundle) = release_row(e, f, id).await;
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let own: Vec<NewConflict> = JobDesignDetector
        .detect(&mut tx, &jr, None, &bundle, id)
        .await
        .unwrap();
    let all = e
        .state
        .conflict_detectors
        .detect(&mut tx, &jr, None, &bundle, id)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let triple = |c: &NewConflict| (c.kind.clone(), c.object.clone(), c.key.clone());
    let own: Vec<_> = own.iter().map(triple).collect();
    let all: BTreeSet<_> = all.iter().map(triple).collect();
    for c in &own {
        assert!(all.contains(c), "the registry runs B5's detector: {c:?}");
    }
    own
}

fn t(kind: &str, object: &str, key: &str) -> (String, String, String) {
    (kind.into(), object.into(), key.into())
}

const REFINE_ENTITY: &str = "INSERT INTO ontology_entity_refinement (tenant_id, job_id, name, base_hash, updated_by) \
                             VALUES ($1, $2, $3, $4, 'u-b5')";
const REFINE_OPTION: &str = "INSERT INTO job_gate_option_refinement \
                                    (tenant_id, job_id, gate, option, label, base_hash, updated_by) \
                             VALUES ($1, $2, $3, $4, 'x', $5, 'u-b5')";
const NARROW: &str = "INSERT INTO toolset_narrowing (tenant_id, job_id, role, activity, op, updated_by) \
                      VALUES ($1, $2, $3, $4, $5, 'u-b5')";

#[tokio::test]
async fn upgrade_reports_parent_missing_for_refined_base_entity() {
    let e = common::env().await;
    let (f, id, _) = released(&e).await;
    assert!(
        detect(&e, &f, &id).await.is_empty(),
        "no B5 rows, no B5 conflicts"
    );
    exec(&e, &f, REFINE_ENTITY, &["Zzgone", "sha256:x"]).await;
    assert_eq!(
        detect(&e, &f, &id).await,
        [t("parent_missing", "entity", "Zzgone")]
    );
}

#[tokio::test]
async fn upgrade_reports_parent_missing_for_option_refinement() {
    let e = common::env().await;
    let (f, id, b) = released(&e).await;
    let gate = b
        .gates
        .iter()
        .find(|g| gate_options_of(g).next().is_some())
        .expect("a gate with options")
        .key
        .clone();
    exec(&e, &f, REFINE_OPTION, &["ZZ9", "a", "sha256:x"]).await;
    exec(
        &e,
        &f,
        REFINE_OPTION,
        &[gate.as_str(), "zz_gone", "sha256:x"],
    )
    .await;
    let got: BTreeSet<_> = detect(&e, &f, &id).await.into_iter().collect();
    let want: BTreeSet<_> = [
        t("parent_missing", "gate", "ZZ9:a"),
        t("parent_missing", "gate", &format!("{gate}:zz_gone")),
    ]
    .into();
    assert_eq!(got, want);
}

#[tokio::test]
async fn upgrade_reports_parent_missing_for_narrowed_role_or_activity() {
    let e = common::env().await;
    let (f, id, b) = released(&e).await;
    let role = b.agent_roles.first().expect("a role").key.clone();
    let activity = b.activities.first().expect("an activity").key.clone();
    exec(
        &e,
        &f,
        NARROW,
        &["ghost", activity.as_str(), "order.cancel"],
    )
    .await;
    exec(&e, &f, NARROW, &[role.as_str(), "zz:gone", "order.cancel"]).await;
    // a narrowing whose role and activity exist raises nothing
    exec(
        &e,
        &f,
        NARROW,
        &[role.as_str(), activity.as_str(), "order.cancel"],
    )
    .await;
    let got: BTreeSet<_> = detect(&e, &f, &id).await.into_iter().collect();
    let want: BTreeSet<_> = [
        t(
            "parent_missing",
            "narrowing",
            &format!("ghost:{activity}:order.cancel"),
        ),
        t(
            "parent_missing",
            "narrowing",
            &format!("{role}:zz:gone:order.cancel"),
        ),
    ]
    .into();
    assert_eq!(got, want);
}

#[tokio::test]
async fn upgrade_reports_base_changed_when_base_hash_differs() {
    let e = common::env().await;
    let (f, id, b) = released(&e).await;
    let (name, item) = b.entities.iter().next().expect("a base entity");
    let fresh = base::entity_hash(item).unwrap();
    exec(&e, &f, REFINE_ENTITY, &[name.as_str(), fresh.as_str()]).await;
    assert!(
        detect(&e, &f, &id).await.is_empty(),
        "the refinement's base is current"
    );
    sqlx::query(
        "UPDATE ontology_entity_refinement SET base_hash = 'sha256:stale' WHERE job_id = $1",
    )
    .bind(f.job.job_id)
    .execute(&e.owner)
    .await
    .unwrap();
    // an option refinement made over another version of a library option
    let (gate, option) = b
        .gates
        .iter()
        .find_map(|g| {
            let o = gate_options_of(g).find(|o| o.get("generated") != Some(&Value::Bool(true)))?;
            Some((g.key.clone(), o["key"].as_str()?.to_owned()))
        })
        .expect("a library (non-generated) option");
    exec(
        &e,
        &f,
        REFINE_OPTION,
        &[gate.as_str(), option.as_str(), "sha256:stale"],
    )
    .await;
    let got: BTreeSet<_> = detect(&e, &f, &id).await.into_iter().collect();
    let want: BTreeSet<_> = [
        t("base_changed", "entity", name),
        t("base_changed", "gate", &format!("{gate}:{option}")),
    ]
    .into();
    assert_eq!(got, want);
}

#[tokio::test]
async fn base_shadowed_when_template_adds_tenant_entity_name_or_gate_key() {
    let e = common::env().await;
    let (f, id, b) = released(&e).await;
    let name = b.entities.keys().next().expect("a base entity").clone();
    let gate = b
        .gates
        .iter()
        .map(|g| g.key.clone())
        .find(|k| {
            k.len() >= 2
                && k.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        })
        .expect("a library gate key");
    exec(
        &e,
        &f,
        "INSERT INTO ontology_entity (tenant_id, job_id, name, label, scope, created_by) \
         VALUES ($1, $2, $3, $3, 'generic', 'u-b5')",
        &[name.as_str()],
    )
    .await;
    exec(
        &e,
        &f,
        "INSERT INTO job_gate_question \
                (tenant_id, job_id, key, point, question, options, fail, rule, action, updated_by) \
         VALUES ($1, $2, $3, 'P1', 'Is this a shadowed question?', \
                 '{\"true\": \"yes\", \"false\": \"no\"}'::jsonb, 'open', '{}'::jsonb, 'annotate', 'u-b5')",
        &[gate.as_str()],
    )
    .await;
    let got: BTreeSet<_> = detect(&e, &f, &id).await.into_iter().collect();
    let want: BTreeSet<_> = [
        t("base_shadowed", "entity", &name),
        t("base_shadowed", "gate_question", &gate),
    ]
    .into();
    assert_eq!(got, want);
}

#[tokio::test]
async fn no_b5_rows_written_on_job_create() {
    let e = common::env().await;
    let f = crate::b5::support::new_job(&e).await;
    for table in [
        "ontology_entity",
        "ontology_entity_refinement",
        "ontology_relation",
        "procedure_version",
        "procedure_history",
        "job_gate_question",
        "job_gate_option_refinement",
        "job_gate_rule_refinement",
        "gate_evaluation",
        "gate_answer",
        "toolset_narrowing",
    ] {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {table} WHERE job_id = $1"
        )))
        .bind(f.job.job_id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        assert_eq!(
            n, 0,
            "{table}: B5 materialises nothing at job creation (R1)"
        );
    }
}

#[tokio::test]
async fn release_conflict_accepts_b5_objects_and_still_refuses_unknown_ones() {
    // 0141 widens 0046's object CHECK: B5's detector output is storable; anything else stays refused (23514)
    let e = common::env().await;
    let f = crate::b5::support::new_job(&e).await;
    let id = crate::b5::support::publish(&e, &f).await;
    let insert = |object: &'static str| {
        sqlx::query(
            "INSERT INTO release_conflict (tenant_id, job_id, release_id, kind, object, key, tenant_row, detail) \
             VALUES ($1, $2, $3, 'parent_missing', $4, 'k', 'override', 'd')",
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .bind(id.clone())
        .bind(object)
    };
    for object in [
        "entity",
        "gate",
        "gate_question",
        "narrowing",
        "procedure",
        "activity",
        "context_space",
        "ontology_attribute",
        "gate_option",
        "knowledge_store",
        "job_setting",
    ] {
        insert(object)
            .execute(&e.owner)
            .await
            .unwrap_or_else(|err| panic!("{object}: {err}"));
    }
    let err = insert("relation")
        .execute(&e.owner)
        .await
        .expect_err("an unknown object is refused");
    assert_eq!(common::sqlstate(&err).as_deref(), Some("23514"), "{err}");
}
