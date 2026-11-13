//! KH T9 (D111, D112, D115 (7)): region as an add-on sub-layer of each platform layer — the key grammar
//! `…/region/<r>`, the fold position right after the parent (each department's region right after that department),
//! selection by `selection.region` (absent = no sub-layer), the parent's tag family (one tag pins both), packs and
//! instructions on a sub-layer behaving like the parent's, the schema (0139) and 422 `unknown_region`.
use agent_api::audit::Actor;
use agent_api::error::ApiError;
use agent_api::platform::db::{rows, tags};
use agent_api::platform::hooks::NoHooks;
use agent_api::platform::templates::{self, TemplatePin};
use agent_api::platform_db::begin_platform;
use agent_model::compile::{
    self, CompileError, CompileInput, Compiled, DepartmentRef, FoldPosition, LayerMeta, Row,
    Selection, code, fold_knowledge, fold_order,
};
use agent_model::kinds::instruction::slot_text;
use agent_model::kinds::{Kind, Op};
use agent_model::layer::LayerKey;
use axum::http::StatusCode;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::common::design_contract;
use crate::common::{insert_user, json as req, person, send};

const A: &str = "industry_dept/retail/kh_a";
const B: &str = "industry_dept/retail/kh_b";
const CS: &str = "industry_dept/retail/customer_service";
const CCA: &str = "job/customer_care_agent";

// ---- a small pure world ---------------------------------------------------------------------------------------------

fn layer(id: &str, key: &str, extends: Option<&str>) -> LayerMeta {
    LayerMeta {
        id: id.to_owned(),
        key: LayerKey::parse_label(key).unwrap_or_else(|e| panic!("{key}: {e}")),
        extends: extends.map(str::to_owned),
        tag: None,
        tagged_at: None,
    }
}

/// base, industry/retail and two departments (archetype `kh_arch`), each with an `nl` region sub-layer, plus a `de`
/// sub-layer of the industry only, and job/care. Region sub-layers are listed BEFORE their parents on purpose: the
/// fold places them by parent, not by list order.
fn layers() -> Vec<LayerMeta> {
    vec![
        layer("L-base-nl", "base/region/nl", None),
        layer("L-base", "base", None),
        layer("L-ret-nl", "industry/retail/region/nl", None),
        layer("L-ret-de", "industry/retail/region/de", None),
        layer("L-ret", "industry/retail", None),
        layer("L-a-nl", &format!("{A}/region/nl"), None),
        layer("L-a", A, Some("kh_arch")),
        layer("L-b", B, Some("kh_arch")),
        layer("L-b-nl", &format!("{B}/region/nl"), None),
        layer("L-job", "job/care", None),
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

fn entity(id: &str, layer_id: &str, op: Op, label: &str) -> Row {
    row(
        id,
        layer_id,
        Kind::Entity,
        "KhThing",
        op,
        json!({"label": label, "synonyms": ["kh"]}),
    )
}

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
            json!({"label": "KH archetype", "knowledge_kinds": ["kh.shared"]}),
        ),
    ]
}

fn sel(departments: &[&str], region: Option<&str>) -> Selection {
    serde_json::from_value(json!({
        "industry": "retail",
        "departments": departments,
        "region": region,
    }))
    .unwrap()
}

fn compile_world(
    layers: &[LayerMeta],
    rows: &[Row],
    sel: &Selection,
) -> Result<Compiled, Vec<CompileError>> {
    let departments = DepartmentRef::from_layers(layers);
    let input = CompileInput {
        layers,
        rows,
        rules: &[],
        profiles: &[],
        departments: &departments,
    };
    compile::compile(&input, sel, Some("care"))
}

fn ok(r: Result<Compiled, Vec<CompileError>>) -> Compiled {
    r.unwrap_or_else(|e| panic!("unexpected compile errors: {e:#?}"))
}

fn labels(ls: &[LayerMeta], s: &Selection) -> Vec<(FoldPosition, String, Option<usize>)> {
    fold_order(ls, s, Some("care"), &[])
        .iter()
        .map(|s| (s.position, s.label(), s.department))
        .collect()
}

// ---- the fold position ---------------------------------------------------------------------------------------------

