//! PK T3 (plan PK-1/PK-2/PK-3): template `knowledge_pack` rows → a job's kind pins. The fold over pinned tags
//! (layer order, the wildcard tiers, layer and job tombstones), job creation materialising the pins
//! (`packs_source = 'template'`), a `knowledge_pack` refinement tombstone dropping one pin of one kind and staying
//! dropped after an adopt, an adopt of a newer tag re-pinning template rows but not `binding` rows, `tag_diff` naming
//! the changed row, the LayerCompiler's `objects.knowledge_pack` base, and the bundle goldens hashing as before.
//! Real Postgres 18 (the pk binary's container, a fresh database per test: the platform rows written here are this
//! test's own).
use crate::common;

use agent_api::audit::Actor;
use agent_api::design::{DraftRef, JobRef, Place};
use agent_api::knowledge::kinds;
use agent_api::knowledge::packs::template as packs;
use agent_api::platform::db::{rows, tags};
use agent_api::platform::hooks::NoHooks;
use agent_api::platform::templates::{self, TemplatePin};
use agent_api::platform_db::begin_platform;
use agent_api::release::compile::{self, TemplateCompiler, run_pins_adopted};
use agent_api::release::hash;
use agent_api::tenancy::begin_scoped;
use agent_model::compile::Selection;
use agent_model::kinds::{Kind, Op};
use agent_model::tags::DiffChange;
use axum::http::StatusCode;
use common::{get, insert_tenant, insert_user, person, send, super_admin};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

const CCA: &str = "job/customer_care_agent";
const RETAIL: &str = "industry/retail";

fn actor() -> Actor {
    Actor::system("service:agent-api-pk-t3-test")
}

/// A `knowledge_pack` revision in its own platform transaction, committed.
async fn write(db: &PgPool, layer: &str, key: &str, op: Op, fields: Value) {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    rows::write_revision(
        &mut tx,
        &actor(),
        layer,
        Kind::KnowledgePack,
        key,
        op,
        fields,
        "pk-t3-author",
    )
    .await
    .unwrap_or_else(|e| panic!("write {layer} {key}: {e:?}"));
    tx.commit().await.expect("commit the revision");
}

/// A `stage_knowledge_kind` declaration (D107: a pack reference must name a declared kind), committed.
async fn declare(db: &PgPool, layer: &str, key: &str) {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    rows::write_revision(
        &mut tx,
        &actor(),
        layer,
        Kind::StageKnowledgeKind,
        key,
        Op::Add,
        json!({"label": key}),
        "pk-t3-author",
    )
    .await
    .unwrap_or_else(|e| panic!("declare {layer} {key}: {e:?}"));
    tx.commit().await.expect("commit the declaration");
}

/// `tag_family` in its own platform transaction, committed: the new version.
async fn cut(db: &PgPool, family: &str) -> i32 {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    let t = tags::tag_family(
        &mut tx,
        &actor(),
        family,
        "pk t3",
        "pk-t3-owner",
        &NoHooks,
        false,
    )
    .await
    .unwrap_or_else(|e| panic!("tag {family}: {e:?}"));
    tx.commit().await.expect("commit the tag");
    t.version
}

/// The seed template's selection (the head's `template_selection`).
fn sel() -> Selection {
    serde_json::from_value(json!({
        "profile": "retail_ecommerce",
        "industry": "retail",
        "departments": ["industry_dept/retail/customer_service"],
    }))
    .unwrap()
}

async fn template_pins(db: &PgPool, version: i32) -> Vec<TemplatePin> {
    let mut c = db.acquire().await.unwrap();
    templates::resolve_template(&mut c, CCA, version)
        .await
        .unwrap_or_else(|e| panic!("{CCA}@v{version}: {e:?}"))
        .pins
}

fn strings(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_owned()).collect()
}

