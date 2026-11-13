//! KH T3 (D107, D112, D115): knowledge kinds at every layer, the archetype fold, THE fold order, sibling departments
//! (`department_conflict`), pack applicability (`pack_kind_undeclared`, `pack_not_applicable`), the pack version
//! conflict of two departments and the upgrade rules of a job's pack tombstones (`replaces`, orphaned, wildcard).
//! Pure tests drive `agent_model::compile` directly; the database tests go through the pinned tags, the template
//! seeding (`knowledge::kinds`), the row write (`write_revision`) and the release compiler.
use agent_api::audit::Actor;
use agent_api::error::ApiError;
use agent_api::knowledge::kinds;
use agent_api::knowledge::packs::template::{self as packs, PackRef};
use agent_api::platform::db::{rows, tags};
use agent_api::platform::hooks::NoHooks;
use agent_api::platform::templates::{self, TemplatePin};
use agent_api::platform_db::begin_platform;
use agent_api::release::compile::{LayerCompiler, TemplateCompiler};
use agent_api::release::conflicts as conflict_engine;
use agent_model::compile::{
    self, CompileError, CompileInput, Compiled, DepartmentRef, FOLD_ORDER, FoldPosition,
    FoldSource, LayerMeta, Row, Selection, code, fold_knowledge, fold_order,
};
use agent_model::kinds::{Kind, Op};
use agent_model::layer::LayerKey;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::design_contract;
use crate::common::{insert_tenant, insert_user, json as req, person, send, super_admin};

const CS: &str = "industry_dept/retail/customer_service";
const CCA: &str = "job/customer_care_agent";

// ---- a small pure world ---------------------------------------------------------------------------------------------

const A: &str = "industry_dept/retail/kh_a";
const B: &str = "industry_dept/retail/kh_b";

fn layer(id: &str, key: &str, extends: Option<&str>) -> LayerMeta {
    LayerMeta {
        id: id.to_owned(),
        key: LayerKey::parse_label(key).unwrap_or_else(|e| panic!("{key}: {e}")),
        extends: extends.map(str::to_owned),
        tag: None,
        tagged_at: None,
    }
}

/// base, industry/retail, two departments (both extend the archetype `kh_arch` unless `split`: then B extends
/// `kh_arch_b`), job/care and its email channel layer.
fn layers(split: bool) -> Vec<LayerMeta> {
    vec![
        layer("L-base", "base", None),
        layer("L-ret", "industry/retail", None),
        layer("L-a", A, Some("kh_arch")),
        layer("L-b", B, Some(if split { "kh_arch_b" } else { "kh_arch" })),
        layer("L-job", "job/care", None),
        layer("L-email", "job/care/channel=email", None),
    ]
}

fn row(id: &str, layer_id: &str, kind: Kind, key: &str, op: Op, fields: Value) -> Row {
    Row {
        id: id.to_owned(),
        layer_id: layer_id.to_owned(),
        kind,
        key: key.to_owned(),
        op,
        fields,
        rev: 1,
    }
}

fn kind_row(id: &str, layer_id: &str, key: &str, fields: Value) -> Row {
    row(id, layer_id, Kind::StageKnowledgeKind, key, Op::Add, fields)
}

fn pack_row(id: &str, layer_id: &str, key: &str, version: i64) -> Row {
    row(
        id,
        layer_id,
        Kind::KnowledgePack,
        key,
        Op::Add,
        json!({ "version": version }),
    )
}

/// The base rows every pure world has: the job `care` and the archetypes.
fn base_rows() -> Vec<Row> {
    vec![
        row(
            "r-job",
            "L-base",
            Kind::Job,
            "care",
            Op::Add,
            json!({"label": "Care"}),
        ),
        row(
            "r-arch",
            "L-base",
            Kind::DepartmentArchetype,
            "kh_arch",
            Op::Add,
            json!({
                "label": "KH archetype",
                "knowledge_kinds": ["kh.shared", "policy.*"],
                "activities": ["Explain a bill", "Take a payment"],
            }),
        ),
        row(
            "r-arch-b",
            "L-base",
            Kind::DepartmentArchetype,
            "kh_arch_b",
            Op::Add,
            json!({"label": "KH archetype B", "knowledge_kinds": ["kh.only_b"]}),
        ),
    ]
}

fn two_departments() -> Selection {
    serde_json::from_value(json!({"industry": "retail", "departments": [A, B]})).unwrap()
}

fn one_department(dept: &str) -> Selection {
    serde_json::from_value(json!({"industry": "retail", "departments": [dept]})).unwrap()
}

fn compile_world(
    layers: &[LayerMeta],
    rows: &[Row],
    sel: &Selection,
    job: Option<&str>,
) -> Result<Compiled, Vec<CompileError>> {
    let departments = DepartmentRef::from_layers(layers);
    let input = CompileInput {
        layers,
        rows,
        rules: &[],
        profiles: &[],
        departments: &departments,
    };
    compile::compile(&input, sel, job)
}

fn codes(errors: &[CompileError]) -> Vec<&str> {
    errors.iter().map(|e| e.code.as_str()).collect()
}

fn kind_keys(k: &compile::CompiledKnowledge) -> Vec<String> {
    k.kinds.iter().map(compile::FoldedKind::key).collect()
}

// ---- the one fold order --------------------------------------------------------------------------------------------