#[test]
fn each_region_sublayer_folds_right_after_its_parent_and_only_with_its_region() {
    use FoldPosition as P;
    let ls = layers();
    let a_nl = format!("{A}/region/nl");
    let b_nl = format!("{B}/region/nl");
    // D115 (7): two departments each with a region sub-layer fold A, A/region, B, B/region
    assert_eq!(
        labels(&ls, &sel(&[A, B], Some("nl"))),
        vec![
            (P::Base, "base".to_owned(), None),
            (P::BaseRegion, "base/region/nl".to_owned(), None),
            (P::Industry, "industry/retail".to_owned(), None),
            (
                P::IndustryRegion,
                "industry/retail/region/nl".to_owned(),
                None
            ),
            (
                P::Archetype,
                "department_archetype/kh_arch".to_owned(),
                Some(0)
            ),
            (P::Department, A.to_owned(), Some(0)),
            (P::DepartmentRegion, a_nl.clone(), Some(0)),
            (P::Department, B.to_owned(), Some(1)),
            (P::DepartmentRegion, b_nl.clone(), Some(1)),
            (P::Job, "job/care".to_owned(), None),
        ]
    );
    // the declared order decides the section order, each region stays with its department
    let reversed: Vec<String> = labels(&ls, &sel(&[B, A], Some("nl")))
        .into_iter()
        .map(|(_, l, _)| l)
        .collect();
    assert_eq!(
        reversed[5..9],
        [B.to_owned(), b_nl.clone(), A.to_owned(), a_nl.clone()]
    );
    // region absent → no sub-layer applies
    let none: Vec<String> = labels(&ls, &sel(&[A, B], None))
        .into_iter()
        .map(|(_, l, _)| l)
        .collect();
    assert_eq!(
        none,
        [
            "base",
            "industry/retail",
            "department_archetype/kh_arch",
            A,
            B,
            "job/care"
        ]
    );
    // another region selects only its own sub-layers (de: the industry's only)
    let de: Vec<(FoldPosition, String)> = labels(&ls, &sel(&[A, B], Some("de")))
        .into_iter()
        .filter(|(p, _, _)| matches!(p, P::BaseRegion | P::IndustryRegion | P::DepartmentRegion))
        .map(|(p, l, _)| (p, l))
        .collect();
    assert_eq!(
        de,
        [(P::IndustryRegion, "industry/retail/region/de".to_owned())]
    );
    // a region sub-layer is never a department of its own
    assert_eq!(DepartmentRef::from_layers(&ls).len(), 2);
}

#[test]
fn an_industry_region_row_overrides_the_industry_and_the_department_overrides_it() {
    let ls = layers();
    let mut rows = base_rows();
    rows.extend([
        entity("e-ret", "L-ret", Op::Add, "Industry"),
        entity("e-ret-nl", "L-ret-nl", Op::Patch, "Industry NL"),
    ]);
    // the sub-layer overrides its parent
    let c = ok(compile_world(&ls, &rows, &sel(&[A], Some("nl"))));
    assert_eq!(c.effective.entities["KhThing"]["label"], "Industry NL");
    assert!(
        c.effective
            .layers
            .iter()
            .any(|l| l == "industry/retail/region/nl"),
        "{:?}",
        c.effective.layers
    );
    // region absent → the sub-layer is ignored (and so is another region's)
    let c = ok(compile_world(&ls, &rows, &sel(&[A], None)));
    assert_eq!(c.effective.entities["KhThing"]["label"], "Industry");
    assert!(!c.effective.layers.iter().any(|l| l.contains("/region/")));
    let c = ok(compile_world(&ls, &rows, &sel(&[A], Some("de"))));
    assert_eq!(c.effective.entities["KhThing"]["label"], "Industry");
    // the department (later in the fold) overrides the industry's region sub-layer
    rows.push(entity("e-a", "L-a", Op::Patch, "Department A"));
    let c = ok(compile_world(&ls, &rows, &sel(&[A], Some("nl"))));
    assert_eq!(c.effective.entities["KhThing"]["label"], "Department A");
    // and the department's own region sub-layer overrides the department
    rows.push(entity("e-a-nl", "L-a-nl", Op::Patch, "Department A NL"));
    let c = ok(compile_world(&ls, &rows, &sel(&[A], Some("nl"))));
    assert_eq!(c.effective.entities["KhThing"]["label"], "Department A NL");
    // the base region sub-layer sits between base and industry
    let mut base = base_rows();
    base.extend([
        entity("e-base", "L-base", Op::Add, "Base"),
        entity("e-base-nl", "L-base-nl", Op::Patch, "Base NL"),
    ]);
    let c = ok(compile_world(&ls, &base, &sel(&[A], Some("nl"))));
    assert_eq!(c.effective.entities["KhThing"]["label"], "Base NL");
    base.push(entity("e-ret", "L-ret", Op::Patch, "Industry"));
    let c = ok(compile_world(&ls, &base, &sel(&[A], Some("nl"))));
    assert_eq!(c.effective.entities["KhThing"]["label"], "Industry");
}

