//! KH T4 (D108): the 26th layer-row kind `instruction` and inheritance — the per-slot fold (replace, append,
//! tombstone; a job's `preamble` is its job layer's `identity`), settings that inherit the nearest value, rule floors
//! that only tighten (`floor_loosened`), the bundle's `objects.instruction` (absent, and the hash unchanged, without
//! instructions), the runtime design and the qualifier reading the folded `identity`, and tenant refinements that may
//! touch `tone` / `escalation` / `format` only (422 `instruction_not_refinable`).
use agent_api::audit::Actor;
use agent_api::design::DesignOf;
use agent_api::platform::db::{rows, tags};
use agent_api::platform::hooks::NoHooks;
use agent_api::platform::templates::{self, TemplatePin};
use agent_api::platform_db::begin_platform;
use agent_api::release::bundle::Bundle;
use agent_api::release::compile::{TemplateCompiler, settings_of};
use agent_api::runtime::design::RuntimeDesign;
use agent_api::turn::qualify;
use agent_model::compile::{
    AxisRuleDef, CompileError, CompileInput, Compiled, DepartmentRef, LayerMeta, ProfileDef, Row,
    Selection, compile,
};
use agent_model::kinds::instruction::{FoldedInstruction, slot_text};
use agent_model::kinds::{Kind, Op};
use agent_model::layer::LayerKey;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob};
use crate::common::{TestEnv, get, insert_tenant, json as req, send, super_admin};

const CCA: &str = "job/customer_care_agent";
const CS: &str = "industry_dept/retail/customer_service";
/// The test offering's job-row preamble (`job/customer_care_agent@v4`, `common::platform`): the job row lives in the
/// base layer, so base@v1 is where the identity comes from.
const SEED_PREAMBLE: &str = "You are a helpful assistant.";
/// The layer marker of the offering's identity.
const PREAMBLE_MARKER: &str = "[layer base@v1]";
// ---- pure compile helpers ------------------------------------------------------------------------------------------