#[test]
fn one_fold_order_archetype_before_each_department_and_shared_once() {
    assert!(
        FOLD_ORDER.windows(2).all(|w| w[0] < w[1]),
        "FOLD_ORDER is the enum order"
    );
    let ls = layers(false);
    let mut sel = two_departments();
    sel.dimensions.channel = vec!["email".into()];
    let steps = fold_order(&ls, &sel, Some("care"), &[]);
    let got: Vec<(FoldPosition, String, Option<usize>)> = steps
        .iter()
        .map(|s| (s.position, s.label(), s.department))
        .collect();
    use FoldPosition as P;
    assert_eq!(
        got,
        vec![
            (P::Base, "base".to_owned(), None),
            (P::Industry, "industry/retail".to_owned(), None),
            (
                P::Archetype,
                "department_archetype/kh_arch".to_owned(),
                Some(0)
            ),
            (P::Department, A.to_owned(), Some(0)),
            // the shared archetype folds once, at its first position
            (P::Department, B.to_owned(), Some(1)),
            (P::Job, "job/care".to_owned(), None),
            (P::Channel, "job/care/channel=email".to_owned(), None),
        ]
    );
    // the declared order decides the section order: B first puts the archetype before B
    let reversed: Selection =
        serde_json::from_value(json!({"industry": "retail", "departments": [B, A]})).unwrap();
    let labels: Vec<String> = fold_order(&ls, &reversed, None, &[])
        .iter()
        .map(|s| s.label())
        .collect();
    assert_eq!(
        labels,
        [
            "base",
            "industry/retail",
            "department_archetype/kh_arch",
            B,
            A
        ]
    );
    assert!(matches!(steps[2].source, FoldSource::Archetype("kh_arch")));
}

// ---- kinds at every layer ------------------------------------------------------------------------------------------

#[test]
fn base_and_archetype_kinds_reach_the_job_and_kinds_of_both_departments_union() {
    let ls = layers(true);
    let mut rows = base_rows();
    rows.extend([
        kind_row(
            "k-base",
            "L-base",
            "*|policy.privacy",
            json!({"label": "Privacy", "required": true, "tenant_data": false}),
        ),
        kind_row(
            "k-ret",
            "L-ret",
            "exit|policy.retail",
            json!({"label": "Retail exit"}),
        ),
        kind_row(
            "k-a",
            "L-a",
            "support|kh.only_a",
            json!({"label": "A only"}),
        ),
        kind_row(
            "k-b",
            "L-b",
            "delivery|kh.slots",
            json!({"label": "Slots", "tenant_data": true, "store_key": "orders"}),
        ),
        kind_row(
            "k-job",
            "L-job",
            "support|kh.job",
            json!({"label": "Job kind"}),
        ),
    ]);
    let (k, errors) = fold_knowledge(&ls, &rows, &two_departments(), Some("care"));
    assert!(errors.is_empty(), "{errors:#?}");
    assert_eq!(
        kind_keys(&k),
        [
            "*|kh.only_b",
            "*|kh.shared",
            "*|policy.privacy",
            "delivery|kh.slots",
            "exit|policy.retail",
            "support|kh.job",
            "support|kh.only_a",
        ]
    );
    let by = |key: &str| k.kinds.iter().find(|x| x.key() == key).unwrap();
    // the base-declared kind reaches the job, inherited by every department
    let base = by("*|policy.privacy");
    assert_eq!((base.layer.as_str(), base.required), ("base", true));
    assert!(base.departments.is_empty());
    // archetype kinds reach their department(s), from the archetype step
    assert_eq!(by("*|kh.shared").layer, "department_archetype/kh_arch");
    assert_eq!(by("*|kh.shared").departments, [A]);
    assert_eq!(by("*|kh.only_b").departments, [B]);
    // D110 / store key carried
    let slots = by("delivery|kh.slots");
    assert!(slots.tenant_data);
    assert_eq!(slots.store_key.as_deref(), Some("orders"));
    assert_eq!(slots.departments, [B]);
    // per department: its own plus the inherited kinds (T5/T8 read this)
    let of_a: Vec<String> = k.kinds_of_department(A).iter().map(|x| x.key()).collect();
    assert_eq!(
        of_a,
        [
            "*|kh.shared",
            "*|policy.privacy",
            "exit|policy.retail",
            "support|kh.job",
            "support|kh.only_a",
        ]
    );
    // the compile carries the same knowledge
    let c = compile_world(&ls, &rows, &two_departments(), Some("care"))
        .unwrap_or_else(|e| panic!("{e:#?}"));
    assert_eq!(c.knowledge, k);
}

#[test]
fn a_department_placing_an_archetype_kind_at_a_stage_replaces_the_stageless_one() {
    let ls = layers(false);
    let mut rows = base_rows();
    rows.push(kind_row(
        "k-a",
        "L-a",
        "orientation|kh.shared",
        json!({"label": "Shared"}),
    ));
    let (k, errors) = fold_knowledge(&ls, &rows, &one_department(A), None);
    assert!(errors.is_empty(), "{errors:#?}");
    assert_eq!(kind_keys(&k), ["orientation|kh.shared"]);
}

#[test]
fn required_cannot_be_unrequired() {
    let ls = layers(false);
    let mut rows = base_rows();
    rows.push(kind_row(
        "k-base",
        "L-base",
        "*|policy.privacy",
        json!({"label": "Privacy", "required": true}),
    ));
    rows.push(row(
        "k-ret",
        "L-ret",
        Kind::StageKnowledgeKind,
        "*|policy.privacy",
        Op::Patch,
        json!({"label": "Privacy", "required": false}),
    ));
    let errors = compile_world(&ls, &rows, &one_department(A), None).expect_err("a downgrade");
    assert_eq!(codes(&errors), [code::KIND_REQUIRED_DOWNGRADE]);
    assert_eq!(errors[0].key, "*|policy.privacy");
    // false → true is fine, and a patch without `required` keeps it
    rows.pop();
    rows.push(row(
        "k-ret",
        "L-ret",
        Kind::StageKnowledgeKind,
        "*|policy.privacy",
        Op::Patch,
        json!({"label": "Privacy (retail)"}),
    ));
    let c =
        compile_world(&ls, &rows, &one_department(A), None).unwrap_or_else(|e| panic!("{e:#?}"));
    let k = c
        .knowledge
        .kinds
        .iter()
        .find(|k| k.kind == "policy.privacy")
        .unwrap();
    assert!(k.required);
    assert_eq!(
        (k.label.as_str(), k.set_by.as_str()),
        ("Privacy (retail)", "industry/retail")
    );
}