#[tokio::test]
async fn fold_applies_layer_order_wildcards_and_tombstones() {
    let e = common::env().await;
    let db = &e.state.db;
    // the brief's fold: base *|*|base@1, industry *|*|industry/retail@1, job support|faq|industry/retail@2 and a
    // job-layer tombstone of *|*|base
    write(db, "base", "*|*|base", Op::Add, json!({"version": 1})).await;
    write(
        db,
        RETAIL,
        "*|*|industry/retail",
        Op::Add,
        json!({"version": 1}),
    )
    .await;
    // D107 (KH T3): a pack reference names a declared kind
    declare(db, CCA, "support|faq").await;
    write(
        db,
        CCA,
        "support|faq|industry/retail",
        Op::Add,
        json!({"version": 2}),
    )
    .await;
    write(db, CCA, "*|*|base", Op::Tombstone, json!({})).await;
    cut(db, "base").await;
    cut(db, RETAIL).await;
    let v = cut(db, CCA).await;
    let pins = template_pins(db, v).await;
    let mut c = db.acquire().await.unwrap();

    let refs = packs::fold(&mut c, &pins, &sel()).await.unwrap();
    let keys: Vec<String> = refs.iter().map(packs::PackRef::key).collect();
    assert_eq!(
        keys,
        ["*|*|industry/retail", "support|faq|industry/retail"],
        "the job layer folds last: its tombstone removes base: {refs:?}"
    );
    let job_row = &refs[1];
    assert_eq!((job_row.layer.as_str(), job_row.version), (CCA, 2));
    assert_eq!(refs[0].layer, RETAIL);
    // exact beats *|*; every other kind keeps the wildcard
    assert_eq!(
        packs::pins_for(&refs, "support", "faq", &[]),
        ["industry/retail@2"]
    );
    assert_eq!(
        packs::pins_for(&refs, "exit", "policy.privacy", &[]),
        ["industry/retail@1"]
    );
    // a job tombstone of one kind's key removes the pack from that kind only
    let tomb = strings(&["support|faq|industry/retail"]);
    assert!(packs::pins_for(&refs, "support", "faq", &tomb).is_empty());
    assert_eq!(
        packs::pins_for(&refs, "support", "returns", &tomb),
        ["industry/retail@1"]
    );
    // *|*|key removes it from every kind
    let every = strings(&["*|*|industry/retail"]);
    assert!(packs::pins_for(&refs, "exit", "policy.privacy", &every).is_empty());
    assert!(packs::pins_for(&refs, "support", "faq", &every).is_empty());

    // a selection without the industry does not apply the industry layer
    let mut no_industry = sel();
    no_industry.industry.clear();
    no_industry.departments.clear();
    let keys: Vec<String> = packs::fold(&mut c, &pins, &no_industry)
        .await
        .unwrap()
        .iter()
        .map(packs::PackRef::key)
        .collect();
    assert_eq!(keys, ["support|faq|industry/retail"]);
    // the seed's own tags carry no knowledge_pack row
    let old = template_pins(db, 4).await;
    assert!(packs::fold(&mut c, &old, &sel()).await.unwrap().is_empty());
}

/// The kind's `(packs, packs_source)`.
async fn packs_of(owner: &PgPool, job: &JobRef, stage: &str, kind: &str) -> (Vec<String>, String) {
    sqlx::query_as(
        "SELECT packs, packs_source FROM job_knowledge_kind WHERE job_id = $1 AND stage = $2 AND kind = $3",
    )
    .bind(job.id)
    .bind(stage)
    .bind(kind)
    .fetch_one(owner)
    .await
    .unwrap_or_else(|e| panic!("{stage}/{kind}: {e}"))
}

async fn refine(
    e: &common::TestEnv,
    token: &str,
    job: &JobRef,
    op: &str,
    key: &str,
) -> (StatusCode, Value) {
    send(
        &e.app,
        common::json(
            "POST",
            &format!("/api/jobs/{}/refinements/{op}", job.key),
            Some(token),
            json!({"object": "knowledge_pack", "key": key}),
        ),
    )
    .await
}