#[test]
fn a_department_region_sublayer_is_part_of_its_department_for_the_sibling_rule() {
    let ls = layers();
    let mut rows = base_rows();
    // A's region sets the key differently from B: the sibling rule names the region sub-layer
    rows.extend([
        entity("e-a-nl", "L-a-nl", Op::Add, "A NL"),
        entity("e-b", "L-b", Op::Add, "B"),
    ]);
    let errors = compile_world(&ls, &rows, &sel(&[A, B], Some("nl"))).expect_err("siblings");
    let codes: Vec<&str> = errors.iter().map(|e| e.code.as_str()).collect();
    assert_eq!(codes, [code::DEPARTMENT_CONFLICT], "{errors:#?}");
    assert_eq!(errors[0].layers, [format!("{A}/region/nl"), B.to_owned()]);
    // without the region there is nothing to disagree on
    let c = ok(compile_world(&ls, &rows, &sel(&[A, B], None)));
    assert_eq!(c.effective.entities["KhThing"]["label"], "B");
    // within one department its region sub-layer simply wins over the department (no conflict)
    let mut own = base_rows();
    own.extend([
        entity("e-a", "L-a", Op::Add, "A"),
        entity("e-a-nl", "L-a-nl", Op::Patch, "A NL"),
    ]);
    let c = ok(compile_world(&ls, &own, &sel(&[A, B], Some("nl"))));
    assert_eq!(c.effective.entities["KhThing"]["label"], "A NL");
}

#[test]
fn packs_kinds_and_instructions_on_region_sublayers_behave_like_their_parents() {
    let ls = layers();
    let mut rows = base_rows();
    rows.extend([
        row(
            "k-ret-nl",
            "L-ret-nl",
            Kind::StageKnowledgeKind,
            "support|kh.nl_law",
            Op::Add,
            json!({"label": "NL law"}),
        ),
        row(
            "k-a-nl",
            "L-a-nl",
            Kind::StageKnowledgeKind,
            "support|kh.a_nl",
            Op::Add,
            json!({"label": "A NL"}),
        ),
        row(
            "p-a-nl",
            "L-a-nl",
            Kind::KnowledgePack,
            "support|kh.a_nl|retail/kh_nl",
            Op::Add,
            json!({"version": 1}),
        ),
        row(
            "i-ret",
            "L-ret",
            Kind::Instruction,
            "tone",
            Op::Add,
            json!({"text": "Retail tone."}),
        ),
        row(
            "i-ret-nl",
            "L-ret-nl",
            Kind::Instruction,
            "tone",
            Op::Add,
            json!({"text": "Dutch tone."}),
        ),
    ]);
    let (k, errors) = fold_knowledge(&ls, &rows, &sel(&[A, B], Some("nl")), Some("care"));
    assert!(errors.is_empty(), "{errors:#?}");
    let kind = |key: &str| k.kinds.iter().find(|x| x.key() == key);
    // an industry region kind is inherited by every department, like the industry's
    let nl_law = kind("support|kh.nl_law").expect("the industry region kind");
    assert_eq!(nl_law.layer, "industry/retail/region/nl");
    assert!(nl_law.departments.is_empty());
    // a department region kind and pack belong to that department
    let a_nl = kind("support|kh.a_nl").expect("the department region kind");
    assert_eq!(a_nl.departments, [A]);
    let of =
        |d: &str| -> Vec<String> { k.kinds_of_department(d).iter().map(|x| x.key()).collect() };
    assert!(of(A).contains(&"support|kh.a_nl".to_owned()));
    assert!(!of(B).contains(&"support|kh.a_nl".to_owned()));
    assert!(of(B).contains(&"support|kh.nl_law".to_owned()));
    let pack = k
        .packs
        .iter()
        .find(|p| p.pack_key == "retail/kh_nl")
        .expect("the region pack");
    assert_eq!(pack.layer, format!("{A}/region/nl"));
    assert_eq!(pack.departments, [A]);
    // region absent: none of it
    let (k, _) = fold_knowledge(&ls, &rows, &sel(&[A, B], None), Some("care"));
    assert!(k.kinds.iter().all(|x| !x.layer.contains("/region/")));
    assert!(k.packs.is_empty());
    // instructions: the region sub-layer replaces the industry's slot text, with its own marker
    let c = ok(compile_world(&ls, &rows, &sel(&[A], Some("nl"))));
    assert_eq!(
        slot_text(&c.catalogue.instructions, "tone"),
        Some("Dutch tone.")
    );
    let tone = c
        .catalogue
        .instructions
        .iter()
        .find(|i| i.key == "tone")
        .unwrap();
    assert_eq!(
        tone.sections
            .iter()
            .map(|s| s.layer.as_str())
            .collect::<Vec<_>>(),
        ["industry/retail/region/nl"]
    );
    let c = ok(compile_world(&ls, &rows, &sel(&[A], None)));
    assert_eq!(
        slot_text(&c.catalogue.instructions, "tone"),
        Some("Retail tone.")
    );
}