// ---- packs ---------------------------------------------------------------------------------------------------------

#[test]
fn a_base_pack_for_other_kinds_is_not_applicable_not_an_error() {
    let ls = layers(false);
    let mut rows = base_rows();
    rows.extend([
        kind_row(
            "k-a",
            "L-a",
            "support|policy.returns",
            json!({"label": "Returns"}),
        ),
        // an IT-desk pack on base: its kind is no retail kind
        pack_row("p-it", "L-base", "support|kb.how_to|it/kb", 1),
        pack_row("p-ok", "L-ret", "support|*|retail/faq", 2),
    ]);
    let c = compile_world(&ls, &rows, &one_department(A), Some("care"))
        .unwrap_or_else(|e| panic!("not an error: {e:#?}"));
    let listed: Vec<String> = c.knowledge.not_applicable.iter().map(|p| p.key()).collect();
    assert_eq!(listed, ["support|kb.how_to|it/kb"]);
    assert_eq!(c.knowledge.packs.len(), 2);
    assert!(
        c.effective
            .warnings
            .iter()
            .any(|w| w.code == code::PACK_NOT_APPLICABLE && w.key == "support|kb.how_to|it/kb"),
        "{:?}",
        c.effective.warnings
    );
}

#[test]
fn one_pack_key_at_two_versions_from_two_departments_is_a_conflict() {
    let ls = layers(false);
    let mut rows = base_rows();
    rows.extend([
        kind_row("k-a", "L-a", "support|faq", json!({"label": "FAQ"})),
        kind_row("k-b", "L-b", "exit|faq", json!({"label": "FAQ"})),
        pack_row("p-a", "L-a", "support|faq|retail/faq", 1),
        pack_row("p-b", "L-b", "exit|faq|retail/faq", 2),
    ]);
    let errors = compile_world(&ls, &rows, &two_departments(), None).expect_err("two versions");
    assert_eq!(codes(&errors), [code::PACK_VERSION_CONFLICT]);
    assert!(
        errors[0].message.contains("retail/faq@1") && errors[0].message.contains("retail/faq@2")
    );
    assert!(
        errors[0].message.contains("tombstone"),
        "the message names the fix: {}",
        errors[0].message
    );
    // the fix the message names: a job-layer tombstone of one department's reference
    let mut fixed = rows.clone();
    fixed.push(row(
        "p-job",
        "L-job",
        Kind::KnowledgePack,
        "exit|faq|retail/faq",
        Op::Tombstone,
        json!({}),
    ));
    let c = compile_world(&ls, &fixed, &two_departments(), Some("care"))
        .unwrap_or_else(|e| panic!("{e:#?}"));
    let pins: Vec<String> = c.knowledge.packs.iter().map(|p| p.pin()).collect();
    assert_eq!(pins, ["retail/faq@1"]);
    // the same version from both: the union, no conflict
    rows.pop();
    rows.push(pack_row("p-b", "L-b", "exit|faq|retail/faq", 1));
    let c =
        compile_world(&ls, &rows, &two_departments(), None).unwrap_or_else(|e| panic!("{e:#?}"));
    assert_eq!(c.knowledge.packs.len(), 2);
}

// ---- sibling departments (D115) ------------------------------------------------------------------------------------

fn entity(id: &str, layer_id: &str, op: Op, label: &str) -> Row {
    let fields = if op == Op::Tombstone {
        json!({})
    } else {
        json!({ "label": label, "synonyms": ["kh"] })
    };
    row(id, layer_id, Kind::Entity, "KhThing", op, fields)
}

#[test]
fn two_departments_setting_one_key_differently_conflict_naming_both_layers() {
    let ls = layers(false);
    let mut rows = base_rows();
    rows.extend([
        entity("e-a", "L-a", Op::Add, "From A"),
        entity("e-b", "L-b", Op::Add, "From B"),
    ]);
    let errors =
        compile_world(&ls, &rows, &two_departments(), Some("care")).expect_err("siblings disagree");
    assert_eq!(codes(&errors), [code::DEPARTMENT_CONFLICT], "{errors:#?}");
    assert_eq!(errors[0].key, "entity:KhThing");
    assert!(
        errors[0].message.contains(A) && errors[0].message.contains(B),
        "{}",
        errors[0].message
    );
    // the layers as data, in the compile result's JSON too
    assert_eq!(errors[0].layers, [A, B]);
    let wire = serde_json::to_value(&errors[0]).unwrap();
    assert_eq!(wire["layers"], json!([A, B]));
    assert!(
        serde_json::to_value(CompileError {
            layers: Vec::new(),
            ..errors[0].clone()
        })
        .unwrap()
        .get("layers")
        .is_none(),
        "absent when empty"
    );
    let listed = compile::department_conflicts(
        &fold_order(&ls, &two_departments(), Some("care"), &[]),
        &rows,
    );
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].layers, [A, B]);
    assert!(!listed[0].resolved);

    // remove vs set is a conflict too
    let mut removing = base_rows();
    removing.extend([
        entity("e-base", "L-base", Op::Add, "Base"),
        entity("e-a", "L-a", Op::Tombstone, ""),
        entity("e-b", "L-b", Op::Patch, "From B"),
    ]);
    let errors =
        compile_world(&ls, &removing, &two_departments(), Some("care")).expect_err("remove vs set");
    assert_eq!(codes(&errors), [code::DEPARTMENT_CONFLICT]);

    // a job-layer row decides: no error, the job's value, no department's
    rows.push(entity("e-job", "L-job", Op::Add, "The job decides"));
    let c = compile_world(&ls, &rows, &two_departments(), Some("care"))
        .unwrap_or_else(|e| panic!("{e:#?}"));
    assert_eq!(c.effective.entities["KhThing"]["label"], "The job decides");
    let listed = compile::department_conflicts(
        &fold_order(&ls, &two_departments(), Some("care"), &[]),
        &rows,
    );
    assert!(listed[0].resolved);
    // a job PATCH decides too: over the departments' skipped adds it is the add
    rows.pop();
    rows.push(entity("e-job", "L-job", Op::Patch, "The job patched"));
    let c = compile_world(&ls, &rows, &two_departments(), Some("care"))
        .unwrap_or_else(|e| panic!("a job patch decides: {e:#?}"));
    assert_eq!(c.effective.entities["KhThing"]["label"], "The job patched");
    // and over a base item the departments both patched: the base item, patched by the job
    let mut over_base = base_rows();
    over_base.extend([
        entity("e-base", "L-base", Op::Add, "Base"),
        entity("e-a", "L-a", Op::Patch, "A"),
        entity("e-b", "L-b", Op::Patch, "B"),
        entity("e-job", "L-job", Op::Patch, "Job"),
    ]);
    let c = compile_world(&ls, &over_base, &two_departments(), Some("care"))
        .unwrap_or_else(|e| panic!("{e:#?}"));
    assert_eq!(c.effective.entities["KhThing"]["label"], "Job");
    // the error says how a job decides
    let errors =
        compile_world(&ls, &removing, &two_departments(), Some("care")).expect_err("remove vs set");
    assert!(
        errors[0].message.contains("an add") && errors[0].message.contains("a patch"),
        "{}",
        errors[0].message
    );
}

