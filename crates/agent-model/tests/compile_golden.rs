//! B6 T2: the compile rules over in-test worlds (all errors at once, region, areas, channel layers, tombstone
//! cascade, catalogue fold, the hash). The goldens over the committed platform seed were removed with the seed.

use std::collections::BTreeMap;

use agent_model::canonical::{canonical_json, sha256_hex};
use agent_model::compile::{
    AxisRuleDef, CatalogueItem, CompileError, CompileInput, Compiled, DepartmentRef, LayerMeta,
    ProfileDef, Row, Selection, SelectionDimensions, compile, compile_preview, effective_hash,
    refusal,
};
use agent_model::kinds::{Kind, Op};
use agent_model::layer::LayerKey;
use serde_json::{Map, Value, json};

// --- synthetic worlds -----------------------------------------------------------------------------------------------

fn layer(id: &str, label: &str) -> LayerMeta {
    LayerMeta {
        id: id.to_owned(),
        key: LayerKey::parse_label(label).unwrap_or_else(|e| panic!("{label}: {e}")),
        extends: None,
        tag: None,
        tagged_at: None,
    }
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

fn run(
    layers: &[LayerMeta],
    rows: &[Row],
    selection: &Selection,
    job: Option<&str>,
) -> Result<Compiled, Vec<CompileError>> {
    let rules = AxisRuleDef::from_rows(rows)?;
    let profiles = ProfileDef::from_rows(rows)?;
    let departments = DepartmentRef::from_layers(layers);
    let input = CompileInput {
        layers,
        rows,
        rules: &rules,
        profiles: &profiles,
        departments: &departments,
    };
    // the small worlds' helper compiles as the platform preview; the hierarchy tests check `compile` (Mode::Job)
    compile_preview(&input, selection, job)
}

fn ok(r: Result<Compiled, Vec<CompileError>>) -> Compiled {
    r.unwrap_or_else(|e| panic!("unexpected compile errors: {e:#?}"))
}

fn codes(errors: &[CompileError]) -> Vec<&str> {
    errors.iter().map(|e| e.code.as_str()).collect()
}

/// The one department the small test worlds define (D106: every compiled selection names one).
const CS: &str = "industry_dept/retail/customer_service";

fn retail() -> Selection {
    Selection {
        industry: "retail".to_owned(),
        departments: vec![CS.to_owned()],
        ..Selection::default()
    }
}

fn names(items: &[CatalogueItem]) -> Vec<&str> {
    items.iter().map(|i| i.key.as_str()).collect()
}

fn keys(items: &BTreeMap<String, Map<String, Value>>) -> Vec<&str> {
    items.keys().map(String::as_str).collect()
}

#[test]
fn errors_are_all_reported() {
    let layers = [
        layer("L0", "base"),
        layer("L1", "industry/retail"),
        layer("L9", CS),
    ];
    let rows = [
        row(
            "r1",
            "L0",
            Kind::Entity,
            "Order",
            Op::Add,
            json!({"label": "Order"}),
        ),
        row(
            "r2",
            "L0",
            Kind::Attribute,
            "Order.order_id",
            Op::Add,
            json!({"data_type": "string"}),
        ),
        // broken 1: patches an entity no layer adds
        row(
            "r3",
            "L1",
            Kind::Entity,
            "Refund",
            Op::Patch,
            json!({"label": "Refund"}),
        ),
        // broken 2: an attribute of an entity the selection does not have
        row(
            "r4",
            "L1",
            Kind::Attribute,
            "Ghost.ghost_id",
            Op::Add,
            json!({"data_type": "string"}),
        ),
    ];
    let errors = run(&layers, &rows, &retail(), None).expect_err("two broken rows");
    assert_eq!(
        codes(&errors),
        vec!["patch_target_missing", "parent_missing"]
    );
    let ids: Vec<Option<&str>> = errors.iter().map(|e| e.row_id.as_deref()).collect();
    assert_eq!(ids, vec![Some("r3"), Some("r4")]);
    assert_eq!(
        errors[0].message,
        "industry/retail patches entity Refund, which no earlier layer adds."
    );
    assert_eq!(errors[1].key, "Ghost.ghost_id");
}

#[test]
fn configurator_row_errors_are_kept() {
    let layers = [
        layer("L0", "base"),
        layer("L1", "industry/retail"),
        layer("L2", "job/x"),
        layer("L9", CS),
    ];
    let rows = [
        row(
            "r1",
            "L0",
            Kind::Entity,
            "Order",
            Op::Add,
            json!({"label": "Order"}),
        ),
        row(
            "r2",
            "L0",
            Kind::Entity,
            "Payment",
            Op::Add,
            json!({"label": "Payment"}),
        ),
        row("r3", "L0", Kind::Job, "x", Op::Add, json!({"label": "X"})),
        row(
            "r4",
            "L1",
            Kind::Entity,
            "Order",
            Op::Add,
            json!({"label": "Again"}),
        ),
        row(
            "r5",
            "L1",
            Kind::Procedure,
            "p@1",
            Op::Add,
            json!({"label": "P"}),
        ),
        row(
            "r6",
            "L1",
            Kind::Entity,
            "bad",
            Op::Add,
            json!({"label": "Bad"}),
        ),
        row(
            "r7",
            "L1",
            Kind::Entity,
            "Payment",
            Op::Tombstone,
            json!({}),
        ),
        row(
            "r8",
            "L2",
            Kind::Entity,
            "Payment",
            Op::Add,
            json!({"label": "Payment"}),
        ),
    ];
    let errors = run(&layers, &rows, &retail(), Some("x")).expect_err("four broken rows");
    assert_eq!(
        codes(&errors),
        vec![
            "duplicate_add",
            "kind_not_allowed_in_layer",
            "invalid_row",
            "add_tombstoned"
        ]
    );
    let ids: Vec<Option<&str>> = errors.iter().map(|e| e.row_id.as_deref()).collect();
    assert_eq!(ids, vec![Some("r4"), Some("r5"), Some("r6"), Some("r8")]);
    assert!(refusal(&errors).is_none());
}

#[test]
fn unknown_profile_is_refused_alone() {
    let layers = [
        layer("L0", "base"),
        layer("L1", "industry/retail"),
        layer("L9", CS),
    ];
    let rows = [row(
        "r1",
        "L1",
        Kind::Entity,
        "bad",
        Op::Add,
        json!({"label": "would be invalid_row"}),
    )];
    let selection = Selection {
        profile: Some("no_such_profile".to_owned()),
        departments: vec!["industry_dept/retail/nowhere".to_owned()],
        ..retail()
    };
    let errors = run(&layers, &rows, &selection, None).expect_err("refused");
    assert_eq!(codes(&errors), vec!["profile_not_found"]);
    let r = refusal(&errors).expect("a 404 refusal");
    assert_eq!((r.status, r.code), (404, "profile_not_found"));
}

fn channel_world() -> (Vec<LayerMeta>, Vec<Row>) {
    let layers = vec![
        layer("L0", "base"),
        layer("L1", "job/x"),
        layer("L2", "job/x/channel=email"),
        layer("L3", "job/x/channel=voice"),
        layer("L4", "job/x/channel=chat"),
        layer("L5", "job/x/region=nl"),
        layer("L6", "industry/retail"),
        layer("L7", CS),
    ];
    let rows = vec![
        row("r1", "L0", Kind::Job, "x", Op::Add, json!({"label": "X"})),
        row(
            "r2",
            "L2",
            Kind::Job,
            "x",
            Op::Patch,
            json!({"stance": "Email"}),
        ),
        row(
            "r3",
            "L3",
            Kind::Job,
            "x",
            Op::Patch,
            json!({"preamble": "Voice"}),
        ),
        row(
            "r4",
            "L4",
            Kind::Job,
            "x",
            Op::Patch,
            json!({"actor": "chat"}),
        ),
        row(
            "r5",
            "L5",
            Kind::Job,
            "x",
            Op::Patch,
            json!({"north_star": "nl"}),
        ),
    ];
    (layers, rows)
}

fn with_dimensions(channel: &[&str], region: Option<&str>) -> Selection {
    Selection {
        dimensions: SelectionDimensions {
            channel: channel.iter().map(|s| (*s).to_owned()).collect(),
        },
        region: region.map(str::to_owned),
        ..retail()
    }
}

#[test]
fn channel_multi_value_applies_each_layer() {
    let (layers, rows) = channel_world();
    let c = ok(run(
        &layers,
        &rows,
        &with_dimensions(&["voice", "email"], None),
        Some("x"),
    ));
    // Layer-list order, not selection order; chat is not selected.
    assert_eq!(
        c.effective.layers,
        vec![
            "base",
            "industry/retail",
            CS,
            "job/x",
            "job/x/channel=email",
            "job/x/channel=voice"
        ]
    );
    let job = c.effective.job.expect("job");
    assert_eq!(job["key"], json!("x"));
    assert_eq!(job["stance"], json!("Email"));
    assert_eq!(job["preamble"], json!("Voice"));
    assert!(!job.contains_key("actor"));
    let ops: Vec<Op> = c.effective.provenance["job:x"]
        .iter()
        .map(|o| o.op)
        .collect();
    assert_eq!(ops, vec![Op::Add, Op::Patch, Op::Patch]);
}

#[test]
fn region_selects_nothing() {
    let (layers, rows) = channel_world();
    let without = ok(run(&layers, &rows, &with_dimensions(&[], None), Some("x")));
    let with = ok(run(
        &layers,
        &rows,
        &with_dimensions(&[], Some("nl")),
        Some("x"),
    ));
    assert_eq!(
        with.effective.layers,
        vec!["base", "industry/retail", CS, "job/x"]
    );
    assert!(
        !with
            .effective
            .job
            .as_ref()
            .is_some_and(|j| j.contains_key("north_star"))
    );
    assert_eq!(with.effective, without.effective);
}

#[test]
fn areas_filter_context_spaces() {
    let layers = [
        layer("L0", "base"),
        layer("L1", "industry/retail"),
        layer("L2", "industry_dept/retail/customer_service"),
    ];
    let rows = [
        row(
            "r1",
            "L2",
            Kind::ContextSpace,
            "retail:logistics:track_parcel",
            Op::Add,
            json!({"label": "Track a parcel", "area": "logistics"}),
        ),
        row(
            "r2",
            "L2",
            Kind::ContextSpace,
            "retail:faq:shipping",
            Op::Add,
            json!({"label": "Shipping", "area": "faq"}),
        ),
    ];
    let pick = |areas: &[&str]| Selection {
        areas: areas.iter().map(|a| (*a).to_owned()).collect(),
        ..retail()
    };
    let spaces = |areas: &[&str]| {
        let c = ok(run(&layers, &rows, &pick(areas), None));
        c.effective
            .job_content
            .context_spaces
            .keys()
            .cloned()
            .collect::<Vec<String>>()
    };
    assert_eq!(
        spaces(&[]),
        vec!["retail:faq:shipping", "retail:logistics:track_parcel"]
    );
    assert_eq!(
        spaces(&["logistics"]),
        vec!["retail:logistics:track_parcel"]
    );
    assert_eq!(spaces(&["payments"]), Vec::<String>::new());
    let all = ok(run(&layers, &rows, &pick(&[]), None));
    let some = ok(run(&layers, &rows, &pick(&["faq"]), None));
    assert_ne!(all.effective.hash, some.effective.hash);
}

#[test]
fn tombstone_cascade_warns() {
    let layers = [
        layer("L0", "base"),
        layer("L1", "industry/retail"),
        layer("L9", CS),
    ];
    let rows = [
        row(
            "r1",
            "L0",
            Kind::Entity,
            "Order",
            Op::Add,
            json!({"label": "Order"}),
        ),
        row(
            "r2",
            "L0",
            Kind::Entity,
            "Payment",
            Op::Add,
            json!({"label": "Payment"}),
        ),
        row(
            "r3",
            "L0",
            Kind::Attribute,
            "Order.order_id",
            Op::Add,
            json!({"data_type": "string"}),
        ),
        row(
            "r4",
            "L0",
            Kind::Attribute,
            "Payment.amount",
            Op::Add,
            json!({"data_type": "money"}),
        ),
        row(
            "r5",
            "L0",
            Kind::Relation,
            "Order|paidBy|Payment",
            Op::Add,
            json!({"cardinality": "n:1"}),
        ),
        // A layer applies as a set: the tombstone (listed first) runs after the layer's add.
        row("r6", "L1", Kind::Entity, "Order", Op::Tombstone, json!({})),
        row(
            "r7",
            "L1",
            Kind::Attribute,
            "Order.placed_at",
            Op::Add,
            json!({"data_type": "datetime"}),
        ),
    ];
    let c = ok(run(&layers, &rows, &retail(), None));
    let e = &c.effective;
    assert_eq!(keys(&e.entities), vec!["Payment"]);
    assert_eq!(keys(&e.attributes), vec!["Payment.amount"]);
    assert!(e.relations.is_empty());
    let w: Vec<(&str, &str, &str)> = e
        .warnings
        .iter()
        .map(|w| (w.code.as_str(), w.key.as_str(), w.message.as_str()))
        .collect();
    assert_eq!(
        w,
        vec![(
            "tombstone_cascade",
            "Order",
            "Removing Order also removed Order.order_id, Order.placed_at, Order|paidBy|Payment."
        )]
    );
    // As the web: the tombstoned item keeps its provenance.
    let ops: Vec<Op> = e.provenance["entity:Order"].iter().map(|o| o.op).collect();
    assert_eq!(ops, vec![Op::Add, Op::Tombstone]);
}

#[test]
fn catalogue_folds_add_patch_tombstone() {
    let layers = [
        layer("L0", "base"),
        layer("L1", "job/x"),
        layer("L8", "industry/retail"),
        layer("L9", CS),
    ];
    let role = |label: &str| json!({"label": label, "max_side_effect": "R", "enabled": true});
    let rows = [
        row("r1", "L0", Kind::Job, "x", Op::Add, json!({"label": "X"})),
        row(
            "r2",
            "L0",
            Kind::Object,
            "order",
            Op::Add,
            json!({"label": "Order", "tier": "base"}),
        ),
        row(
            "r3",
            "L0",
            Kind::Operation,
            "order.cancel",
            Op::Add,
            json!({"description": "Cancel an order."}),
        ),
        row(
            "r4",
            "L0",
            Kind::Operation,
            "order.read",
            Op::Add,
            json!({}),
        ),
        row("r5", "L0", Kind::AgentRole, "staff", Op::Add, role("Staff")),
        row("r6", "L0", Kind::AgentRole, "old", Op::Add, role("Old")),
        row(
            "r7",
            "L1",
            Kind::Object,
            "order",
            Op::Patch,
            json!({"label": "Purchase", "tier": "base"}),
        ),
        row("r8", "L1", Kind::AgentRole, "old", Op::Tombstone, json!({})),
        row(
            "r9",
            "L1",
            Kind::JobSetting,
            "stance",
            Op::Add,
            json!({"label": "Stance"}),
        ),
    ];
    let c = ok(run(&layers, &rows, &retail(), Some("x")));
    let cat = &c.catalogue;
    assert_eq!(cat.objects.len(), 1);
    assert_eq!(
        (
            cat.objects[0].key.as_str(),
            cat.objects[0].row_id.as_str(),
            cat.objects[0].layer.as_str()
        ),
        ("order", "r7", "job/x")
    );
    assert_eq!(cat.objects[0].fields["label"], json!("Purchase"));
    let ops: Vec<&str> = cat.operations.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(ops, vec!["order.cancel", "order.read"]);
    assert_eq!(names(&cat.agent_roles), vec!["staff"]);
    assert_eq!(names(&cat.job_settings), vec!["stance"]);
    assert!(cat.error_states.is_empty() && cat.gates.is_empty());
    // Overlay rows first, then the catalogue layer by layer; a layer's tombstones after its adds and patches.
    assert_eq!(
        c.provenance_rows,
        vec!["r1", "r2", "r3", "r4", "r5", "r6", "r7", "r9", "r8"]
    );
}

#[test]
fn effective_hash_is_stable_and_covers_the_config() {
    let layers = [layer("L0", "base")];
    let mut rows = [
        row(
            "r1",
            "L0",
            Kind::Entity,
            "Order",
            Op::Add,
            json!({"label": "Order"}),
        ),
        row(
            "r2",
            "L0",
            Kind::Attribute,
            "Order.order_id",
            Op::Add,
            json!({"data_type": "string"}),
        ),
    ];
    let a = ok(run(&layers, &rows, &Selection::default(), None));
    let b = ok(run(&layers, &rows, &Selection::default(), None));
    assert_eq!(a.effective.hash, b.effective.hash);
    assert_eq!(a.effective.hash.len(), "sha256:".len() + 64);
    assert_eq!(a.effective.hash, effective_hash(&a.effective));

    // The hash input (plan §6): the UI EffectiveConfig minus hash, provenance and warnings.
    let e = &a.effective;
    let formula = json!({
        "entities": e.entities,
        "attributes": e.attributes,
        "relations": e.relations,
        "job": e.job,
        "job_content": e.job_content,
        "layers": e.layers,
        "rules_fired": e.rules_fired,
    });
    assert_eq!(
        e.hash,
        format!("sha256:{}", sha256_hex(&canonical_json(&formula)))
    );

    // Provenance and warnings are not hashed …
    let mut moved = a.effective.clone();
    moved.warnings.clear();
    moved.provenance.clear();
    assert_eq!(effective_hash(&moved), a.effective.hash);

    // … a field change is.
    rows[0].fields = json!({"label": "Purchase"});
    let c = ok(run(&layers, &rows, &Selection::default(), None));
    assert_ne!(c.effective.hash, a.effective.hash);
}

/// Ruling 18a: the compiled catalogue carries the RESOLVED operations (variant merged with `*`, generated
/// `error_states`) over the folded objects, a job-layer object patch included.
#[test]
fn catalogue_operations_are_resolved() {
    let layers = [
        layer("L0", "base"),
        layer("L1", "job/x"),
        layer("L8", "industry/retail"),
        layer("L9", CS),
    ];
    let order = |states: &[&str]| {
        json!({
            "label": "Order",
            "tier": "base",
            "states": states.iter().map(|s| json!({"status": s, "label": s})).collect::<Vec<_>>(),
            "transitions": [{"verb": "order.cancel", "from_status": "open", "to_status": "cancelled"}]
        })
    };
    let rows = [
        row("r1", "L0", Kind::Job, "x", Op::Add, json!({"label": "X"})),
        row(
            "r2",
            "L0",
            Kind::Object,
            "order",
            Op::Add,
            order(&["open", "cancelled"]),
        ),
        row(
            "r3",
            "L0",
            Kind::Operation,
            "order.cancel{fraud}",
            Op::Add,
            json!({"attrs": {"identity_level": "S"}, "extra_errors": ["outside_hours"]}),
        ),
        row(
            "r4",
            "L0",
            Kind::Operation,
            "order.cancel",
            Op::Add,
            json!({"attrs": {
                "modality": "do", "is_derive": false, "identity_level": "2", "side_effect": "X",
                "idempotency": "key", "input_fields": ["order_id"], "output_fields": ["status"],
                "confirm_back": true, "verify_read": null, "builtin": false, "op_kind": "standard",
                "guard_rules": ["RET-01"]
            }}),
        ),
        // the job adds a state: the operation's invalid states follow the patched object
        row(
            "r5",
            "L1",
            Kind::Object,
            "order",
            Op::Patch,
            order(&["open", "shipped", "cancelled"]),
        ),
    ];
    let c = ok(run(&layers, &rows, &retail(), Some("x")));
    let ops = &c.catalogue.operations;
    let keys: Vec<&str> = ops.iter().map(|o| o.key.as_str()).collect();
    assert_eq!(keys, vec!["order.cancel", "order.cancel{fraud}"]);
    let star_errors = vec![
        "identity_required",
        "invalid_input",
        "conflict",
        "invalid_state:shipped",
        "invalid_state:cancelled",
        "ineligible:RET-01",
        "upstream_unavailable",
        "timeout",
        "rate_limited",
        "connector_auth_failed",
        "unknown_outcome",
    ];
    assert_eq!(ops[0].error_states, star_errors);
    assert_eq!(ops[1].overridden, vec!["identity_level"]);
    assert_eq!(ops[1].attrs["identity_level"], json!("S"));
    assert_eq!(ops[1].attrs["side_effect"], json!("X"));
    assert_eq!(
        ops[1].error_states,
        vec![
            "identity_required",
            "invalid_input",
            "conflict",
            "outside_hours",
            "invalid_state:shipped",
            "invalid_state:cancelled",
            "ineligible:RET-01",
            "upstream_unavailable",
            "timeout",
            "rate_limited",
            "connector_auth_failed",
            "unknown_outcome",
        ]
    );
}

/// Ruling 18b: an `axis_rule` or `profile` row the compile cannot read is an `invalid_row` error, every one of
/// them, never a silently dropped rule or profile.
#[test]
fn unreadable_rule_and_profile_rows_are_reported() {
    let rows = [
        row(
            "a1",
            "L0",
            Kind::AxisRule,
            "ok_rule",
            Op::Add,
            json!({"priority": 1, "condition": {"eq": ["delivery_type", "pickup"]}, "layer": "profile/ok_rule"}),
        ),
        row(
            "a2",
            "L0",
            Kind::AxisRule,
            "bad_rule",
            Op::Add,
            json!({"priority": "high", "condition": {"eq": ["delivery_type", "pickup"]}, "layer": "profile/bad_rule"}),
        ),
        row("a3", "L0", Kind::AxisRule, "gone", Op::Tombstone, json!({})),
        row(
            "p1",
            "L0",
            Kind::Profile,
            "no_selection",
            Op::Add,
            json!({"label": "No selection"}),
        ),
        row(
            "p2",
            "L0",
            Kind::Profile,
            "bad_selection",
            Op::Add,
            json!({"label": "Bad", "selection": {"delivery_type": 3}}),
        ),
        row(
            "p3",
            "L0",
            Kind::Profile,
            "fine",
            Op::Add,
            json!({"label": "Fine", "selection": {"delivery_type": ["pickup"]}}),
        ),
        row("p4", "L0", Kind::Profile, "gone", Op::Tombstone, json!({})),
    ];
    let rule_errors = AxisRuleDef::from_rows(&rows).expect_err("bad_rule is unreadable");
    assert_eq!(
        rule_errors
            .iter()
            .map(|e| (e.code.as_str(), e.key.as_str(), e.row_id.as_deref()))
            .collect::<Vec<_>>(),
        vec![("invalid_row", "bad_rule", Some("a2"))]
    );
    assert!(
        rule_errors[0].message.contains("layer L0"),
        "{rule_errors:?}"
    );
    let profile_errors = ProfileDef::from_rows(&rows).expect_err("two unreadable profiles");
    assert_eq!(
        profile_errors
            .iter()
            .map(|e| (e.code.as_str(), e.key.as_str(), e.row_id.as_deref()))
            .collect::<Vec<_>>(),
        vec![
            ("invalid_row", "no_selection", Some("p1")),
            ("invalid_row", "bad_selection", Some("p2")),
        ]
    );
    // the readable rows alone parse, tombstones skipped
    let fine: Vec<Row> = rows
        .iter()
        .filter(|r| ["a1", "a3", "p3", "p4"].contains(&r.id.as_str()))
        .cloned()
        .collect();
    let rules = AxisRuleDef::from_rows(&fine).expect("readable rules");
    assert_eq!(
        rules.iter().map(|r| r.key.as_str()).collect::<Vec<_>>(),
        vec!["ok_rule"]
    );
    let profiles = ProfileDef::from_rows(&fine).expect("readable profiles");
    assert_eq!(
        profiles.iter().map(|p| p.key.as_str()).collect::<Vec<_>>(),
        vec!["fine"]
    );
    // the compile path reports them: `run` returns the definition errors as the compile's
    let layers = [layer("L0", "base")];
    let errors = run(&layers, &rows, &Selection::default(), None).expect_err("unreadable rows");
    assert_eq!(codes(&errors), vec!["invalid_row"]);
}

// ---- owner ruling: the vulnerability axis and `vulnerable_arrangement` ------------------------------------------

// ---- KH T1 (D106/D115): the selection hierarchy ------------------------------------------------------------------

/// A world with two retail departments and one utilities department.
fn hierarchy_world() -> Vec<LayerMeta> {
    vec![
        layer("L0", "base"),
        layer("L1", "industry/retail"),
        layer("L2", CS),
        layer("L3", "industry_dept/retail/returns"),
        layer("L4", "industry/utilities"),
        layer("L5", "industry_dept/utilities/billing"),
        layer("L6", "job/x"),
    ]
}

fn paths(errors: &[CompileError]) -> Vec<(&str, &str)> {
    errors
        .iter()
        .map(|e| (e.code.as_str(), e.key.as_str()))
        .collect()
}

#[test]
fn a_job_without_industry_or_department_is_refused_hierarchy_incomplete() {
    let layers = hierarchy_world();
    let rows = [row(
        "r1",
        "L0",
        Kind::Job,
        "x",
        Op::Add,
        json!({"label": "X"}),
    )];
    let none = Selection::default();
    let errors = run(&layers, &rows, &none, Some("x")).expect_err("no hierarchy");
    assert_eq!(
        paths(&errors),
        vec![
            ("hierarchy_incomplete", "selection.industry"),
            ("hierarchy_incomplete", "selection.departments"),
        ]
    );
    let r = refusal(&errors).expect("a refusal");
    assert_eq!((r.status, r.code), (422, "hierarchy_incomplete"));

    let no_dept = Selection {
        industry: "retail".to_owned(),
        ..Selection::default()
    };
    let errors = run(&layers, &rows, &no_dept, None).expect_err("no department");
    assert_eq!(
        paths(&errors),
        vec![("hierarchy_incomplete", "selection.departments")]
    );
    // a profile alone is not the base-only preview either
    let profile_only = Selection {
        profile: Some("p".to_owned()),
        ..Selection::default()
    };
    assert!(run(&layers, &rows, &profile_only, None).is_err());
}

#[test]
fn a_department_of_another_industry_or_twice_is_refused() {
    let layers = hierarchy_world();
    let mixed = Selection {
        departments: vec![
            CS.to_owned(),
            "industry_dept/utilities/billing".to_owned(),
            CS.to_owned(),
            "customer_service".to_owned(),
        ],
        ..retail()
    };
    let errors = run(&layers, &[], &mixed, None).expect_err("refused");
    assert_eq!(
        paths(&errors),
        vec![
            ("department_industry_mismatch", "selection.departments[1]"),
            ("duplicate_department", "selection.departments[2]"),
            ("invalid_selection", "selection.departments[3]"),
        ]
    );
    let r = refusal(&errors).expect("a refusal");
    assert_eq!((r.status, r.code), (422, "department_industry_mismatch"));
    assert_eq!(
        refusal(
            &run(
                &layers,
                &[],
                &Selection {
                    departments: vec![CS.into(), CS.into()],
                    ..retail()
                },
                None
            )
            .expect_err("duplicate")
        )
        .map(|r| r.code),
        Some("duplicate_department")
    );
}

#[test]
fn two_departments_compile_in_declared_order_and_round_trip() {
    let layers = hierarchy_world();
    let returns = "industry_dept/retail/returns";
    for order in [[returns, CS], [CS, returns]] {
        let sel = Selection {
            departments: order.iter().map(|d| (*d).to_owned()).collect(),
            ..retail()
        };
        let c = ok(run(&layers, &[], &sel, None));
        assert_eq!(
            c.effective.layers,
            vec!["base", "industry/retail", order[0], order[1]]
        );
        assert_eq!(sel.primary_department(), Some(order[0]));
        let back: Selection =
            serde_json::from_value(serde_json::to_value(&sel).expect("serialise")).expect("parse");
        assert_eq!(back, sel, "the order survives a JSON round trip");
    }
}

#[test]
fn an_unknown_department_of_the_industry_is_a_compile_error() {
    let layers = hierarchy_world();
    let sel = Selection {
        departments: vec!["industry_dept/retail/nowhere".to_owned()],
        ..retail()
    };
    let errors = run(&layers, &[], &sel, None).expect_err("no such layer");
    assert_eq!(
        paths(&errors),
        vec![("invalid_selection", "selection.departments[0]")]
    );
    assert!(refusal(&errors).is_none());
}

#[test]
fn the_base_only_preview_compiles_and_is_labelled() {
    let layers = hierarchy_world();
    let c = ok(run(&layers, &[], &Selection::default(), None));
    assert_eq!(c.effective.layers, vec!["base"]);
    let codes: Vec<&str> = c
        .effective
        .warnings
        .iter()
        .map(|w| w.code.as_str())
        .collect();
    assert_eq!(codes, vec!["base_only_preview"]);
    // only the explicit preview exempts it: a job-context compile (Mode::Job) refuses the empty selection
    let departments = DepartmentRef::from_layers(&layers);
    let input = CompileInput {
        layers: &layers,
        rows: &[],
        rules: &[],
        profiles: &[],
        departments: &departments,
    };
    let errors =
        compile(&input, &Selection::default(), None).expect_err("Mode::Job has no exemption");
    assert_eq!(
        paths(&errors),
        vec![
            ("hierarchy_incomplete", "selection.industry"),
            ("hierarchy_incomplete", "selection.departments"),
        ]
    );
    // a job's compile is never labelled
    let full = ok(run(&layers, &[], &retail(), None));
    assert!(
        full.effective
            .warnings
            .iter()
            .all(|w| w.code != "base_only_preview")
    );
}

#[test]
fn a_legacy_stored_selection_reads_as_incomplete_never_as_base_only() {
    let legacy: Selection = serde_json::from_value(json!({
        "profile": "retail_ecommerce",
        "industry": "retail",
        "industry_depts": ["customer_service"],
        "areas": [],
        "dimensions": {"channel": ["chat"], "region": ["nl"]},
    }))
    .expect("a legacy selection still parses");
    assert!(legacy.departments.is_empty() && legacy.region.is_none());
    assert_eq!(
        legacy
            .hierarchy_issues()
            .iter()
            .map(|i| (i.code.as_str(), i.path.as_str()))
            .collect::<Vec<_>>(),
        vec![("hierarchy_incomplete", "selection.departments")]
    );
    let null_industry: Selection =
        serde_json::from_value(json!({"industry": null})).expect("null industry parses");
    assert_eq!(null_industry.industry, "");
}

#[test]
fn one_mapper_picks_the_refusal_code_of_a_mixed_list() {
    // KH T1 fix 4: `refusal` (compile errors) and the API's issue lists map through `issues_refusal`
    let sel = Selection {
        departments: vec!["bogus".to_owned(), "industry_dept/x/y".to_owned()],
        ..retail()
    };
    let errors = run(&hierarchy_world(), &[], &sel, None).expect_err("refused");
    let from_compile = refusal(&errors).expect("a refusal");
    let from_issues = agent_model::compile::issues_refusal(
        sel.hierarchy_issues()
            .iter()
            .map(|i| (i.code.as_str(), i.message.as_str())),
    )
    .expect("a refusal");
    assert_eq!(from_compile, from_issues);
    assert_eq!(from_compile.code, "department_industry_mismatch");
    assert!(agent_model::compile::issues_refusal(std::iter::empty()).is_none());
}

// ---- owner ruling 2026-10-23: D60 archetypes carry instruction rows ----------------------------------------------

const RETURNS: &str = "industry_dept/retail/returns";

/// Base (tagged `base@v3`) defines archetype `service_desk` with an escalation and a policy instruction plus a base
/// policy of its own; customer service and returns both extend it.
fn archetype_world(instructions: Value) -> (Vec<LayerMeta>, Vec<Row>) {
    let mut layers = vec![
        layer("L0", "base"),
        layer("L1", "industry/retail"),
        layer("L2", CS),
        layer("L3", RETURNS),
        layer("L6", "job/x"),
    ];
    layers[0].tag = Some("base@v3".to_owned());
    layers[2].extends = Some("service_desk".to_owned());
    layers[3].extends = Some("service_desk".to_owned());
    let mut archetype = json!({"label": "Service desk"});
    if !instructions.is_null() {
        archetype["instructions"] = instructions;
    }
    let rows = vec![
        row("job", "L0", Kind::Job, "x", Op::Add, json!({"label": "X"})),
        row(
            "a1",
            "L0",
            Kind::DepartmentArchetype,
            "service_desk",
            Op::Add,
            archetype,
        ),
        row(
            "p0",
            "L0",
            Kind::Instruction,
            "policy",
            Op::Add,
            json!({"text": "Be honest."}),
        ),
    ];
    (layers, rows)
}

fn desk_instructions() -> Value {
    json!({
        "escalation": {"text": "Hand security incidents to a person at once."},
        "policy": {"text": "Never ask for a password."}
    })
}

fn in_departments(departments: &[&str]) -> Selection {
    Selection {
        departments: departments.iter().map(|d| (*d).to_owned()).collect(),
        ..retail()
    }
}

fn section_layers(c: &Compiled, slot: &str) -> Vec<String> {
    c.catalogue
        .instructions
        .iter()
        .find(|i| i.key == slot)
        .map(|i| i.sections.iter().map(|s| s.marker()).collect())
        .unwrap_or_default()
}

fn slot<'c>(c: &'c Compiled, slot: &str) -> Option<&'c str> {
    agent_model::kinds::instruction::slot_text(&c.catalogue.instructions, slot)
}