// ---- the database ---------------------------------------------------------------------------------------------------

fn actor() -> Actor {
    Actor::system("service:agent-api-kh-t9-test")
}

async fn insert_region_layer(
    db: &PgPool,
    key: &str,
    scope: &str,
    industry: Option<&str>,
    department: Option<&str>,
    region: &str,
    family: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO layer (key, scope, industry, department, region, family) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(key)
    .bind(scope)
    .bind(industry)
    .bind(department)
    .bind(region)
    .bind(family)
    .execute(db)
    .await
    .map(|_| ())
}

fn constraint(r: Result<(), sqlx::Error>) -> String {
    let e = r.expect_err("the schema refuses it");
    e.as_database_error()
        .and_then(|d| d.constraint())
        .unwrap_or_else(|| panic!("not a constraint violation: {e}"))
        .to_owned()
}

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
        "kh-t9-author",
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
        "kh t9",
        "kh-t9-owner",
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

fn retail(region: Option<&str>) -> Selection {
    serde_json::from_value(json!({
        "profile": "retail_ecommerce",
        "industry": "retail",
        "departments": [CS],
        "region": region,
    }))
    .unwrap()
}

#[tokio::test]
async fn the_schema_takes_region_sublayers_in_their_parents_family_only() {
    let e = crate::common::env().await;
    let db = &e.owner;
    insert_region_layer(db, "base/region/nl", "base", None, None, "nl", "base")
        .await
        .expect("base/region/nl");
    insert_region_layer(
        db,
        "industry/retail/region/nl",
        "industry",
        Some("retail"),
        None,
        "nl",
        "industry/retail",
    )
    .await
    .expect("industry/retail/region/nl");
    insert_region_layer(
        db,
        &format!("{CS}/region/nl"),
        "industry_dept",
        Some("retail"),
        Some("customer_service"),
        "nl",
        CS,
    )
    .await
    .expect("the department's region");
    // a sub-layer is not a head: its family is its parent
    let r = insert_region_layer(
        db,
        "base/region/de",
        "base",
        None,
        None,
        "de",
        "base/region/de",
    )
    .await;
    let c = constraint(r);
    assert!(
        c == "layer_family_head" || c == "layer_region_family",
        "{c}"
    );
    let r = insert_region_layer(
        db,
        "industry/retail/region/de",
        "industry",
        Some("retail"),
        None,
        "de",
        "base",
    )
    .await;
    assert_eq!(constraint(r), "layer_region_family");
    // the key is the label (`…/region/<r>` after the industry / department)
    let r = insert_region_layer(
        db,
        "industry/retail/de",
        "industry",
        Some("retail"),
        None,
        "de",
        "industry/retail",
    )
    .await;
    assert_eq!(constraint(r), "layer_key_label");
    // job and profile layers carry no region (the key spelt as the label CHECK would, so only the scope refuses)
    let r = sqlx::query(
        "INSERT INTO layer (key, scope, job, region, family) \
         VALUES ('job/region/nl/customer_care_agent', 'job', 'customer_care_agent', 'nl', 'job')",
    )
    .execute(db)
    .await
    .map(|_| ());
    assert_eq!(constraint(r), "layer_scope_keys");
    // no `/` or `=` in a region
    let r = insert_region_layer(db, "base/region/n=l", "base", None, None, "n=l", "base").await;
    assert_eq!(constraint(r), "layer_region_check");
    // the template metadata stays on the parent
    let r = sqlx::query(
        "INSERT INTO layer (key, scope, industry, department, region, family, extends) \
         VALUES ($1, 'industry_dept', 'retail', 'customer_service', 'de', $2, 'customer_service')",
    )
    .bind(format!("{CS}/region/de"))
    .bind(CS)
    .execute(db)
    .await
    .map(|_| ());
    assert_eq!(constraint(r), "layer_region_metadata");
    // one layer per key
    let r = insert_region_layer(db, "base/region/nl", "base", None, None, "nl", "base").await;
    assert!(r.is_err());
    // the store reads it back with its region, as a member of the parent's family
    let mut c = e.state.db.acquire().await.unwrap();
    let family = agent_api::platform::db::layers::family_layers(&mut c, "industry/retail")
        .await
        .unwrap();
    let sub = family
        .iter()
        .find(|l| l.key == "industry/retail/region/nl")
        .expect("in the parent's family");
    assert_eq!(sub.region.as_deref(), Some("nl"));
    assert!(!sub.is_head());
}