#[test]
fn equal_values_from_two_departments_pass() {
    let ls = layers(false);
    let mut rows = base_rows();
    rows.extend([
        entity("e-a", "L-a", Op::Add, "Same"),
        entity("e-b", "L-b", Op::Add, "Same"),
    ]);
    let c = compile_world(&ls, &rows, &two_departments(), Some("care"))
        .unwrap_or_else(|e| panic!("equal values are no conflict (nor a duplicate add): {e:#?}"));
    assert_eq!(c.effective.entities["KhThing"]["label"], "Same");
    // a knowledge kind both departments declare alike is one kind of both
    let mut kinds_rows = base_rows();
    kinds_rows.extend([
        kind_row("k-a", "L-a", "support|faq", json!({"label": "FAQ"})),
        kind_row("k-b", "L-b", "support|faq", json!({"label": "FAQ"})),
    ]);
    let (k, errors) = fold_knowledge(&ls, &kinds_rows, &two_departments(), None);
    assert!(errors.is_empty(), "{errors:#?}");
    assert_eq!(kind_keys(&k), ["*|kh.shared", "support|faq"]);
    // and differently: a conflict on the kind key, resolved by a job row
    kinds_rows.pop();
    kinds_rows.push(kind_row(
        "k-b",
        "L-b",
        "support|faq",
        json!({"label": "Other FAQ"}),
    ));
    let (_, errors) = fold_knowledge(&ls, &kinds_rows, &two_departments(), Some("care"));
    assert_eq!(codes(&errors), [code::DEPARTMENT_CONFLICT]);
    assert_eq!(errors[0].key, "stage_knowledge_kind:support|faq");
    kinds_rows.push(kind_row(
        "k-job",
        "L-job",
        "support|faq",
        json!({"label": "Job FAQ"}),
    ));
    let (k, errors) = fold_knowledge(&ls, &kinds_rows, &two_departments(), Some("care"));
    assert!(errors.is_empty(), "{errors:#?}");
    let faq = k.kinds.iter().find(|x| x.key() == "support|faq").unwrap();
    assert_eq!(
        (faq.label.as_str(), faq.layer.as_str()),
        ("Job FAQ", "job/care")
    );
}

fn instruction(id: &str, layer_id: &str, slot: &str, text: &str) -> Row {
    row(
        id,
        layer_id,
        Kind::Instruction,
        slot,
        Op::Add,
        json!({ "text": text }),
    )
}

fn slot_text<'c>(c: &'c Compiled, slot: &str) -> Option<&'c str> {
    c.catalogue
        .instructions
        .iter()
        .find(|i| i.key == slot)
        .map(|i| i.text.as_str())
}

/// KH T3 x T4 (D108 + D115): instruction slots fold in the one fold order under the sibling rule — two departments
/// with different `identity` / `policy` text conflict (each slot its own `department_conflict`, naming both
/// layers); equal text passes once; a job-layer instruction of the slot decides and the departments' rows of it do
/// not fold.
#[test]
fn sibling_department_instructions_conflict_and_a_job_instruction_decides() {
    let ls = layers(false);
    let mut rows = base_rows();
    rows.extend([
        instruction("i-a-id", "L-a", "identity", "You are A's agent."),
        instruction("i-b-id", "L-b", "identity", "You are B's agent."),
        instruction("i-a-pol", "L-a", "policy", "Refund within 14 days."),
        instruction("i-b-pol", "L-b", "policy", "Refund within 30 days."),
        instruction("i-a-tone", "L-a", "tone", "Warm."),
        instruction("i-b-tone", "L-b", "tone", "Warm."),
    ]);
    let errors = compile_world(&ls, &rows, &two_departments(), Some("care"))
        .expect_err("sibling instructions disagree");
    assert_eq!(
        codes(&errors),
        [code::DEPARTMENT_CONFLICT, code::DEPARTMENT_CONFLICT],
        "{errors:#?}"
    );
    let mut keys: Vec<&str> = errors.iter().map(|e| e.key.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["instruction:identity", "instruction:policy"]);
    assert!(errors.iter().all(|e| e.layers == [A, B]), "{errors:#?}");

    // a job-layer instruction decides each slot: no error, the job's text only, never a department's
    rows.extend([
        instruction("i-job-id", "L-job", "identity", "You are the care agent."),
        instruction("i-job-pol", "L-job", "policy", "Refund within 21 days."),
    ]);
    let c = compile_world(&ls, &rows, &two_departments(), Some("care"))
        .unwrap_or_else(|e| panic!("a job instruction decides: {e:#?}"));
    assert_eq!(slot_text(&c, "identity"), Some("You are the care agent."));
    assert_eq!(slot_text(&c, "policy"), Some("Refund within 21 days."));
    // equal sibling text folds once (the later sibling's row is skipped, not appended or re-tagged)
    assert_eq!(slot_text(&c, "tone"), Some("Warm."));
    let tone = c
        .catalogue
        .instructions
        .iter()
        .find(|i| i.key == "tone")
        .unwrap();
    assert_eq!(tone.sections.len(), 1, "{tone:#?}");
    for slot in ["identity", "policy"] {
        let folded = c
            .catalogue
            .instructions
            .iter()
            .find(|i| i.key == slot)
            .unwrap();
        assert!(
            folded.sections.iter().all(|s| s.layer != A && s.layer != B),
            "{slot}: no department section survives a job decision: {folded:#?}"
        );
    }
    let listed = compile::department_conflicts(
        &fold_order(&ls, &two_departments(), Some("care"), &[]),
        &rows,
    );
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|c| c.resolved));
}