#[test]
fn an_archetype_instruction_folds_into_every_department_that_extends_it() {
    let (layers, rows) = archetype_world(desk_instructions());
    for dept in [CS, RETURNS] {
        let c = ok(run(&layers, &rows, &in_departments(&[dept]), None));
        // the archetype step comes after base: its policy replaces base's
        assert_eq!(
            slot(&c, "policy"),
            Some("Never ask for a password."),
            "{dept}"
        );
        assert_eq!(
            slot(&c, "escalation"),
            Some("Hand security incidents to a person at once."),
            "{dept}"
        );
        assert_eq!(
            section_layers(&c, "escalation"),
            vec!["[layer department_archetype/service_desk@v3]"],
            "{dept}: the section names the archetype at the base pin"
        );
        assert!(c.provenance_rows.iter().any(|r| r == "a1"), "{dept}");
        assert!(
            !c.provenance_rows.iter().any(|r| r.starts_with("a1!")),
            "{dept}: provenance names the archetype row, never a synthetic id"
        );
    }
    // both departments of one job: the shared archetype folds once
    let both = ok(run(&layers, &rows, &in_departments(&[CS, RETURNS]), None));
    assert_eq!(section_layers(&both, "escalation").len(), 1);
}

#[test]
fn a_department_append_composes_on_top_of_its_archetype() {
    let (layers, mut rows) = archetype_world(desk_instructions());
    rows.push(row(
        "e2",
        "L2",
        Kind::Instruction,
        "escalation",
        Op::Add,
        json!({"text": "Refunds over 100 EUR go to a supervisor.", "mode": "append"}),
    ));
    rows.push(row(
        "p2",
        "L2",
        Kind::Instruction,
        "policy",
        Op::Add,
        json!({"text": "Quote the returns policy."}),
    ));
    let c = ok(run(&layers, &rows, &in_departments(&[CS]), None));
    assert_eq!(
        slot(&c, "escalation"),
        Some(
            "Hand security incidents to a person at once.\n\nRefunds over 100 EUR go to a supervisor."
        )
    );
    assert_eq!(
        section_layers(&c, "escalation"),
        vec![
            "[layer department_archetype/service_desk@v3]",
            "[layer industry_dept/retail/customer_service@head]",
        ]
    );
    // a department replace still replaces the archetype's slot
    assert_eq!(slot(&c, "policy"), Some("Quote the returns policy."));
    // returns keeps the archetype's text alone
    let r = ok(run(&layers, &rows, &in_departments(&[RETURNS]), None));
    assert_eq!(slot(&r, "policy"), Some("Never ask for a password."));
}