fn layer(id: &str, label: &str, tag: Option<&str>) -> LayerMeta {
    LayerMeta {
        id: id.to_owned(),
        key: LayerKey::parse_label(label).unwrap_or_else(|e| panic!("{label}: {e}")),
        extends: None,
        tag: tag.map(str::to_owned),
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

fn retail() -> Selection {
    Selection {
        industry: "retail".to_owned(),
        departments: vec![CS.to_owned()],
        ..Selection::default()
    }
}

/// base (L0, tagged base@v3) → industry/retail (L1) → the department (L2) → job/x (L3) → job/x/channel=email (L4).
fn world() -> Vec<LayerMeta> {
    vec![
        layer("L0", "base", Some("base@v3")),
        layer("L1", "industry/retail", Some("industry/retail@v2")),
        layer("L2", CS, Some("industry_dept/retail/customer_service@v2")),
        layer("L3", "job/x", Some("job/x@v5")),
        layer("L4", "job/x/channel=email", Some("job/x@v5")),
    ]
}

fn run(layers: &[LayerMeta], rows: &[Row], sel: &Selection) -> Result<Compiled, Vec<CompileError>> {
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
    compile(&input, sel, Some("x"))
}

fn ok(r: Result<Compiled, Vec<CompileError>>) -> Compiled {
    r.unwrap_or_else(|e| panic!("unexpected compile errors: {e:#?}"))
}

fn instr(id: &str, layer: &str, slot: &str, op: Op, fields: Value) -> Row {
    row(id, layer, Kind::Instruction, slot, op, fields)
}

fn job_x(extra: Vec<Row>) -> Vec<Row> {
    let mut rows = vec![row(
        "j0",
        "L0",
        Kind::Job,
        "x",
        Op::Add,
        json!({"label": "X", "actor": "customer care"}),
    )];
    rows.extend(extra);
    rows
}

fn markers(set: &[FoldedInstruction], slot: &str) -> Vec<String> {
    set.iter()
        .find(|i| i.key == slot)
        .map(|i| i.sections.iter().map(|s| s.marker()).collect())
        .unwrap_or_default()
}

// ---- the fold ------------------------------------------------------------------------------------------------------

#[test]
fn instructions_fold_per_slot_with_append_and_tombstone() {
    let rows = job_x(vec![
        instr("i1", "L0", "tone", Op::Add, json!({"text": "Be kind."})),
        instr(
            "i2",
            "L0",
            "policy",
            Op::Add,
            json!({"text": "No legal advice."}),
        ),
        instr(
            "i3",
            "L0",
            "safety",
            Op::Add,
            json!({"text": "Never share card numbers."}),
        ),
        instr(
            "i4",
            "L1",
            "tone",
            Op::Add,
            json!({"text": "Use first names.", "mode": "append", "language": "en"}),
        ),
        instr(
            "i5",
            "L2",
            "policy",
            Op::Patch,
            json!({"text": "Returns within 30 days."}),
        ),
        instr(
            "i6",
            "L3",
            "safety",
            Op::Tombstone,
            json!({"reason": "the job has its own"}),
        ),
        instr(
            "i7",
            "L4",
            "format",
            Op::Add,
            json!({"text": "Plain text, no markdown.", "mode": "append"}),
        ),
        // the job row's preamble is the job layer's identity
        row(
            "j1",
            "L3",
            Kind::Job,
            "x",
            Op::Patch,
            json!({"preamble": "  You help X customers.  "}),
        ),
    ]);
    let mut email = retail();
    email.dimensions.channel = vec!["email".to_owned()];
    let c = ok(run(&world(), &rows, &email));
    let set = &c.catalogue.instructions;
    let keys: Vec<&str> = set.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(
        keys,
        ["identity", "policy", "tone", "format"],
        "slot order; safety tombstoned"
    );
    assert_eq!(slot_text(set, "identity"), Some("You help X customers."));
    assert_eq!(markers(set, "identity"), ["[layer job/x@v5]"]);
    assert_eq!(
        slot_text(set, "policy"),
        Some("Returns within 30 days."),
        "replace"
    );
    assert_eq!(
        markers(set, "policy"),
        ["[layer industry_dept/retail/customer_service@v2]"]
    );
    assert_eq!(
        slot_text(set, "tone"),
        Some("Be kind.\n\nUse first names."),
        "append"
    );
    assert_eq!(
        markers(set, "tone"),
        ["[layer base@v3]", "[layer industry/retail@v2]"]
    );
    assert_eq!(set[2].sections[1].language.as_deref(), Some("en"));
    assert_eq!(markers(set, "format"), ["[layer job/x/channel=email@v5]"]);
    // the settings bases: preamble = the folded identity
    assert_eq!(
        settings_of(&c).preamble.as_deref(),
        Some("You help X customers.")
    );
    assert!(c.provenance_rows.iter().any(|r| r == "i7"));

    // the job layer's own identity row wins over its preamble; a later append adds a section
    let mut rows2 = rows.clone();
    rows2.push(instr(
        "i8",
        "L3",
        "identity",
        Op::Add,
        json!({"text": "You are X's agent."}),
    ));
    rows2.push(instr(
        "i9",
        "L4",
        "identity",
        Op::Add,
        json!({"text": "You answer email.", "mode": "append"}),
    ));
    let c2 = ok(run(&world(), &rows2, &email));
    assert_eq!(
        slot_text(&c2.catalogue.instructions, "identity"),
        Some("You are X's agent.\n\nYou answer email.")
    );
    assert_eq!(
        markers(&c2.catalogue.instructions, "identity"),
        ["[layer job/x@v5]", "[layer job/x/channel=email@v5]"]
    );

    // a tombstone in the job layer removes an inherited identity, preamble and all
    let mut rows3 = rows.clone();
    rows3.push(instr(
        "i10",
        "L0",
        "identity",
        Op::Add,
        json!({"text": "Base identity."}),
    ));
    rows3.push(instr("i11", "L3", "identity", Op::Tombstone, json!({})));
    let c3 = ok(run(&world(), &rows3, &retail()));
    assert_eq!(slot_text(&c3.catalogue.instructions, "identity"), None);

    // no instruction rows and no preamble: nothing folded, and nothing serialised
    let bare = ok(run(&world(), &job_x(Vec::new()), &retail()));
    assert!(bare.catalogue.instructions.is_empty());
    let v = serde_json::to_value(&bare.catalogue).unwrap();
    assert!(v.get("instructions").is_none(), "{v}");
}

#[test]
fn instruction_rows_are_checked_like_every_row() {
    let layers = world();
    let rows = job_x(vec![
        instr("bad1", "L0", "persona", Op::Add, json!({"text": "x"})),
        instr("bad2", "L1", "tone", Op::Add, json!({"text": " "})),
        instr(
            "bad3",
            "L2",
            "tone",
            Op::Add,
            json!({"text": "x", "mode": "prepend"}),
        ),
    ]);
    let errors = run(&layers, &rows, &retail()).expect_err("three bad rows");
    let got: Vec<(&str, Option<&str>)> = errors
        .iter()
        .map(|e| (e.code.as_str(), e.row_id.as_deref()))
        .collect();
    assert_eq!(
        got,
        [
            ("invalid_row", Some("bad1")),
            ("invalid_row", Some("bad2")),
            ("invalid_row", Some("bad3")),
        ]
    );
    assert!(!Kind::Instruction.allowed_in(agent_model::layer::Scope::Profile));
}

// ---- settings and floors ---------------------------------------------------------------------------------------------

fn setting(id: &str, layer: &str, key: &str, op: Op, fields: Value) -> Row {
    row(id, layer, Kind::JobSetting, key, op, fields)
}

#[test]
fn settings_take_the_nearest_value_down_the_fold() {
    let rows = job_x(vec![
        setting(
            "s1",
            "L0",
            "stance",
            Op::Add,
            json!({"label": "Stance", "value": "warm"}),
        ),
        setting(
            "s2",
            "L1",
            "actor_note",
            Op::Add,
            json!({"label": "Note", "value": "retail"}),
        ),
        // the department relabels without a value: the base value stays
        setting(
            "s3",
            "L2",
            "stance",
            Op::Patch,
            json!({"label": "Stance (care)"}),
        ),
        setting(
            "s4",
            "L3",
            "actor_note",
            Op::Patch,
            json!({"label": "Note", "value": "job"}),
        ),
    ]);
    let c = ok(run(&world(), &rows, &retail()));
    let get = |k: &str| {
        c.catalogue
            .job_settings
            .iter()
            .find(|s| s.key == k)
            .unwrap_or_else(|| panic!("setting {k}"))
    };
    let stance = get("stance");
    assert_eq!(stance.fields["value"], json!("warm"));
    assert_eq!(stance.fields["label"], json!("Stance (care)"));
    assert_eq!(stance.layer, CS);
    assert_eq!(get("actor_note").fields["value"], json!("job"));
    // the bundle base: the job row's actor; stance from the nearest job_setting value
    let s = settings_of(&c);
    assert_eq!(s.actor.as_deref(), Some("customer care"));
    assert_eq!(s.stance.as_deref(), Some("warm"));
    assert_eq!(s.preamble, None);
}

/// Fix round 1 (review I2): a `job` row's `actor` / `stance` folds as its layer's setting, so the nearest layer wins in
/// fold order whichever row kind set it.
#[test]
fn job_row_settings_and_job_settings_take_the_nearest_layer() {
    let base_row = |fields: Value| row("j0", "L0", Kind::Job, "x", Op::Add, fields);
    // a base job row's stance, a department job_setting's stance: the department is nearer
    let rows = vec![
        base_row(json!({"label": "X", "stance": "base row stance"})),
        setting(
            "s1",
            "L2",
            "stance",
            Op::Add,
            json!({"label": "Stance", "value": "department stance"}),
        ),
    ];
    let c = ok(run(&world(), &rows, &retail()));
    assert_eq!(settings_of(&c).stance.as_deref(), Some("department stance"));
    // the reverse depth: a base job_setting, then the job layer's job row: the job layer is nearer
    let rows = vec![
        base_row(json!({"label": "X"})),
        setting(
            "s1",
            "L0",
            "stance",
            Op::Add,
            json!({"label": "Stance", "value": "base setting stance"}),
        ),
        row(
            "j1",
            "L3",
            Kind::Job,
            "x",
            Op::Patch,
            json!({"stance": "job row stance"}),
        ),
    ];
    let c = ok(run(&world(), &rows, &retail()));
    assert_eq!(settings_of(&c).stance.as_deref(), Some("job row stance"));
    let stance = c
        .catalogue
        .job_settings
        .iter()
        .find(|s| s.key == "stance")
        .unwrap();
    assert_eq!(
        (stance.layer.as_str(), stance.row_id.as_str()),
        ("job/x", "j1")
    );
    // the same layer's own job_setting wins over its job row
    let rows = vec![
        base_row(json!({"label": "X", "actor": "row actor"})),
        setting(
            "s1",
            "L0",
            "actor",
            Op::Add,
            json!({"label": "Actor", "value": "setting actor"}),
        ),
    ];
    let c = ok(run(&world(), &rows, &retail()));
    assert_eq!(settings_of(&c).actor.as_deref(), Some("setting actor"));
}

/// Fix round 1 (controller ruling 4): a `job_setting` keyed `preamble` folds as an `identity` instruction at its own
/// layer position (its `value`, else its `summary`), never silently ignored; a tombstone removes the identity.
#[test]
fn a_preamble_setting_folds_as_identity_at_its_layer() {
    let preamble =
        |id: &str, layer: &str, op: Op, fields: Value| setting(id, layer, "preamble", op, fields);
    let rows = job_x(vec![
        preamble(
            "p0",
            "L0",
            Op::Add,
            json!({"label": "Preamble", "value": "Base identity."}),
        ),
        preamble(
            "p1",
            "L2",
            Op::Add,
            json!({"label": "Preamble", "summary": "Department identity."}),
        ),
    ]);
    let c = ok(run(&world(), &rows, &retail()));
    let set = &c.catalogue.instructions;
    assert_eq!(slot_text(set, "identity"), Some("Department identity."));
    assert_eq!(
        markers(set, "identity"),
        ["[layer industry_dept/retail/customer_service@v2]"]
    );
    assert_eq!(
        settings_of(&c).preamble.as_deref(),
        Some("Department identity.")
    );
    // only the base one: it stays at the base position
    let only_base = job_x(vec![preamble(
        "p0",
        "L0",
        Op::Add,
        json!({"label": "Preamble", "value": "Base identity."}),
    )]);
    let c = ok(run(&world(), &only_base, &retail()));
    assert_eq!(
        markers(&c.catalogue.instructions, "identity"),
        ["[layer base@v3]"]
    );
    // in one layer: the job_setting wins over the job row's preamble field; the layer's identity row over both
    let mut same = job_x(vec![
        preamble(
            "p3",
            "L3",
            Op::Add,
            json!({"label": "Preamble", "value": "Setting identity."}),
        ),
        row(
            "j1",
            "L3",
            Kind::Job,
            "x",
            Op::Patch,
            json!({"preamble": "Row identity."}),
        ),
    ]);
    let c = ok(run(&world(), &same, &retail()));
    assert_eq!(
        slot_text(&c.catalogue.instructions, "identity"),
        Some("Setting identity.")
    );
    same.push(instr(
        "i1",
        "L3",
        "identity",
        Op::Add,
        json!({"text": "Instruction identity."}),
    ));
    let c = ok(run(&world(), &same, &retail()));
    assert_eq!(
        slot_text(&c.catalogue.instructions, "identity"),
        Some("Instruction identity.")
    );
    // a lower layer's preamble tombstone removes the inherited identity
    let mut gone = rows.clone();
    gone.push(preamble("p4", "L3", Op::Tombstone, json!({})));
    let c = ok(run(&world(), &gone, &retail()));
    assert_eq!(slot_text(&c.catalogue.instructions, "identity"), None);
}

#[test]
fn a_looser_floor_is_a_compile_error() {
    let floor = |id: &str, layer: &str, op: Op, fields: Value| {
        setting(id, layer, "return_window_days", op, fields)
    };
    let base = floor(
        "f0",
        "L0",
        Op::Add,
        json!({"label": "Return window", "value": 14, "floor": "min"}),
    );
    // tightening (14 → 30) and a value-less patch pass; the floor is inherited
    let rows = job_x(vec![
        base.clone(),
        floor(
            "f1",
            "L1",
            Op::Patch,
            json!({"label": "Return window", "value": 30}),
        ),
        floor(
            "f2",
            "L3",
            Op::Patch,
            json!({"label": "Return window (job)"}),
        ),
    ]);
    let c = ok(run(&world(), &rows, &retail()));
    let w = c
        .catalogue
        .job_settings
        .iter()
        .find(|s| s.key == "return_window_days")
        .unwrap();
    assert_eq!(
        (w.fields["value"].clone(), w.fields["floor"].clone()),
        (json!(30), json!("min"))
    );

    // loosening (30 → 7 in the department), changing the direction or tombstoning: floor_loosened, every one
    for (bad, layer_name) in [
        (
            floor("x1", "L2", Op::Patch, json!({"label": "W", "value": 7})),
            CS,
        ),
        (
            floor(
                "x2",
                "L3",
                Op::Patch,
                json!({"label": "W", "value": 60, "floor": "max"}),
            ),
            "job/x",
        ),
        (floor("x3", "L3", Op::Tombstone, json!({})), "job/x"),
    ] {
        let id = bad.id.clone();
        let rows = job_x(vec![
            base.clone(),
            floor(
                "f1",
                "L1",
                Op::Patch,
                json!({"label": "Return window", "value": 30}),
            ),
            bad,
        ]);
        let errors = run(&world(), &rows, &retail()).expect_err("a looser floor");
        assert_eq!(errors.len(), 1, "{errors:#?}");
        let e = &errors[0];
        assert_eq!(
            (e.code.as_str(), e.key.as_str(), e.row_id.as_deref()),
            ("floor_loosened", "return_window_days", Some(id.as_str()))
        );
        assert!(e.message.starts_with(layer_name), "{}", e.message);
    }
    // a max floor may come down, never go up
    let max = floor(
        "m0",
        "L0",
        Op::Add,
        json!({"label": "Refund limit", "value": 100, "floor": "max"}),
    );
    let down = floor(
        "m1",
        "L3",
        Op::Patch,
        json!({"label": "Refund limit", "value": 50}),
    );
    assert!(run(&world(), &job_x(vec![max.clone(), down]), &retail()).is_ok());
    let up = floor(
        "m2",
        "L3",
        Op::Patch,
        json!({"label": "Refund limit", "value": 150}),
    );
    let errors = run(&world(), &job_x(vec![max, up]), &retail()).expect_err("raised");
    assert_eq!(errors[0].code, "floor_loosened");
}

// ---- bundles ---------------------------------------------------------------------------------------------------------

async fn template_pins(db: &PgPool, version: i32) -> Vec<TemplatePin> {
    let mut c = db.acquire().await.unwrap();
    templates::resolve_template(&mut c, CCA, version)
        .await
        .unwrap_or_else(|e| panic!("resolve {CCA}@{version}: {e:?}"))
        .pins
}

fn seed_selection() -> Selection {
    serde_json::from_value(json!({
        "profile": "retail_ecommerce",
        "industry": "retail",
        "departments": [CS],
    }))
    .unwrap()
}

/// A bundle without instructions serialises and hashes exactly as before (no `objects.instruction`); the offering's
/// v4 job-row preamble becomes the bundle's `identity` instruction, with the same `settings.preamble`.
#[tokio::test]
async fn bundles_without_instructions_hash_as_before() {
    let e = crate::common::env().await;
    let mut c = e.state.db.acquire().await.unwrap();
    // (a template without a preamble or instruction rows compiles to a bundle with no `objects.instruction`: the
    // pure compile in `instructions_fold_per_slot_with_append_and_tombstone` covers it)
    // v4: the job row's preamble is the job layer's identity
    let pins4 = template_pins(&e.state.db, 4).await;
    let v4 =
        TemplateCompiler::compile(e.state.compiler.as_ref(), &mut c, &seed_selection(), &pins4)
            .await
            .unwrap_or_else(|e| panic!("v4 compiles: {e:?}"));
    let set = v4.bundle.instructions();
    assert_eq!(slot_text(&set, "identity"), Some(SEED_PREAMBLE));
    assert_eq!(markers(&set, "identity"), [PREAMBLE_MARKER]);
    assert_eq!(v4.bundle.settings.preamble.as_deref(), Some(SEED_PREAMBLE));
    let back: Bundle = serde_json::from_value(serde_json::to_value(&v4.bundle).unwrap()).unwrap();
    assert_eq!(
        back.hash().unwrap(),
        v4.bundle_hash,
        "a stored bundle reads back"
    );
}

// ---- the runtime + the qualifier (R3) --------------------------------------------------------------------------------

fn actor() -> Actor {
    Actor::system("service:agent-api-kh-t4-test")
}

async fn write(db: &PgPool, layer: &str, kind: Kind, key: &str, op: Op, fields: Value) {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    rows::write_revision(
        &mut tx,
        &actor(),
        layer,
        kind,
        key,
        op,
        fields,
        "kh-t4-author",
    )
    .await
    .unwrap_or_else(|e| panic!("write {layer} {kind} {key}: {e:?}"));
    tx.commit().await.expect("commit the revision");
}

async fn cut(db: &PgPool, family: &str) -> i32 {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    let t = tags::tag_family(
        &mut tx,
        &actor(),
        family,
        "kh t4",
        "kh-t4-owner",
        &NoHooks,
        false,
    )
    .await
    .unwrap_or_else(|e| panic!("tag {family}: {e:?}"));
    tx.commit().await.expect("commit the tag");
    t.version
}

/// A job of a new tenant created from `CCA@version` through `POST /api/jobs`.
async fn job_from(e: &TestEnv, version: i32) -> ContractJob {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, &format!("KH T4 {t}")).await;
    let mut r = req(
        "POST",
        "/api/jobs",
        Some(&super_admin()),
        json!({"template": format!("{}@v{}", CCA, version)}),
    );
    r.headers_mut()
        .insert("x-active-tenant-id", t.to_string().parse().unwrap());
    let (s, b) = send(&e.app, r).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let key = b["key"].as_str().expect("JobSummary.key").to_owned();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(t)
        .bind(&key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    design_contract::contract_job(t, id, &key)
}

async fn draft_design(e: &TestEnv, f: &ContractJob) -> RuntimeDesign {
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let d = RuntimeDesign::load(&mut tx, &e.state.design, &f.job_ref(), &DesignOf::Draft)
        .await
        .unwrap_or_else(|e| panic!("load the draft design: {e:?}"));
    tx.commit().await.unwrap();
    d
}

fn qualifier_system(d: &RuntimeDesign) -> String {
    // KH T5: the qualifier's system message is the assembled prompt (the identity is the job layer's section)
    let prompt = agent_api::runtime::prompt::assemble(&d.into(), None, None);
    let b = qualify::body("m", "hi", None, &d.spaces, &d.settings, &prompt);
    b["messages"][0]["content"]
        .as_str()
        .expect("one system message")
        .to_owned()
}

#[tokio::test]
async fn the_job_identity_instruction_reaches_the_qualifier() {
    let e = crate::common::env().await;
    let db = &e.state.db;

    // v4: the seed's preamble, now the job layer's identity, still reaches the qualifier
    let f4 = job_from(&e, 4).await;
    let d4 = draft_design(&e, &f4).await;
    assert!(d4.template_errors.is_empty(), "{:?}", d4.template_errors);
    assert_eq!(slot_text(&d4.instructions, "identity"), Some(SEED_PREAMBLE));
    assert_eq!(d4.settings.get("preamble"), Some(&json!(SEED_PREAMBLE)));
    assert!(
        qualifier_system(&d4).contains(&format!("{PREAMBLE_MARKER}\n{SEED_PREAMBLE}")),
        "{}",
        qualifier_system(&d4)
    );

    // v5: the job layer moves its preamble into an identity instruction row (which wins over the job row's
    // preamble) and adds a tone
    let identity = "You are the care agent of Demo Retail.";
    // the instructions the job inherits (authored here; the job's tone replaces the industry's)
    write(
        db,
        "base",
        Kind::Instruction,
        "policy",
        Op::Add,
        json!({"text": "Follow company policy."}),
    )
    .await;
    cut(db, "base").await;
    write(
        db,
        "industry/retail",
        Kind::Instruction,
        "tone",
        Op::Add,
        json!({"text": "Formal and precise.", "language": "en"}),
    )
    .await;
    write(
        db,
        "industry/retail",
        Kind::Instruction,
        "safety",
        Op::Add,
        json!({"text": "Never share another customer's data."}),
    )
    .await;
    cut(db, "industry/retail").await;
    write(
        db,
        CCA,
        Kind::Instruction,
        "identity",
        Op::Add,
        json!({"text": identity}),
    )
    .await;
    write(
        db,
        CCA,
        Kind::Instruction,
        "tone",
        Op::Add,
        json!({"text": "Warm and brief.", "language": "en"}),
    )
    .await;
    let v5 = cut(db, CCA).await;
    let f5 = job_from(&e, v5).await;
    let d5 = draft_design(&e, &f5).await;
    assert!(d5.template_errors.is_empty(), "{:?}", d5.template_errors);
    // base gives policy, retail gives safety (and a tone); the job's tone replaces retail's
    let keys: Vec<&str> = d5.instructions.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, ["identity", "policy", "tone", "safety"]);
    assert_eq!(slot_text(&d5.instructions, "tone"), Some("Warm and brief."));
    assert_eq!(slot_text(&d5.instructions, "identity"), Some(identity));
    assert_eq!(
        markers(&d5.instructions, "identity"),
        [format!("[layer {CCA}@v{v5}]")]
    );
    assert_eq!(d5.settings.get("preamble"), Some(&json!(identity)));
    let system = qualifier_system(&d5);
    assert!(
        system.contains(&format!("[layer {CCA}@v{v5}]\n{identity}")),
        "{system}"
    );
    assert!(!system.contains(SEED_PREAMBLE), "{system}");

    // the release snapshot carries the same set: cut + apply, then the Live design reads the bundle's
    design_contract::publishable(&e, &f5).await;
    design_contract::publish(&e, &f5).await;
    let mut tx = f5.job.begin(db).await.unwrap();
    let live = RuntimeDesign::load(&mut tx, &e.state.design, &f5.job_ref(), &DesignOf::Live)
        .await
        .expect("the live design");
    tx.commit().await.unwrap();
    assert!(
        matches!(
            &live.case,
            agent_api::design::CaseDesign::Live {
                release_id: Some(_)
            }
        ),
        "{:?}",
        live.case
    );
    assert_eq!(live.instructions, d5.instructions);
    assert_eq!(live.settings.get("preamble"), Some(&json!(identity)));
}

// ---- tenant refinements ----------------------------------------------------------------------------------------------

fn uri(f: &ContractJob, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", f.job.job_key)
}

#[tokio::test]
async fn tenants_refine_tone_escalation_and_format_only() {
    let e = crate::common::env().await;
    let f = design_contract::new_job(&e).await;
    let tok = design_contract::release_manager(&e, &f).await;

    // the refinable object lists the release's folded slots as its base
    let (s, b) = send(&e.app, get(&uri(&f, "instruction"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let base_keys: Vec<&str> = b["base"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|i| i["key"].as_str())
        .collect();
    assert_eq!(base_keys, ["identity"], "{b}");

    for slot in ["identity", "policy", "safety", "persona"] {
        let (s, b) = send(
            &e.app,
            req(
                "POST",
                &uri(&f, "refinements/hide"),
                Some(&tok),
                json!({"object": "instruction", "key": slot}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{slot}: {b}");
        assert_eq!(
            b["error"]["code"], "instruction_not_refinable",
            "{slot}: {b}"
        );
        let (s, b) = send(
            &e.app,
            req(
                "PUT",
                &uri(&f, &format!("instructions/{slot}")),
                Some(&tok),
                json!({"text": "Override."}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{slot}: {b}");
        assert_eq!(
            b["error"]["code"], "instruction_not_refinable",
            "{slot}: {b}"
        );
    }
    let refused: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM job_overlay_row WHERE job_id = $1 AND object = 'instruction'",
    )
    .bind(f.job.job_id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(refused, 0, "nothing written");

    // controller ruling 3: the preamble is no tenant setting any more (the identity instruction), at the route and in
    // the table (0133 narrowed job_setting_override_key_check)
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &uri(&f, "settings"),
            Some(&tok),
            json!({"preamble": "Mine."}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "unknown_setting", "{b}");
    let (s, b) = send(&e.app, get(&uri(&f, "settings"), Some(&tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(b["base"].get("preamble").is_none(), "{b}");
    let err = sqlx::query(
        "INSERT INTO job_setting_override (tenant_id, job_id, key, value, base_hash, updated_by) \
         VALUES ($1, $2, 'preamble', 'Mine.', NULL, 'test')",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .execute(&e.owner)
    .await
    .expect_err("0133 refuses a preamble override");
    assert_eq!(
        err.as_database_error().and_then(|d| d.code()).as_deref(),
        Some("23514"),
        "{err}"
    );

    // a blank text is not an instruction
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &uri(&f, "instructions/tone"),
            Some(&tok),
            json!({"text": "  "}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_body", "{b}");

    // tone: the release has none, so the tenant's text is an extension; a second write updates it
    for text in ["Warm.", "Warm and brief."] {
        let (s, b) = send(
            &e.app,
            req(
                "PUT",
                &uri(&f, "instructions/tone"),
                Some(&tok),
                json!({"text": text, "language": "en"}),
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(b["draft_rev"].as_i64().is_some(), "{b}");
    }
    let (kind, item): (String, Value) = sqlx::query_as(
        "SELECT kind, item FROM job_overlay_row \
          WHERE job_id = $1 AND object = 'instruction' AND key = 'tone'",
    )
    .bind(f.job.job_id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(kind, "extension");
    assert_eq!(
        item,
        json!({"key": "tone", "text": "Warm and brief.", "language": "en"})
    );
    let ledger: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM draft_ledger WHERE job_id = $1 AND key = 'overlay:instruction:tone'",
    )
    .bind(f.job.job_id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(ledger, 1, "one tracked draft change");
}

/// KH-F1 (B4 wording): the qualifier's `[task qualify]` shows each context space by its `description`, never its
/// label — the two spaces the qualifier confused (a late parcel vs. a plain "where is my order") are told apart by
/// their descriptions. The platform holds no spaces of its own now, so the test authors them on the department
/// (every space it adds carries a non-empty description), tags the department and the job layer, and reads the task
/// of a job created from the new tag: every space of the compiled design is described and shown by its description.
#[tokio::test]
async fn the_qualifier_task_describes_the_retail_context_spaces() {
    let e = crate::common::env().await;
    let db = &e.state.db;
    let spaces = [
        (
            "retail:order-support:delivery_problem",
            "Delivery problem",
            "The parcel is late, lost or damaged, or tracking says delivered but the customer did not receive it.",
        ),
        (
            "retail:order-support:order_status",
            "Order status",
            "The customer asks where an order is or when it arrives, without reporting a problem.",
        ),
    ];
    for (key, label, description) in spaces {
        write(
            db,
            CS,
            Kind::ContextSpace,
            key,
            Op::Add,
            json!({"label": label, "description": description, "area": "order-support"}),
        )
        .await;
    }
    cut(db, CS).await;
    // the job layer needs one new current row to be tagged again
    write(
        db,
        CCA,
        Kind::JobSetting,
        "kh_t4_spaces",
        Op::Add,
        json!({"label": "Setting 5"}),
    )
    .await;
    let v = cut(db, CCA).await;
    let adds = spaces.len();
    let f = job_from(&e, v).await;
    let d = draft_design(&e, &f).await;
    assert!(d.template_errors.is_empty(), "{:?}", d.template_errors);
    let system = qualifier_system(&d);
    let task = &system[system.find("[task qualify]\n").expect("the task section")..];
    assert!(d.spaces.by_key("retail:faq:gift_cards").is_none());
    assert!(d.spaces.spaces().len() >= adds, "{:?}", d.spaces);
    for space in d.spaces.spaces() {
        let text = space.description.split_whitespace().collect::<Vec<_>>();
        assert!(!text.is_empty(), "{} has no description", space.key);
        let line = format!("\n- {}: {}\n", space.key, text.join(" "));
        // the last space's line may end the task
        assert!(
            format!("{task}\n").contains(&line),
            "{line:?} missing in {task}"
        );
    }
    for line in [
        "\n- retail:order-support:delivery_problem: The parcel is late, lost or damaged, or tracking says delivered but \
         the customer did not receive it.\n",
        "\n- retail:order-support:order_status: The customer asks where an order is or when it arrives, without \
         reporting a problem.\n",
    ] {
        assert!(task.contains(line), "{line:?} missing in {task}");
    }
    for label in ["Delivery problem\n", "Order status\n"] {
        assert!(
            !task.contains(label),
            "a described space shows no label: {task}"
        );
    }
}