// ---- upgrade rules (D112) ------------------------------------------------------------------------------------------

fn pref(key: &str, version: i32, replaces: Option<&str>) -> PackRef {
    let (stage, kind, pack_key) = agent_model::kinds::knowledge_pack::split_key(key).unwrap();
    PackRef {
        stage: stage.into(),
        kind: kind.into(),
        pack_key: pack_key.into(),
        version,
        layer: "industry/retail".into(),
        row_id: Uuid::nil(),
        replaces: replaces.map(str::to_owned),
    }
}

#[test]
fn a_renamed_pack_keeps_the_tombstone_and_a_wildcard_tombstone_excludes_later_packs() {
    let tombs = [
        "support|faq|retail/old".to_owned(),
        "*|*|retail/later".to_owned(),
    ];
    let after = [
        pref("support|faq|retail/new", 1, Some("retail/old")),
        pref("orientation|faq|retail/later", 3, None),
    ];
    // the renamed pack stays excluded where the old one was
    assert!(packs::pins_for(&after, "support", "faq", &tombs).is_empty());
    // the wildcard tombstone excludes the pack added later
    assert!(packs::pins_for(&after, "orientation", "faq", &tombs).is_empty());
    // without `replaces` the rename is a new pack the old tombstone does not reach
    let plain = [pref("support|faq|retail/new", 1, None)];
    assert_eq!(
        packs::pins_for(&plain, "support", "faq", &tombs),
        ["retail/new@1"]
    );
}

#[test]
fn a_base_pack_can_target_a_base_stageless_kind() {
    let ls = layers(false);
    let mut rows = base_rows();
    rows.extend([
        kind_row(
            "k-base",
            "L-base",
            "*|policy.privacy",
            json!({"label": "Privacy"}),
        ),
        pack_row("p-base", "L-base", "*|policy.privacy|base/privacy", 1),
    ]);
    let c = compile_world(&ls, &rows, &one_department(A), Some("care"))
        .unwrap_or_else(|e| panic!("`*|<kind>` is a pack key now: {e:#?}"));
    assert_eq!(c.knowledge.packs.len(), 1);
    assert!(c.knowledge.not_applicable.is_empty(), "it reaches the kind");
    let refs = packs::refs_of(&c.knowledge);
    assert_eq!(
        packs::pins_for(&refs, "exit", "policy.privacy", &[]),
        ["base/privacy@1"]
    );
    assert!(packs::pins_for(&refs, "exit", "other", &[]).is_empty());
}

// ---- archetypes per department -------------------------------------------------------------------------------------

#[test]
fn archetype_kinds_and_activities_give_way_per_department() {
    // A and B share the archetype; only B places `kh.shared` at a stage and redeclares `Take a payment`
    let ls = layers(false);
    let mut rows = base_rows();
    rows.extend([
        kind_row(
            "k-b",
            "L-b",
            "support|kh.shared",
            json!({"label": "Shared"}),
        ),
        row(
            "a-b",
            "L-b",
            Kind::Activity,
            "purchase:take_payment",
            Op::Add,
            json!({"label": "Take a payment"}),
        ),
    ]);
    let (k, errors) = fold_knowledge(&ls, &rows, &two_departments(), None);
    assert!(errors.is_empty(), "{errors:#?}");
    let keys_of = |dept: &str| -> Vec<String> {
        k.kinds_of_department(dept)
            .iter()
            .map(|x| x.key())
            .collect()
    };
    assert_eq!(keys_of(A), ["*|kh.shared"], "A keeps the archetype's kind");
    assert_eq!(keys_of(B), ["support|kh.shared"], "B placed it at a stage");
    // the archetype's activities reach the departments, as stage `*`, before the department
    let acts_of = |dept: &str| -> Vec<(String, String)> {
        k.activities
            .iter()
            .filter(|a| a.of_department(dept))
            .map(|a| (a.key.clone(), a.layer.clone()))
            .collect()
    };
    let arch = "department_archetype/kh_arch".to_owned();
    assert_eq!(
        acts_of(A),
        [
            ("*:explain_a_bill".to_owned(), arch.clone()),
            ("*:take_a_payment".to_owned(), arch.clone()),
        ]
    );
    // B's own `Take a payment` wins over the archetype's
    assert_eq!(
        acts_of(B),
        [
            ("*:explain_a_bill".to_owned(), arch.clone()),
            ("purchase:take_payment".to_owned(), B.to_owned()),
        ]
    );
    // the compile carries them
    let c = compile_world(&ls, &rows, &two_departments(), Some("care"))
        .unwrap_or_else(|e| panic!("{e:#?}"));
    assert_eq!(c.knowledge.activities, k.activities);
}

// ---- the database ---------------------------------------------------------------------------------------------------

fn actor() -> Actor {
    Actor::system("service:agent-api-kh-t3-test")
}