#[tokio::test]
async fn job_pins_follow_template_tombstones_and_adopts() {
    let e = common::env().await;
    let db = &e.state.db;
    // the kinds the job materialises pins for are the platform's declarations: this test declares its own three
    for kind in [
        "support|policy.returns",
        "support|policy.warranty",
        "purchase|policy.payment",
    ] {
        declare(db, "base", kind).await;
    }
    write(db, "base", "*|*|base", Op::Add, json!({"version": 1})).await;
    write(
        db,
        RETAIL,
        "*|*|industry/retail",
        Op::Add,
        json!({"version": 1}),
    )
    .await;
    write(
        db,
        CCA,
        "support|*|job/customer_care_agent",
        Op::Add,
        json!({"version": 1}),
    )
    .await;
    cut(db, "base").await;
    let retail_v1 = cut(db, RETAIL).await;
    let v = cut(db, CCA).await;

    // creating a job materialises the template's pins
    let tenant = Uuid::new_v4();
    insert_tenant(&e.owner, tenant, &format!("PK T3 {tenant}")).await;
    let (s, b) = send(
        &e.app,
        common::json(
            "POST",
            &format!("/api/tenants/{tenant}/jobs"),
            Some(&super_admin()),
            json!({"template": format!("{CCA}@v{v}"), "key": "pk_t3", "label": "pk t3"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = 'pk_t3'")
        .bind(tenant)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let job = JobRef {
        id,
        key: "pk_t3".into(),
    };
    let three = strings(&["base@1", "industry/retail@1", "job/customer_care_agent@1"]);
    assert_eq!(
        packs_of(&e.owner, &job, "support", "policy.returns").await,
        (three.clone(), "template".to_string())
    );
    assert_eq!(
        packs_of(&e.owner, &job, "purchase", "policy.payment").await,
        (
            strings(&["base@1", "industry/retail@1"]),
            "template".to_string()
        ),
        "support|* does not reach another stage"
    );

    // the overlay base: one item per (stage, kind, pack) of the declared kinds
    let sub = format!("u-pk-t3-{}", Uuid::new_v4().simple());
    insert_user(&e.owner, &sub, tenant, "job_designer").await;
    let token = person(&sub, &[]);
    let (s, b) = send(&e.app, get("/api/jobs/pk_t3/knowledge_pack", Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let item = b["base"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["key"] == "support|policy.returns|base")
        .unwrap_or_else(|| panic!("the base item: {b}"))
        .clone();
    assert_eq!(item["pin"], "base@1");
    assert_eq!(item["label"], "base@v1 → support/policy.returns");

    // a refinement tombstone drops one pin of one kind, in the same tracked write
    let (s, b) = refine(&e, &token, &job, "hide", "support|policy.returns|base").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        packs_of(&e.owner, &job, "support", "policy.returns")
            .await
            .0,
        strings(&["industry/retail@1", "job/customer_care_agent@1"])
    );
    assert_eq!(
        packs_of(&e.owner, &job, "support", "policy.warranty")
            .await
            .0,
        three,
        "another kind keeps it"
    );
    // unhide restores it, hide again drops it
    let (s, b) = refine(&e, &token, &job, "unhide", "support|policy.returns|base").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        packs_of(&e.owner, &job, "support", "policy.returns")
            .await
            .0,
        three
    );
    let (s, b) = refine(&e, &token, &job, "hide", "support|policy.returns|base").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let tombstone: Value = sqlx::query_scalar(
        "SELECT to_jsonb(r) - 'tenant_id' - 'job_id' - 'object' FROM job_overlay_row r \
          WHERE job_id = $1 AND object = 'knowledge_pack' AND key = 'support|policy.returns|base'",
    )
    .bind(job.id)
    .fetch_one(&e.owner)
    .await
    .unwrap();

    // discarding the tombstone draft (the overlay target's `set` back to the live value) pins the pack again
    let (s, b) = send(
        &e.app,
        common::json("DELETE", "/api/jobs/pk_t3/draft", Some(&token), json!({})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        packs_of(&e.owner, &job, "support", "policy.returns")
            .await
            .0,
        three,
        "discard re-pins"
    );
    // restoring it (the same `set` with the tombstone, as `drafts::restore` writes it) drops it again
    let mut place = DraftRef::new();
    place.insert("object".into(), json!("knowledge_pack"));
    place.insert("key".into(), json!("support|policy.returns|base"));
    let mut tx = begin_scoped(db, tenant).await.unwrap();
    let overlay = e.state.design.target("overlay").unwrap();
    agent_api::design::DesignTarget::set(
        overlay,
        &mut tx,
        Place {
            tenant,
            job: Some(&job),
        },
        &place,
        &Some(tombstone),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        packs_of(&e.owner, &job, "support", "policy.returns")
            .await
            .0,
        strings(&["industry/retail@1", "job/customer_care_agent@1"]),
        "restore unpins"
    );

    // knowledge_pack takes tombstones only: a rebase that would keep an extension is refused
    sqlx::query(
        "INSERT INTO job_overlay_row (tenant_id, job_id, object, key, kind, item, updated_by) \
         VALUES ($1, $2, 'knowledge_pack', 'support|policy.returns|ghost', 'extension', \
                 '{\"key\": \"support|policy.returns|ghost\"}', 'test')",
    )
    .bind(tenant)
    .bind(job.id)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = refine(&e, &token, &job, "rebase", "support|policy.returns|ghost").await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(b["error"]["code"], "invalid_refinement", "{b}");
    sqlx::query(
        "DELETE FROM job_overlay_row WHERE job_id = $1 AND key = 'support|policy.returns|ghost'",
    )
    .bind(job.id)
    .execute(&e.owner)
    .await
    .unwrap();

    // a binding row is the tenant's: an adopt never re-pins it
    sqlx::query(
        "UPDATE job_knowledge_kind SET packs = '{base@1}', packs_source = 'binding' \
          WHERE job_id = $1 AND stage = 'purchase' AND kind = 'policy.payment'",
    )
    .bind(job.id)
    .execute(&e.owner)
    .await
    .unwrap();

    // a newer industry tag: tag_diff names the changed knowledge_pack row
    write(
        db,
        RETAIL,
        "*|*|industry/retail",
        Op::Add,
        json!({"version": 2}),
    )
    .await;
    let retail_v2 = cut(db, RETAIL).await;
    {
        let mut c = db.acquire().await.unwrap();
        let diff = templates::tag_diff(&mut c, RETAIL, retail_v1, retail_v2)
            .await
            .unwrap();
        let row = diff
            .iter()
            .find(|d| d.kind == Kind::KnowledgePack && d.key == "*|*|industry/retail")
            .unwrap_or_else(|| panic!("the pack row changed: {diff:#?}"));
        assert_eq!(row.change, DiffChange::Changed);
        assert_ne!(row.before_hash, row.after_hash);
    }

    // adopt it as approve does: adopted pins, the compile, the registered PinsAdopted hook
    let mut tx = begin_scoped(db, tenant).await.unwrap();
    let pins: Vec<TemplatePin> = serde_json::from_value(
        sqlx::query_scalar::<_, Value>("SELECT pins FROM jobs WHERE id = $1")
            .bind(job.id)
            .fetch_one(&mut *tx)
            .await
            .unwrap(),
    )
    .unwrap();
    let new = compile::adopted_pins(
        &mut tx,
        e.state.compiler.as_ref(),
        &pins,
        &[RETAIL.to_string()],
    )
    .await
    .unwrap();
    assert!(
        new.iter()
            .any(|p| p.layer == RETAIL && p.version == retail_v2),
        "{new:?}"
    );
    let compiled = TemplateCompiler::compile(e.state.compiler.as_ref(), &mut tx, &sel(), &new)
        .await
        .unwrap_or_else(|e| panic!("the adopted pins compile: {e:?}"));
    let base_item = compiled
        .bundle
        .objects
        .knowledge_pack
        .iter()
        .find(|i| i["key"] == "support|policy.returns|industry/retail")
        .unwrap_or_else(|| panic!("{:?}", compiled.bundle.objects.knowledge_pack));
    assert_eq!(base_item["pin"], "industry/retail@2");
    run_pins_adopted(&e.state.pins_adopted, &mut tx, &job, &pins, &new, &compiled)
        .await
        .unwrap();
    sqlx::query("UPDATE jobs SET pins = $2 WHERE id = $1")
        .bind(job.id)
        .bind(serde_json::to_value(&new).unwrap())
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        packs_of(&e.owner, &job, "support", "policy.returns").await,
        (
            strings(&["industry/retail@2", "job/customer_care_agent@1"]),
            "template".to_string()
        ),
        "re-pinned, and the tombstone survives the adopt"
    );
    assert_eq!(
        packs_of(&e.owner, &job, "support", "policy.warranty")
            .await
            .0,
        strings(&["base@1", "industry/retail@2", "job/customer_care_agent@1"])
    );
    assert_eq!(
        packs_of(&e.owner, &job, "purchase", "policy.payment").await,
        (strings(&["base@1"]), "binding".to_string()),
        "a binding row is never re-pinned"
    );

    // a *|*|key tombstone removes the pack from every kind it reached
    sqlx::query(
        "INSERT INTO job_overlay_row (tenant_id, job_id, object, key, kind, updated_by) \
         VALUES ($1, $2, 'knowledge_pack', '*|*|job/customer_care_agent', 'tombstone', 'test')",
    )
    .bind(tenant)
    .bind(job.id)
    .execute(&e.owner)
    .await
    .unwrap();
    let mut tx = begin_scoped(db, tenant).await.unwrap();
    kinds::refresh_template_packs(&mut tx, &job, "*|*|job/customer_care_agent")
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        packs_of(&e.owner, &job, "support", "policy.warranty")
            .await
            .0,
        strings(&["base@1", "industry/retail@2"])
    );
    assert_eq!(
        packs_of(&e.owner, &job, "support", "policy.returns")
            .await
            .0,
        strings(&["industry/retail@2"])
    );
}

/// PK-3: a bundle with no packs serialises and hashes exactly as before (the goldens' objects carry no
/// `knowledge_pack`, and a stored bundle without the field reads back to the same hash).
#[tokio::test]
async fn bundles_without_packs_hash_as_before() {
    // the LayerCompiler over the seed's own tags (no knowledge_pack rows) leaves the field out too
    let e = common::env().await;
    let pins = template_pins(&e.state.db, 4).await;
    let mut c = e.state.db.acquire().await.unwrap();
    let got = TemplateCompiler::compile(e.state.compiler.as_ref(), &mut c, &sel(), &pins)
        .await
        .unwrap_or_else(|e| panic!("the seed template compiles: {e:?}"));
    assert!(got.bundle.objects.knowledge_pack.is_empty());
    let v = serde_json::to_value(&got.bundle).unwrap();
    assert!(
        v["objects"].get("knowledge_pack").is_none(),
        "{}",
        v["objects"]
    );
    assert_eq!(got.bundle_hash, hash::bundle_hash(&v).unwrap());
}