#[tokio::test]
async fn a_tag_of_the_parent_pins_its_region_sublayer_and_packs_and_instructions_compile_like_the_parents()
 {
    let e = crate::common::env().await;
    let db = &e.state.db;
    let cs_nl = format!("{CS}/region/nl");
    insert_region_layer(
        &e.owner,
        "industry/retail/region/nl",
        "industry",
        Some("retail"),
        None,
        "nl",
        "industry/retail",
    )
    .await
    .expect("industry/retail/region/nl");
    insert_region_layer(
        &e.owner,
        &cs_nl,
        "industry_dept",
        Some("retail"),
        Some("customer_service"),
        "nl",
        CS,
    )
    .await
    .expect("the department's region");

    // rows on the sub-layers, written like the parent's (same kinds, same authoring checks)
    must_write(
        db,
        "industry/retail/region/nl",
        Kind::StageKnowledgeKind,
        "support|kh.nl_law",
        json!({"label": "NL consumer law", "required": true}),
    )
    .await;
    must_write(
        db,
        "industry/retail/region/nl",
        Kind::Instruction,
        "tone",
        json!({"text": "Spreek de klant aan met u."}),
    )
    .await;
    // a department region pack may name a kind its industry's region sub-layer declares …
    must_write(
        db,
        &cs_nl,
        Kind::KnowledgePack,
        "support|kh.nl_law|retail/kh_nl_law",
        json!({"version": 1}),
    )
    .await;
    // … and one its parent department declares; an undeclared kind is the parent's 422
    let refused = write(
        db,
        &cs_nl,
        Kind::KnowledgePack,
        "support|kh.nothing|retail/kh_nothing",
        Op::Add,
        json!({"version": 1}),
    )
    .await
    .expect_err("undeclared");
    assert_eq!(
        refused.status_and_code(),
        (StatusCode::UNPROCESSABLE_ENTITY, "pack_kind_undeclared")
    );
    // a sub-layer cannot be tagged on its own: its family is the parent
    let mut tx = begin_platform(db).await.unwrap();
    let own = tags::tag_family(
        &mut tx,
        &actor(),
        "industry/retail/region/nl",
        "kh t9",
        "kh-t9-owner",
        &NoHooks,
        false,
    )
    .await
    .expect_err("not a family");
    drop(tx);
    assert_eq!(
        own.status_and_code(),
        (StatusCode::NOT_FOUND, "layer_not_found")
    );

    // one tag of the parent pins parent and sub-layer together
    let v_ret = tag(db, "industry/retail").await;
    let v_cs = tag(db, CS).await;
    let mut c = db.acquire().await.unwrap();
    let members = tags::tag_members(&mut c, "industry/retail", v_ret)
        .await
        .unwrap();
    assert!(
        members
            .iter()
            .any(|m| m.layer_key == "industry/retail/region/nl" && m.key == "support|kh.nl_law"),
        "{members:#?}"
    );
    // the instruction row on the sub-layer (its id: the compile's provenance names it)
    let instruction = members
        .iter()
        .find(|m| m.layer_key == "industry/retail/region/nl" && m.kind == "instruction")
        .expect("the region instruction is a member")
        .id
        .to_string();
    let members = tags::tag_members(&mut c, CS, v_cs).await.unwrap();
    assert!(members.iter().any(|m| m.layer_key == cs_nl));
    drop(c);
    must_write(
        db,
        CCA,
        Kind::StageKnowledgeKind,
        "support|kh.t9_job",
        json!({"label": "T9 job"}),
    )
    .await;
    let v = tag(db, CCA).await;
    let pins = pins_of(db, CCA, v).await;
    assert!(
        pins.iter()
            .any(|p| p.layer == "industry/retail" && p.version == v_ret),
        "{pins:?}"
    );
    assert!(
        !pins.iter().any(|p| p.layer.contains("/region/")),
        "a sub-layer is never a pin of its own: {pins:?}"
    );
    let job = Some("customer_care_agent");
    // a sub-layer created after its parent's pinned tag is not in that tag: the pinned version compiles without it
    insert_region_layer(&e.owner, "base/region/nl", "base", None, None, "nl", "base")
        .await
        .expect("base/region/nl");
    must_write(
        db,
        "base/region/nl",
        Kind::StageKnowledgeKind,
        "support|kh.base_nl",
        json!({"label": "Base NL"}),
    )
    .await;

    // region nl: the sub-layers fold, each right after its parent
    let mut c = db.acquire().await.unwrap();
    let compiled = templates::compile_pinned(&mut c, &pins, &retail(Some("nl")), job)
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("{e:#?}"));
    let order = &compiled.effective.layers;
    let at = |k: &str| order.iter().position(|l| l == k);
    assert_eq!(
        at("industry/retail/region/nl"),
        at("industry/retail").map(|n| n + 1),
        "{order:?}"
    );
    assert_eq!(at(&cs_nl), at(CS).map(|n| n + 1), "{order:?}");
    assert_eq!(
        at("base/region/nl"),
        None,
        "not in base's pinned tag: {order:?}"
    );
    let k = &compiled.knowledge;
    let nl_law = k
        .kinds
        .iter()
        .find(|x| x.key() == "support|kh.nl_law")
        .expect("the region kind");
    assert_eq!(nl_law.layer, "industry/retail/region/nl");
    assert!(nl_law.required);
    let pack = k
        .packs
        .iter()
        .find(|p| p.pack_key == "retail/kh_nl_law")
        .expect("the region pack");
    assert_eq!(pack.layer, cs_nl);
    assert_eq!(pack.departments, [CS]);
    // the instruction folded at the sub-layer's position (a later layer of the seed may still replace the slot)
    assert!(
        compiled.provenance_rows.contains(&instruction),
        "{:?}",
        compiled.provenance_rows
    );

    // region absent: compiled as before, the sub-layers ignored
    let plain = templates::compile_pinned(&mut c, &pins, &retail(None), job)
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("{e:#?}"));
    assert!(
        !plain
            .effective
            .layers
            .iter()
            .any(|l| l.contains("/region/")),
        "{:?}",
        plain.effective.layers
    );
    assert!(
        !plain
            .knowledge
            .kinds
            .iter()
            .any(|x| x.key() == "support|kh.nl_law")
    );
    assert!(
        !plain
            .knowledge
            .packs
            .iter()
            .any(|p| p.pack_key == "retail/kh_nl_law")
    );
    assert!(!plain.provenance_rows.contains(&instruction));
}