/// One revision, committed; the write's refusal is returned.
async fn write(
    db: &PgPool,
    layer: &str,
    kind: Kind,
    key: &str,
    op: Op,
    fields: Value,
) -> Result<(), ApiError> {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    let r = rows::write_revision(
        &mut tx,
        &actor(),
        layer,
        kind,
        key,
        op,
        fields,
        "kh-t3-author",
    )
    .await;
    if r.is_ok() {
        tx.commit().await.expect("commit the revision");
    }
    r.map(|_| ())
}

async fn must_write(db: &PgPool, layer: &str, kind: Kind, key: &str, fields: Value) {
    write(db, layer, kind, key, Op::Add, fields)
        .await
        .unwrap_or_else(|e| panic!("write {layer} {kind} {key}: {e:?}"));
}

async fn tag(db: &PgPool, family: &str) -> i32 {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    let t = tags::tag_family(
        &mut tx,
        &actor(),
        family,
        "kh t3",
        "kh-t3-owner",
        &NoHooks,
        false,
    )
    .await
    .unwrap_or_else(|e| panic!("tag {family}: {e:?}"));
    tx.commit().await.expect("commit the tag");
    t.version
}

async fn pins_of(db: &PgPool, layer: &str, version: i32) -> Vec<TemplatePin> {
    let mut c = db.acquire().await.unwrap();
    templates::resolve_template(&mut c, layer, version)
        .await
        .unwrap_or_else(|e| panic!("{layer}@v{version}: {e:?}"))
        .pins
}

fn retail(departments: &[&str]) -> Selection {
    serde_json::from_value(json!({
        "profile": "retail_ecommerce",
        "industry": "retail",
        "departments": departments,
    }))
    .unwrap()
}

/// The retail customer service department's inheritance, authored through the platform's own write path (the
/// platform holds no archetype or kind of its own): the base `customer_service` archetype (family `policy.*`) which
/// `CS` extends, and the two kinds `CS` declares itself (`support|policy.returns`, `support|policy.warranty`). Base
/// and `CS` are tagged, so the job layer's next tag pins them. Idempotent per database.
async fn customer_service_kinds(e: &crate::common::TestEnv) {
    let db = &e.state.db;
    let done: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM layer WHERE key = $1 AND extends = 'customer_service')",
    )
    .bind(CS)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    if done {
        return;
    }
    must_write(
        db,
        "base",
        Kind::DepartmentArchetype,
        "customer_service",
        json!({"label": "Customer service", "knowledge_kinds": ["policy.*"]}),
    )
    .await;
    tag(db, "base").await;
    sqlx::query("UPDATE layer SET extends = 'customer_service' WHERE key = $1")
        .bind(CS)
        .execute(&e.owner)
        .await
        .unwrap();
    for (key, label) in [
        ("support|policy.returns", "Returns"),
        ("support|policy.warranty", "Warranty"),
    ] {
        must_write(
            db,
            CS,
            Kind::StageKnowledgeKind,
            key,
            json!({"label": label}),
        )
        .await;
    }
    tag(db, CS).await;
}

#[tokio::test]
async fn a_pack_row_naming_an_undeclared_kind_is_422() {
    let e = crate::common::env().await;
    customer_service_kinds(&e).await;
    let db = &e.state.db;
    let refused = write(
        db,
        "base",
        Kind::KnowledgePack,
        "support|kh.nothing|base",
        Op::Add,
        json!({"version": 1}),
    )
    .await
    .expect_err("undeclared");
    assert_eq!(
        refused.status_and_code(),
        (StatusCode::UNPROCESSABLE_ENTITY, "pack_kind_undeclared")
    );
    // `*` kinds and tombstones are never checked
    must_write(
        db,
        "base",
        Kind::KnowledgePack,
        "*|*|base",
        json!({"version": 1}),
    )
    .await;
    write(
        db,
        "base",
        Kind::KnowledgePack,
        "support|kh.nothing|base",
        Op::Tombstone,
        json!({}),
    )
    .await
    .expect("a tombstone is never checked");
    // declared on an ancestor (base) → a department may reference it
    must_write(
        db,
        "base",
        Kind::StageKnowledgeKind,
        "*|kh.base_kind",
        json!({"label": "Base kind"}),
    )
    .await;
    must_write(
        db,
        CS,
        Kind::KnowledgePack,
        "support|kh.base_kind|retail/kb",
        json!({"version": 1}),
    )
    .await;
    // a base pack for the base `*|kind` (one kind at every stage) is a pack key now
    must_write(
        db,
        "base",
        Kind::KnowledgePack,
        "*|kh.base_kind|base/kb",
        json!({"version": 1}),
    )
    .await;
    // covered by the department's archetype family (`customer_service` declares `policy.*`)
    must_write(
        db,
        CS,
        Kind::KnowledgePack,
        "support|policy.kh_new|retail/kb",
        json!({"version": 1}),
    )
    .await;
    // the job layer draws on its template's departments
    must_write(
        db,
        CCA,
        Kind::KnowledgePack,
        "support|policy.returns|retail/kb",
        json!({"version": 1}),
    )
    .await;
    // a kind declared only in a job layer does not reach the department
    must_write(
        db,
        CCA,
        Kind::StageKnowledgeKind,
        "support|kh.job_only",
        json!({"label": "Job only"}),
    )
    .await;
    let refused = write(
        db,
        CS,
        Kind::KnowledgePack,
        "support|kh.job_only|retail/kb",
        Op::Add,
        json!({"version": 1}),
    )
    .await
    .expect_err("not an ancestor");
    assert_eq!(refused.status_and_code().1, "pack_kind_undeclared");
}