#[test]
fn an_archetype_without_instructions_changes_nothing() {
    let (layers, plain) = archetype_world(Value::Null);
    let (_, empty) = archetype_world(json!({}));
    for dept in [CS, RETURNS] {
        let a = ok(run(&layers, &plain, &in_departments(&[dept]), None));
        let b = ok(run(&layers, &empty, &in_departments(&[dept]), None));
        assert_eq!(a, b, "{dept}");
        assert_eq!(slot(&a, "policy"), Some("Be honest."));
        assert_eq!(section_layers(&a, "policy"), vec!["[layer base@v3]"]);
    }
}

#[test]
fn sibling_archetypes_that_disagree_on_a_slot_are_a_department_conflict() {
    let (mut layers, mut rows) = archetype_world(desk_instructions());
    layers[3].extends = Some("returns_desk".to_owned());
    rows.push(row(
        "a2",
        "L0",
        Kind::DepartmentArchetype,
        "returns_desk",
        Op::Add,
        json!({"label": "Returns desk", "instructions": {"escalation": {"text": "Escalate damaged goods."}}}),
    ));
    let errors = run(&layers, &rows, &in_departments(&[CS, RETURNS]), None).expect_err("conflict");
    assert_eq!(
        paths(&errors),
        vec![("department_conflict", "instruction:escalation")]
    );
    // a job row decides the slot: neither archetype's text folds
    rows.push(row(
        "j1",
        "L6",
        Kind::Instruction,
        "escalation",
        Op::Add,
        json!({"text": "Escalate to the shift lead."}),
    ));
    let c = ok(run(
        &layers,
        &rows,
        &in_departments(&[CS, RETURNS]),
        Some("x"),
    ));
    assert_eq!(slot(&c, "escalation"), Some("Escalate to the shift lead."));
    assert_eq!(slot(&c, "policy"), Some("Never ask for a password."));
}