const ADMIN: &str = "u-kh-t9-admin";

#[tokio::test]
async fn an_unknown_region_is_422_and_a_platform_region_saves() {
    let e = crate::common::env().await;
    let f = design_contract::new_job(&e).await;
    insert_user(&e.owner, ADMIN, f.tenant, "tenant_admin").await;
    let tok = person(ADMIN, &[]);
    let path = format!("/api/jobs/{}/target", f.job.job_key);
    let selection = |region: &str| {
        json!({
            "profile": "retail_ecommerce", "industry": "retail", "departments": [CS],
            "areas": ["order-support"], "dimensions": {"channel": ["chat"]}, "region": region,
        })
    };
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &path,
            Some(&tok),
            json!({"selection": selection("xx"), "expected_updated_at": null}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "unknown_region", "{b}");
    assert_eq!(b["error"]["errors"][0]["path"], "selection.region", "{b}");
    assert!(
        b["error"]["message"].as_str().unwrap().contains("nl"),
        "the message lists the platform's regions: {b}"
    );
    // the platform's vocabulary (the `region` dimension values) is what a selection may name
    let mut c = e.state.db.acquire().await.unwrap();
    let vocab = templates::region_vocabulary(&mut c).await.unwrap();
    drop(c);
    assert!(vocab.iter().any(|r| r == "nl"), "{vocab:?}");
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &path,
            Some(&tok),
            json!({"selection": selection("nl"), "expected_updated_at": null}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["selection"]["region"], "nl", "{b}");
    // a region the curator adds to the vocabulary is selectable (the options follow the vocabulary)
    sqlx::query(
        "INSERT INTO dimension_value (dimension, key, label, added_by) VALUES ('region', 'kh_t9', 'KH T9', 'kh-t9')",
    )
    .execute(&e.owner)
    .await
    .unwrap();
    let at = b["selection_updated_at"].clone();
    let (s, b) = send(
        &e.app,
        req(
            "PUT",
            &path,
            Some(&tok),
            json!({"selection": selection("kh_t9"), "expected_updated_at": at}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}

// ---- fix round 1 ----------------------------------------------------------------------------------------------------

#[tokio::test]
async fn platform_wide_definitions_are_refused_on_region_sublayers() {
    let e = crate::common::env().await;
    let db = &e.state.db;
    insert_region_layer(&e.owner, "base/region/nl", "base", None, None, "nl", "base")
        .await
        .expect("base/region/nl");
    insert_region_layer(
        &e.owner,
        "industry/retail/region/nl",
        "industry",
        Some("retail"),
        None,
        "nl",
        "industry/retail",
    )
    .await
    .expect("industry/retail/region/nl");
    for (layer, kind, key) in [
        ("base/region/nl", Kind::AxisRule, "kh_t9_rule"),
        ("base/region/nl", Kind::Profile, "kh_t9_profile"),
        ("industry/retail/region/nl", Kind::Profile, "kh_t9_profile"),
        ("base/region/nl", Kind::DepartmentArchetype, "kh_t9_arch"),
    ] {
        let refused = write(db, layer, kind, key, Op::Add, json!({"label": "x"}))
            .await
            .expect_err("refused on a region sub-layer");
        assert_eq!(
            refused.status_and_code(),
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "kind_not_allowed_in_layer"
            ),
            "{layer} {kind}"
        );
        // the schema refuses it too, whoever writes
        let raw = sqlx::query(
            "INSERT INTO layer_row (layer_id, kind, key, op, fields, rev, is_current, author, fields_hash) \
             SELECT id, $2, $3, 'add', '{}', 1, true, 'kh-t9', 'sha256:' || repeat('0', 64) FROM layer WHERE key = $1",
        )
        .bind(layer)
        .bind(kind.as_str())
        .bind(key)
        .execute(&e.owner)
        .await
        .expect_err("the trigger refuses it");
        assert!(
            raw.to_string().contains("layer_row_region_kind"),
            "{layer} {kind}: {raw}"
        );
    }
    // the same kinds on their usual layers are unchanged
    must_write(
        db,
        "base",
        Kind::DepartmentArchetype,
        "kh_t9_arch",
        json!({"label": "KH T9 archetype", "knowledge_kinds": ["kh.t9"]}),
    )
    .await;
    // and region-specific kinds still go on a sub-layer
    must_write(
        db,
        "base/region/nl",
        Kind::StageKnowledgeKind,
        "support|kh.t9_nl",
        json!({"label": "NL"}),
    )
    .await;
    // a compile reading such a row (written before the guard) reports it, never applies it
    let ls = layers();
    let mut rows = base_rows();
    rows.push(row(
        "a-nl",
        "L-base-nl",
        Kind::DepartmentArchetype,
        "kh_arch_nl",
        Op::Add,
        json!({"label": "NL archetype"}),
    ));
    let errors = compile_world(&ls, &rows, &sel(&[A], Some("nl"))).expect_err("refused");
    assert!(
        errors
            .iter()
            .any(|x| x.code == code::KIND_NOT_ALLOWED_IN_LAYER
                && x.row_id.as_deref() == Some("a-nl")),
        "{errors:#?}"
    );
}

async fn latest(db: &PgPool, family: &str) -> i32 {
    let mut c = db.acquire().await.unwrap();
    tags::latest_tag(&mut c, family)
        .await
        .unwrap()
        .map(|t| t.version)
        .unwrap_or(0)
}

/// `(change, region)` of the diff rows of `key`.
async fn diff_of(db: &PgPool, from: i32, to: i32, key: &str) -> Vec<(String, Option<String>)> {
    let mut c = db.acquire().await.unwrap();
    templates::tag_diff(&mut c, "industry/retail", from, to)
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.key == key)
        .map(|r| {
            (
                serde_json::to_value(r.change)
                    .unwrap()
                    .as_str()
                    .unwrap()
                    .to_owned(),
                r.region,
            )
        })
        .collect()
}

/// The pinned compile of the customer-care job over `pins` with `region`.
async fn compile_at(db: &PgPool, pins: &[TemplatePin], region: Option<&str>) -> Compiled {
    let mut c = db.acquire().await.unwrap();
    templates::compile_pinned(&mut c, pins, &retail(region), Some("customer_care_agent"))
        .await
        .unwrap()
        .unwrap_or_else(|e| panic!("{region:?}: {e:#?}"))
}

#[tokio::test]
async fn a_region_override_never_masks_its_parents_item_in_tag_contents_and_diffs() {
    let e = crate::common::env().await;
    let db = &e.state.db;
    let key = "support|kh.t9_diff";
    insert_region_layer(
        &e.owner,
        "industry/retail/region/nl",
        "industry",
        Some("retail"),
        None,
        "nl",
        "industry/retail",
    )
    .await
    .expect("industry/retail/region/nl");
    let v0 = latest(db, "industry/retail").await;
    let kind = |label: &str| json!({ "label": label });
    must_write(
        db,
        "industry/retail",
        Kind::StageKnowledgeKind,
        key,
        kind("Parent 1"),
    )
    .await;
    must_write(
        db,
        "industry/retail/region/nl",
        Kind::StageKnowledgeKind,
        key,
        kind("NL 1"),
    )
    .await;
    let v1 = tag(db, "industry/retail").await;
    // both items, apart
    assert_eq!(
        diff_of(db, v0, v1, key).await,
        [
            ("added".to_owned(), None),
            ("added".to_owned(), Some("nl".to_owned()))
        ]
    );
    // a change of the parent only: the parent's item changed (the nl override does not hide it)
    must_write(
        db,
        "industry/retail",
        Kind::StageKnowledgeKind,
        key,
        kind("Parent 2"),
    )
    .await;
    let v2 = tag(db, "industry/retail").await;
    assert_eq!(
        diff_of(db, v1, v2, key).await,
        [("changed".to_owned(), None)]
    );
    // a change of the nl override only: the nl item changed, the parent's did not
    must_write(
        db,
        "industry/retail/region/nl",
        Kind::StageKnowledgeKind,
        key,
        kind("NL 2"),
    )
    .await;
    let v3 = tag(db, "industry/retail").await;
    assert_eq!(
        diff_of(db, v2, v3, key).await,
        [("changed".to_owned(), Some("nl".to_owned()))]
    );

    // what a job's conflict check compares is its own compile: a job without the nl region compiles v2 and v3 alike
    let cca = latest(db, CCA).await;
    let at = |v: i32, pins: &[TemplatePin]| -> Vec<TemplatePin> {
        pins.iter()
            .map(|p| TemplatePin {
                layer: p.layer.clone(),
                version: if p.layer == "industry/retail" {
                    v
                } else {
                    p.version
                },
            })
            .collect()
    };
    let pins = pins_of(db, CCA, cca).await;
    let plain2 = compile_at(db, &at(v2, &pins), None).await;
    let plain3 = compile_at(db, &at(v3, &pins), None).await;
    assert_eq!(plain2.effective.hash, plain3.effective.hash);
    assert_eq!(plain2.knowledge, plain3.knowledge);
    let label = |c: &Compiled| {
        c.knowledge
            .kinds
            .iter()
            .find(|k| k.key() == key)
            .map(|k| k.label.clone())
    };
    assert_eq!(label(&plain3).as_deref(), Some("Parent 2"));
    let nl3 = compile_at(db, &at(v3, &pins), Some("nl")).await;
    assert_eq!(label(&nl3).as_deref(), Some("NL 2"));
}

#[tokio::test]
async fn a_job_layer_pack_may_name_a_kind_any_region_sublayer_declares() {
    let e = crate::common::env().await;
    let db = &e.state.db;
    insert_region_layer(
        &e.owner,
        "industry/retail/region/de",
        "industry",
        Some("retail"),
        None,
        "de",
        "industry/retail",
    )
    .await
    .expect("industry/retail/region/de");
    must_write(
        db,
        "industry/retail/region/de",
        Kind::StageKnowledgeKind,
        "support|kh.de_law",
        json!({"label": "DE law"}),
    )
    .await;
    // the job template names no region (each job picks its own): a de-only kind is still a declared kind
    must_write(
        db,
        CCA,
        Kind::KnowledgePack,
        "support|kh.de_law|retail/kh_de_law",
        json!({"version": 1}),
    )
    .await;
    // a kind no layer declares stays 422
    let refused = write(
        db,
        CCA,
        Kind::KnowledgePack,
        "support|kh.t9_nowhere|retail/kh_x",
        Op::Add,
        json!({"version": 1}),
    )
    .await
    .expect_err("undeclared");
    assert_eq!(
        refused.status_and_code(),
        (StatusCode::UNPROCESSABLE_ENTITY, "pack_kind_undeclared")
    );
}