#[tokio::test]
async fn template_seeding_reads_the_folded_kind_set() {
    let e = crate::common::env().await;
    customer_service_kinds(&e).await;
    let db = &e.state.db;
    // a kind declared on base, with the new fields
    must_write(
        db,
        "base",
        Kind::StageKnowledgeKind,
        "*|policy.kh_base",
        json!({"label": "KH base", "required": true, "tenant_data": true}),
    )
    .await;
    must_write(
        db,
        CCA,
        Kind::StageKnowledgeKind,
        "support|kh.job_kind",
        json!({"label": "Job kind"}),
    )
    .await;
    tag(db, "base").await;
    let v = tag(db, CCA).await;
    let pins = pins_of(db, CCA, v).await;
    let mut c = db.acquire().await.unwrap();
    let seeds = kinds::template_seeds(&mut c, &pins, Some(&retail(&[CS])), None)
        .await
        .unwrap();
    drop(c);
    let find = |s: &str, k: &str| seeds.iter().find(|x| x.stage == s && x.kind == k);
    let base = find("*", "policy.kh_base").unwrap_or_else(|| panic!("{seeds:#?}"));
    assert!(base.required && base.tenant_data, "{base:?}");
    assert!(
        find("support", "kh.job_kind").is_some(),
        "the job layer's kind"
    );
    assert!(
        find("support", "policy.returns").is_some(),
        "the department's kind"
    );
    // archetype kinds the department placed at a stage are not repeated stage-less
    assert!(find("*", "faq.general").is_none(), "{seeds:#?}");

    // a job created from the template materialises them (0135 `tenant_data`)
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, &format!("KH {t}")).await;
    let mut r = req(
        "POST",
        "/api/jobs",
        Some(&super_admin()),
        json!({"template": format!("{}@v{}", CCA, v)}),
    );
    r.headers_mut()
        .insert("x-active-tenant-id", t.to_string().parse().unwrap());
    let (s, b) = send(&e.app, r).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let row: (bool, bool) = sqlx::query_as(
        "SELECT k.required, k.tenant_data FROM job_knowledge_kind k JOIN jobs j ON j.id = k.job_id \
          WHERE j.tenant_id = $1 AND k.stage = '*' AND k.kind = 'policy.kh_base'",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .expect("the base kind is a job kind");
    assert_eq!(row, (true, true));
}

/// A second retail department extending the same archetype, tagged with `rows`.
async fn second_department(e: &crate::common::TestEnv, key: &str) {
    let dept = key.rsplit('/').next().unwrap();
    sqlx::query(
        "INSERT INTO layer (key, scope, industry, department, family, label, extends, areas) \
         VALUES ($1, 'industry_dept', 'retail', $2, $1, $3, 'customer_service', '{}')",
    )
    .bind(key)
    .bind(dept)
    .bind(format!("KH {dept}"))
    .execute(&e.owner)
    .await
    .unwrap_or_else(|err| panic!("insert {key}: {err}"));
}

#[tokio::test]
async fn two_pinned_departments_conflict_the_job_decides_and_two_pack_versions_fail_the_cut() {
    let e = crate::common::env().await;
    customer_service_kinds(&e).await;
    let db = &e.state.db;
    let returns = "industry_dept/retail/kh_t3_returns";
    second_department(&e, returns).await;
    // the second department sets the customer-service kind `support|policy.returns` differently
    must_write(
        db,
        returns,
        Kind::StageKnowledgeKind,
        "support|policy.returns",
        json!({"label": "Returns (returns desk)", "required": true}),
    )
    .await;
    must_write(
        db,
        returns,
        Kind::StageKnowledgeKind,
        "exit|kh.return_label",
        json!({"label": "Return label"}),
    )
    .await;
    tag(db, returns).await;
    must_write(
        db,
        CCA,
        Kind::StageKnowledgeKind,
        "support|kh.job_kind",
        json!({"label": "Job kind"}),
    )
    .await;
    let v = tag(db, CCA).await;
    let pins = pins_of(db, CCA, v).await;
    assert!(pins.iter().any(|p| p.layer == returns), "{pins:?}");
    let sel = retail(&[CS, returns]);
    let job = Some("customer_care_agent");

    let mut c = db.acquire().await.unwrap();
    let errors = templates::compile_pinned(&mut c, &pins, &sel, job)
        .await
        .unwrap()
        .expect_err("the departments disagree");
    drop(c);
    let conflict = errors
        .iter()
        .find(|x| x.code == "department_conflict")
        .unwrap_or_else(|| panic!("{errors:#?}"));
    assert_eq!(conflict.key, "stage_knowledge_kind:support|policy.returns");
    assert!(
        conflict.message.contains(CS) && conflict.message.contains(returns),
        "{}",
        conflict.message
    );

    // a job-layer row decides
    must_write(
        db,
        CCA,
        Kind::StageKnowledgeKind,
        "support|policy.returns",
        json!({"label": "Returns (the job's)", "required": true}),
    )
    .await;
    let v = tag(db, CCA).await;
    let pins = pins_of(db, CCA, v).await;
    let mut c = db.acquire().await.unwrap();
    let compiled = templates::compile_pinned(&mut c, &pins, &sel, job)
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("{e:#?}"));
    drop(c);
    let k = &compiled.knowledge;
    let returns_kind = k
        .kinds
        .iter()
        .find(|x| x.key() == "support|policy.returns")
        .unwrap();
    assert_eq!(returns_kind.label, "Returns (the job's)");
    assert_eq!(returns_kind.set_by, CCA);
    // the kinds of both departments reach the job
    let of = |dept: &str, key: &str| k.kinds_of_department(dept).iter().any(|x| x.key() == key);
    assert!(of(returns, "exit|kh.return_label") && !of(CS, "exit|kh.return_label"));
    assert!(of(CS, "support|policy.warranty") && !of(returns, "support|policy.warranty"));
    assert!(of(CS, "support|kh.job_kind") && of(returns, "support|kh.job_kind"));

    // the same pack key at two versions, one per department: the cut's compile fails 422
    must_write(
        db,
        CS,
        Kind::KnowledgePack,
        "support|policy.returns|retail/kh_pack",
        json!({"version": 1}),
    )
    .await;
    must_write(
        db,
        returns,
        Kind::KnowledgePack,
        "exit|kh.return_label|retail/kh_pack",
        json!({"version": 2}),
    )
    .await;
    tag(db, CS).await;
    tag(db, returns).await;
    must_write(
        db,
        CCA,
        Kind::StageKnowledgeKind,
        "support|kh.job_kind_2",
        json!({"label": "Again"}),
    )
    .await;
    let v = tag(db, CCA).await;
    let pins = pins_of(db, CCA, v).await;
    let mut c = db.acquire().await.unwrap();
    let errors = templates::compile_pinned(&mut c, &pins, &sel, job)
        .await
        .unwrap()
        .expect_err("two versions of one pack");
    assert_eq!(codes(&errors), [code::PACK_VERSION_CONFLICT], "{errors:#?}");
    let compiler = LayerCompiler::new();
    let refused = ApiError::from(
        compiler
            .compile(&mut c, &sel, &pins)
            .await
            .expect_err("the cut refuses"),
    );
    assert_eq!(
        refused.status_and_code(),
        (StatusCode::UNPROCESSABLE_ENTITY, "compile_failed")
    );
}

// ---- the upgrade surface (D112) -------------------------------------------------------------------------------------

const ADMIN: &str = "u-kh-t3-admin";

/// A `knowledge_pack` base item of a bundle (`release::compile::knowledge_pack_items`' shape).
fn pack_item(key: &str, replaces: Option<&str>) -> Value {
    let (stage, kind, pack) = agent_model::kinds::knowledge_pack::split_key(key).unwrap();
    let mut v = json!({"key": key, "label": format!("{pack}@v1 → {stage}/{kind}"), "pin": format!("{pack}@1")});
    if let Some(old) = replaces {
        v["replaces"] = json!(old);
    }
    v
}

fn with_packs(bundle: &Value, items: &[Value]) -> Value {
    let mut b = bundle.clone();
    b["objects"]["knowledge_pack"] = json!(items);
    b
}

#[tokio::test]
async fn an_upgrade_lists_packs_a_wildcard_tombstone_excludes_and_does_not_block() {
    let e = crate::common::env().await;
    let f = design_contract::new_job(&e).await;
    insert_user(&e.owner, ADMIN, f.tenant, "tenant_admin").await;
    let tok = person(ADMIN, &[]);
    let path = |tail: &str| format!("/api/jobs/{}/{tail}", f.job.job_key);
    let sel = json!({
        "profile": "retail_ecommerce", "industry": "retail", "departments": [CS],
        "areas": ["order-support"], "dimensions": {"channel": ["chat"]}, "region": "eu",
    });
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &path("target"),
            Some(&tok),
            json!({"selection": sel, "expected_updated_at": null}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let at = b["selection_updated_at"].clone();
    let (s, b) = send(
        &e.app,
        req(
            "POST",
            &path("releases"),
            Some(&tok),
            json!({"apply": false, "selection_seen_at": at}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id = b["release"]["id"].as_str().unwrap().to_owned();

    // the job's pack rules: two wildcards and one exact tombstone
    for (key, base_hash) in [
        ("*|*|retail/kh_later", None),
        ("*|*|retail/kh_renamed", None),
        ("support|policy.returns|retail/kh_old", Some("sha256:x")),
    ] {
        sqlx::query(
            "INSERT INTO job_overlay_row (tenant_id, job_id, object, key, kind, base_hash, updated_by) \
             VALUES ($1, $2, 'knowledge_pack', $3, 'tombstone', $4, 'test')",
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .bind(key)
        .bind(base_hash)
        .execute(&e.owner)
        .await
        .unwrap_or_else(|err| panic!("{key}: {err}"));
    }
    let bundle: Value =
        sqlx::query_scalar("SELECT bundle FROM job_release WHERE job_id = $1 AND release_id = $2")
            .bind(f.job.job_id)
            .bind(&id)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    let from = with_packs(
        &bundle,
        &[
            pack_item("support|policy.returns|retail/kh_old", None),
            pack_item("exit|policy.privacy|retail/kh_renamed", None),
        ],
    );
    // the upgrade: a pack under the first wildcard, kh_renamed renamed with `replaces`, kh_old gone
    let to = with_packs(
        &bundle,
        &[
            pack_item("support|policy.returns|retail/kh_later", None),
            pack_item(
                "exit|policy.privacy|retail/kh_new",
                Some("exit|policy.privacy|retail/kh_renamed"),
            ),
        ],
    );
    let pin = |v: i32| TemplatePin {
        layer: "industry/retail".to_owned(),
        version: v,
    };
    let jr = f.job_ref();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let n = conflict_engine::raise_upgrade(
        &mut tx,
        &e.state.conflict_detectors,
        f.tenant,
        &jr,
        &id,
        &[pin(1)],
        &[pin(2)],
        Some(&from),
        &to,
    )
    .await
    .expect("the upgrade conflicts are stored");
    tx.commit().await.unwrap();
    let raised: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT kind, object, key FROM release_conflict WHERE job_id = $1 AND status = 'open' ORDER BY id",
    )
    .bind(f.job.job_id)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    let want = |k: &str, key: &str| (k.to_owned(), "knowledge_pack".to_owned(), key.to_owned());
    assert_eq!(
        raised,
        vec![
            // listed, informational: the wildcard also excludes the new pack
            want("excluded_by_rule", "support|policy.returns|retail/kh_later"),
            // the exact tombstone's pack is gone with no successor: the approver decides
            want("base_removed", "support|policy.returns|retail/kh_old"),
        ],
        "the renamed pack's wildcard follows it (no orphan)"
    );
    assert_eq!(n, 2);

    // nothing blocks: the release applies with the finding open
    let (s, b) = send(
        &e.app,
        req(
            "POST",
            &path(&format!("releases/{id}/apply")),
            Some(&tok),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["applied"], true, "{b}");
}
