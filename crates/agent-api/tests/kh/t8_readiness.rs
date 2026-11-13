//! KH T8 (D110, D115): required knowledge blocks go-live. A required kind with no resolvable leaf (tenant leaf ∪
//! frozen pack leaves) is a blocking `knowledge_unbound` item per department; frozen packs alone satisfy a kind
//! unless it is `tenant_data`; the primary language's coverage blocks while other languages only warn; a stage `*`
//! binding serves every stage; a second department missing a required kind blocks, naming that department; the 409
//! `release_not_ready` lists the kind; a job with no selection at all blocks on the hierarchy (T1 carried).
//!
//! Real Postgres 18 (the KH binary's container, a fresh database per test), the production readiness registry, on
//! contract jobs created from `job/customer_care_agent@v4` (their template's required kinds bound to the fixture leaf
//! by `design_contract::bind_required_knowledge`).
use agent_api::audit::Actor;
use agent_api::design::DesignOf;
use agent_api::design::DraftRef;
use agent_api::platform::db::{rows, tags};
use agent_api::platform::hooks::NoHooks;
use agent_api::platform::templates;
use agent_api::platform_db::begin_platform;
use agent_api::readiness::{self, Assessed, KNOWLEDGE_DEPENDENCY, ReadOf, ReadinessCx};
use agent_api::release::bundle::Requires;
use agent_model::compile::Selection;
use agent_model::kinds::{Kind, Op};
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::common::design_contract::{self, ContractJob, FIXTURE_LEAF, cuttable_selection};
use crate::common::{TestEnv, env};

const CS: &str = "industry_dept/retail/customer_service";
const CCA: &str = "job/customer_care_agent";

/// A contract job whose draft selection is the cuttable one (template kinds bound by the fixture).
async fn job(e: &TestEnv) -> ContractJob {
    let f = design_contract::new_job(e).await;
    design_contract::write(
        e,
        &f,
        "selection",
        &DraftRef::new(),
        Some(cuttable_selection()),
    )
    .await;
    design_contract::publishable(e, &f).await;
    f
}

/// A working knowledge kind of the job (stage `t8` unless given), written after the baseline: the draft has it.
async fn kind(
    e: &TestEnv,
    f: &ContractJob,
    stage: &str,
    kind: &str,
    tenant_data: bool,
    store: Option<&str>,
    packs: &[&str],
) {
    let packs: Vec<String> = packs.iter().map(|p| p.to_string()).collect();
    sqlx::query(
        "INSERT INTO job_knowledge_kind (tenant_id, job_id, stage, kind, required, tenant_data, description, origin, \
                                         store_path, packs, updated_by) \
         VALUES ($1, $2, $3, $4, true, $5, 'T8 kind.', 'base', $6, $7, 't8')",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(stage)
    .bind(kind)
    .bind(tenant_data)
    .bind(store)
    .bind(packs)
    .execute(&e.owner)
    .await
    .unwrap();
}

/// A cached, listed platform pack `key@version` with one leaf (a frozen pack the draft can search).
async fn pack_with_leaf(e: &TestEnv, f: &ContractJob, key: &str, version: i32) {
    sqlx::query(
        "INSERT INTO platform_pack (tenant_id, pack_key, version, root_id, listed) VALUES ($1, $2, $3, $4, true)",
    )
    .bind(f.tenant)
    .bind(key)
    .bind(version)
    .bind(Uuid::new_v4())
    .execute(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO platform_pack_leaf (tenant_id, pack_key, version, leaf_id, path) VALUES ($1, $2, $3, $4, '{}')",
    )
    .bind(f.tenant)
    .bind(key)
    .bind(version)
    .bind(Uuid::new_v4())
    .execute(&e.owner)
    .await
    .unwrap();
}

/// A knowledge store of the tenant (no strata store); its id.
async fn store(e: &TestEnv, f: &ContractJob, key: &str, kind: &str, parent: Option<Uuid>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO knowledge_store (tenant_id, id, key, label, parent_id, kind, region, updated_by) \
         VALUES ($1, $2, $3, $3, $4, $5, 'eu', 't8')",
    )
    .bind(f.tenant)
    .bind(id)
    .bind(key)
    .bind(parent)
    .bind(kind)
    .execute(&e.owner)
    .await
    .unwrap();
    id
}

async fn upload(e: &TestEnv, f: &ContractJob, leaf: Uuid, lang: &str) {
    sqlx::query(
        "INSERT INTO knowledge_upload (tenant_id, id, store_id, language, files, created_by) \
         VALUES ($1, $2, $3, $4, '[{\"name\": \"returns.pdf\"}]', 't8')",
    )
    .bind(f.tenant)
    .bind(Uuid::new_v4())
    .bind(leaf)
    .bind(lang)
    .execute(&e.owner)
    .await
    .unwrap();
}

async fn languages(e: &TestEnv, f: &ContractJob, langs: &[&str]) {
    let langs: Vec<String> = langs.iter().map(|l| l.to_string()).collect();
    sqlx::query("UPDATE jobs SET languages = $2 WHERE id = $1")
        .bind(f.job.job_id)
        .bind(langs)
        .execute(&e.owner)
        .await
        .unwrap();
}

/// The production registry over the job's draft.
async fn assess(e: &TestEnv, f: &ContractJob) -> Assessed {
    let st = &e.state;
    let jr = f.job_ref();
    let mut tx = f.job.begin(&st.db).await.unwrap();
    let checked = readiness::checked(&mut tx, st.compiler.as_ref(), &jr, ReadOf::Draft)
        .await
        .unwrap();
    let cx = checked.cx(&st.design, st.compiler.as_ref());
    let a = st.readiness.assess(&mut tx, &cx).await.unwrap();
    tx.commit().await.unwrap();
    a
}

/// The knowledge items of `a` (`slot`s), in order.
fn knowledge(a: &Assessed) -> Vec<String> {
    a.missing
        .iter()
        .filter(|m| m.dependency == KNOWLEDGE_DEPENDENCY)
        .map(|m| m.slot.clone())
        .collect()
}

fn go_live_keys(a: &Assessed) -> Vec<String> {
    a.go_live.iter().map(|g| g.key.clone()).collect()
}

#[tokio::test]
async fn the_fixture_job_is_ready_and_an_unbound_required_kind_blocks_its_department() {
    let e = env().await;
    let f = job(&e).await;
    let a = assess(&e, &f).await;
    assert!(knowledge(&a).is_empty(), "{:?}", a.missing);

    kind(&e, &f, "t8", "returns", false, None, &[]).await;
    let a = assess(&e, &f).await;
    assert_eq!(knowledge(&a), [format!("{CS}:t8|returns")]);
    let item = a
        .missing
        .iter()
        .find(|m| m.dependency == KNOWLEDGE_DEPENDENCY)
        .unwrap();
    assert!(
        item.detail.contains("returns (T8 stage)"),
        "{}",
        item.detail
    );
    assert!(item.detail.contains(CS), "{}", item.detail);
    assert_eq!(
        item.href,
        format!("/admin/jobs/{}/knowledge-kinds", f.job.job_key)
    );
    // it is blocking, not a go-live warning
    assert!(
        !go_live_keys(&a).iter().any(|k| k.contains("t8:returns")),
        "{:?}",
        go_live_keys(&a)
    );
    let area = a
        .areas
        .iter()
        .find(|x| x.key == readiness::AreaKey::Knowledge)
        .unwrap();
    assert_eq!(area.status, readiness::AreaStatus::Gap);
}

#[tokio::test]
async fn a_required_kind_satisfied_by_frozen_packs_only_is_ready() {
    let e = env().await;
    let f = job(&e).await;
    pack_with_leaf(&e, &f, "industry/retail", 7).await;
    kind(&e, &f, "t8", "returns", false, None, &["industry/retail@7"]).await;
    let a = assess(&e, &f).await;
    assert!(knowledge(&a).is_empty(), "{:?}", a.missing);
    // a pin the cache has no leaf of resolves nothing
    kind(
        &e,
        &f,
        "t8",
        "warranty",
        false,
        None,
        &["industry/retail@8"],
    )
    .await;
    let a = assess(&e, &f).await;
    assert_eq!(knowledge(&a), [format!("{CS}:t8|warranty")]);
}

#[tokio::test]
async fn a_tenant_data_kind_with_packs_only_is_blocked_until_a_tenant_leaf_feeds_it() {
    let e = env().await;
    let f = job(&e).await;
    pack_with_leaf(&e, &f, "industry/retail", 7).await;
    kind(
        &e,
        &f,
        "t8",
        "order_status",
        true,
        None,
        &["industry/retail@7"],
    )
    .await;
    let a = assess(&e, &f).await;
    assert_eq!(knowledge(&a), [format!("{CS}:t8|order_status")]);
    let item = &a
        .missing
        .iter()
        .find(|m| m.dependency == KNOWLEDGE_DEPENDENCY)
        .unwrap();
    assert!(item.detail.contains("your own data"), "{}", item.detail);

    // the fixture leaf (fed by a source in the primary language) satisfies it
    sqlx::query(
        "UPDATE job_knowledge_kind SET store_path = $2 WHERE job_id = $1 AND kind = 'order_status'",
    )
    .bind(f.job.job_id)
    .bind(FIXTURE_LEAF)
    .execute(&e.owner)
    .await
    .unwrap();
    let a = assess(&e, &f).await;
    assert!(knowledge(&a).is_empty(), "{:?}", a.missing);
}

#[tokio::test]
async fn the_primary_language_gap_blocks_and_other_languages_warn() {
    let e = env().await;
    let f = job(&e).await;
    let root = store(&e, &f, "t8-kb", "group", None).await;
    let leaf = store(&e, &f, "t8-policy", "leaf", Some(root)).await;
    upload(&e, &f, leaf, "en").await;
    kind(&e, &f, "t8", "returns", false, Some("t8-kb/t8-policy"), &[]).await;

    languages(&e, &f, &["en", "nl"]).await;
    let a = assess(&e, &f).await;
    assert!(knowledge(&a).is_empty(), "{:?}", a.missing);
    let gap = a
        .go_live
        .iter()
        .find(|g| g.key == "knowledge_coverage_gap:t8:returns")
        .unwrap_or_else(|| panic!("{:?}", go_live_keys(&a)));
    assert_eq!(gap.title, "returns has no documents in nl (T8 stage)");

    // Dutch first: the primary language is uncovered, so it blocks; English is covered
    languages(&e, &f, &["nl", "en"]).await;
    let a = assess(&e, &f).await;
    assert_eq!(knowledge(&a), [format!("{CS}:t8|returns:nl")]);
    let item = a
        .missing
        .iter()
        .find(|m| m.dependency == KNOWLEDGE_DEPENDENCY)
        .unwrap();
    assert!(item.detail.contains("primary language"), "{}", item.detail);
    assert!(
        !go_live_keys(&a).contains(&"knowledge_coverage_gap:t8:returns".to_string()),
        "the primary language is not also a warning: {:?}",
        go_live_keys(&a)
    );
    // a document in Dutch closes it
    upload(&e, &f, leaf, "nl").await;
    let a = assess(&e, &f).await;
    assert!(knowledge(&a).is_empty(), "{:?}", a.missing);
}

#[tokio::test]
async fn a_stage_star_binding_serves_every_stage() {
    let e = env().await;
    let f = job(&e).await;
    kind(&e, &f, "support", "t8_any", false, None, &[]).await;
    let a = assess(&e, &f).await;
    assert_eq!(knowledge(&a), [format!("{CS}:support|t8_any")]);
    kind(&e, &f, "*", "t8_any", false, Some(FIXTURE_LEAF), &[]).await;
    let a = assess(&e, &f).await;
    assert!(knowledge(&a).is_empty(), "{:?}", a.missing);
}

#[tokio::test]
async fn release_not_ready_lists_the_kind() {
    let e = env().await;
    let f = job(&e).await;
    kind(&e, &f, "t8", "returns", false, None, &[]).await;
    let tok = design_contract::release_manager(&e, &f).await;
    let (s, b) = design_contract::approve(&e, &f, &tok, true).await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(b["error"]["code"], "release_not_ready", "{b}");
    let subject = format!("{CS}:t8|returns");
    let item = b["error"]["blocking"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["subject"] == subject.as_str())
        .unwrap_or_else(|| panic!("{b}"))
        .clone();
    assert_eq!(item["code"], "knowledge_unbound", "{item}");
    assert!(
        b["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&format!("knowledge.{subject}")),
        "{b}"
    );

    // bound: the same approve applies
    sqlx::query("UPDATE job_knowledge_kind SET store_path = $2 WHERE job_id = $1 AND kind = 'returns' AND stage = 't8'")
        .bind(f.job.job_id)
        .bind(FIXTURE_LEAF)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = design_contract::approve(&e, &f, &tok, true).await;
    assert!(s == StatusCode::CREATED || s == StatusCode::OK, "{s}: {b}");
    assert_eq!(b["applied"], true, "{b}");
}

#[tokio::test]
async fn a_job_without_a_selection_blocks_on_the_hierarchy() {
    let e = env().await;
    let f = job(&e).await;
    sqlx::query("UPDATE job_target SET selection = NULL WHERE job_id = $1")
        .bind(f.job.job_id)
        .execute(&e.owner)
        .await
        .unwrap();
    let st = &e.state;
    let mut tx = f.job.begin(&st.db).await.unwrap();
    let items = st
        .readiness
        .draft_blocking(&mut tx, &st.design, st.compiler.as_ref(), &f.job_ref())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let hierarchy: Vec<&str> = items
        .iter()
        .filter(|i| i.code == readiness::ReadinessCode::HierarchyIncomplete)
        .map(|i| i.subject.as_str())
        .collect();
    assert_eq!(hierarchy, ["hierarchy_incomplete:selection"], "{items:?}");
}

// ---- D115: every department must be satisfied ------------------------------------------------------------------------

fn actor() -> Actor {
    Actor::system("service:agent-api-kh-t8-test")
}

async fn must_write(e: &TestEnv, layer: &str, key: &str, fields: Value) {
    let mut tx = begin_platform(&e.state.db).await.expect("begin_platform");
    rows::write_revision(
        &mut tx,
        &actor(),
        layer,
        Kind::StageKnowledgeKind,
        key,
        Op::Add,
        fields,
        "kh-t8-author",
    )
    .await
    .unwrap_or_else(|err| panic!("write {layer} {key}: {err:?}"));
    tx.commit().await.expect("commit the revision");
}

async fn tag(e: &TestEnv, family: &str) -> i32 {
    let mut tx = begin_platform(&e.state.db).await.expect("begin_platform");
    let t = tags::tag_family(
        &mut tx,
        &actor(),
        family,
        "kh t8",
        "kh-t8-owner",
        &NoHooks,
        false,
    )
    .await
    .unwrap_or_else(|err| panic!("tag {family}: {err:?}"));
    tx.commit().await.expect("commit the tag");
    t.version
}

#[tokio::test]
async fn a_second_department_missing_a_required_kind_blocks_naming_it() {
    let e = env().await;
    let b_dept = "industry_dept/retail/kh_t8_b";
    sqlx::query(
        "INSERT INTO layer (key, scope, industry, department, family, label, extends, areas) \
         VALUES ($1, 'industry_dept', 'retail', 'kh_t8_b', $1, 'KH T8 B', 'customer_service', '{}')",
    )
    .bind(b_dept)
    .execute(&e.owner)
    .await
    .unwrap();
    must_write(
        &e,
        b_dept,
        "support|kh.t8_b_only",
        json!({"label": "B only", "required": true}),
    )
    .await;
    tag(&e, b_dept).await;
    must_write(
        &e,
        CCA,
        "support|kh.t8_job",
        json!({"label": "T8 job kind"}),
    )
    .await;
    let v = tag(&e, CCA).await;
    let pins = {
        let mut c = e.state.db.acquire().await.unwrap();
        templates::resolve_template(&mut c, CCA, v)
            .await
            .unwrap_or_else(|err| panic!("{CCA}@v{v}: {err:?}"))
            .pins
    };
    assert!(pins.iter().any(|p| p.layer == b_dept), "{pins:?}");

    // a job on the new pins, both departments selected (customer service first)
    let f = job(&e).await;
    sqlx::query("UPDATE jobs SET pins = $2 WHERE id = $1")
        .bind(f.job.job_id)
        .bind(serde_json::to_value(&pins).unwrap())
        .execute(&e.owner)
        .await
        .unwrap();
    let mut sel: Selection = serde_json::from_value(cuttable_selection()).unwrap();
    sel.departments = vec![CS.to_string(), b_dept.to_string()];
    let items = |sel: Selection| {
        let e = &e;
        let f = &f;
        async move {
            let st = &e.state;
            let jr = f.job_ref();
            let requires = Requires::default();
            let causes = BTreeMap::new();
            let cx = ReadinessCx {
                job: &jr,
                of: ReadOf::Draft,
                design: DesignOf::Draft,
                selection: Some(&sel),
                requires: &requires,
                causes: &causes,
                rules: None,
                reg: &st.design,
                compiler: st.compiler.as_ref(),
            };
            let mut tx = f.job.begin(&st.db).await.unwrap();
            let a = st.readiness.assess(&mut tx, &cx).await.unwrap();
            tx.commit().await.unwrap();
            a
        }
    };
    let a = items(sel.clone()).await;
    let k = knowledge(&a);
    assert!(
        k.contains(&format!("{b_dept}:support|kh.t8_b_only")),
        "the second department's own kind blocks, named by it: {k:?}"
    );
    assert!(
        !k.iter().any(|s| s.starts_with(&format!("{CS}:"))),
        "customer service is satisfied: {k:?}"
    );
    let item = a
        .missing
        .iter()
        .find(|m| m.slot == format!("{b_dept}:support|kh.t8_b_only"))
        .unwrap();
    assert!(item.detail.contains(b_dept), "{}", item.detail);

    // customer service alone does not have the kind
    let mut one = sel.clone();
    one.departments = vec![CS.to_string()];
    let a = items(one).await;
    assert!(
        !knowledge(&a).iter().any(|s| s.contains("kh.t8_b_only")),
        "{:?}",
        knowledge(&a)
    );

    // bound (the template's kinds of the new selection materialised, then the fixture leaf): every department passes
    {
        let st = &e.state;
        let jr = f.job_ref();
        let mut tx = f.job.begin(&st.db).await.unwrap();
        let seeds =
            agent_api::knowledge::kinds::template_seeds(&mut tx, &pins, Some(&sel), Some(&jr))
                .await
                .unwrap();
        agent_api::knowledge::kinds::materialise(&mut tx, &jr, &seeds)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    sqlx::query("UPDATE job_knowledge_kind SET store_path = $2 WHERE job_id = $1 AND required AND store_path IS NULL")
        .bind(f.job.job_id)
        .bind(FIXTURE_LEAF)
        .execute(&e.owner)
        .await
        .unwrap();
    let a = items(sel).await;
    assert!(knowledge(&a).is_empty(), "{:?}", knowledge(&a));
}

// ---- fix round 1: what feeds a tenant leaf, and the upgrade finding --------------------------------------------------

/// A `file` source of the job writing into `leaf` in `lang` (`enabled`); its id.
async fn source(
    e: &TestEnv,
    f: &ContractJob,
    name: &str,
    leaf: Uuid,
    lang: &str,
    enabled: bool,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO source (tenant_id, job_id, id, name, type, target_store_id, language, enabled, updated_by) \
         VALUES ($1, $2, $3, $4, 'file', $5, $6, $7, 't8')",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(id)
    .bind(name)
    .bind(leaf)
    .bind(lang)
    .bind(enabled)
    .execute(&e.owner)
    .await
    .unwrap();
    id
}

/// A finished run of `source` (`completed` | `failed`) that produced `documents`.
async fn run(e: &TestEnv, f: &ContractJob, source: Uuid, status: &str, documents: i32) {
    sqlx::query(
        "INSERT INTO source_run (tenant_id, job_id, id, source_id, status, finished_at, started_by, documents) \
         VALUES ($1, $2, gen_random_uuid(), $3, $4, now(), 't8', $5)",
    )
    .bind(f.tenant)
    .bind(f.job.job_id)
    .bind(source)
    .bind(status)
    .bind(documents)
    .execute(&e.owner)
    .await
    .unwrap();
}

#[tokio::test]
async fn a_source_covers_its_language_once_a_run_produced_documents_enabled_or_not() {
    let e = env().await;
    let f = job(&e).await;
    let root = store(&e, &f, "t8-src", "group", None).await;
    let leaf = store(&e, &f, "t8-fed", "leaf", Some(root)).await;
    kind(&e, &f, "t8", "returns", false, Some("t8-src/t8-fed"), &[]).await;
    languages(&e, &f, &["en"]).await;
    let blocked = [format!("{CS}:t8|returns:en")];

    // enabled, never ran: feeds nothing
    let s = source(&e, &f, "t8 site", leaf, "en", true).await;
    assert_eq!(knowledge(&assess(&e, &f).await), blocked);
    // a failed run and a completed run with no documents: still nothing
    run(&e, &f, s, "failed", 0).await;
    run(&e, &f, s, "completed", 0).await;
    assert_eq!(knowledge(&assess(&e, &f).await), blocked);
    // a completed run with documents covers en, and stays covering once the source is disabled
    run(&e, &f, s, "completed", 3).await;
    assert!(knowledge(&assess(&e, &f).await).is_empty());
    sqlx::query("UPDATE source SET enabled = false WHERE id = $1")
        .bind(s)
        .execute(&e.owner)
        .await
        .unwrap();
    let a = assess(&e, &f).await;
    assert!(knowledge(&a).is_empty(), "{:?}", a.missing);
    // documents removed by a later run or a purge: nothing left
    sqlx::query("UPDATE source_run SET documents_removed = true WHERE source_id = $1")
        .bind(s)
        .execute(&e.owner)
        .await
        .unwrap();
    assert_eq!(knowledge(&assess(&e, &f).await), blocked);
}

#[tokio::test]
async fn languages_compare_exactly_as_coverage_does() {
    let e = env().await;
    let f = job(&e).await;
    let root = store(&e, &f, "t8-lang", "group", None).await;
    let leaf = store(&e, &f, "t8-gb", "leaf", Some(root)).await;
    kind(&e, &f, "t8", "returns", false, Some("t8-lang/t8-gb"), &[]).await;
    let s = source(&e, &f, "t8 uk site", leaf, "en-GB", true).await;
    run(&e, &f, s, "completed", 2).await;
    // coverage_of counts a document in its stored tag only: en-GB documents do not serve en
    languages(&e, &f, &["en"]).await;
    assert_eq!(
        knowledge(&assess(&e, &f).await),
        [format!("{CS}:t8|returns:en")]
    );
    languages(&e, &f, &["en-GB"]).await;
    let a = assess(&e, &f).await;
    assert!(
        !knowledge(&a).iter().any(|k| k.contains("t8|returns")),
        "{:?}",
        knowledge(&a)
    );
}

#[tokio::test]
async fn a_tenant_data_kind_on_a_bound_but_empty_leaf_is_unbound() {
    let e = env().await;
    let f = job(&e).await;
    let root = store(&e, &f, "t8-own", "group", None).await;
    let leaf = store(&e, &f, "t8-orders", "leaf", Some(root)).await;
    kind(
        &e,
        &f,
        "t8",
        "order_status",
        true,
        Some("t8-own/t8-orders"),
        &[],
    )
    .await;
    // unbound (nothing feeds the leaf), not merely a primary-language gap: the job's languages do not decide it
    // (`jobs.languages` is never empty, so "no languages" cannot let it pass either)
    let a = assess(&e, &f).await;
    assert_eq!(knowledge(&a), [format!("{CS}:t8|order_status")]);
    let item = a
        .missing
        .iter()
        .find(|m| m.dependency == KNOWLEDGE_DEPENDENCY)
        .unwrap();
    assert!(
        item.detail.contains("nothing of yours feeds it"),
        "{}",
        item.detail
    );
    // a connector run with documents feeds it
    let s = source(&e, &f, "t8 orders api", leaf, "en", true).await;
    run(&e, &f, s, "completed", 5).await;
    let a = assess(&e, &f).await;
    assert!(knowledge(&a).is_empty(), "{:?}", a.missing);
}

#[tokio::test]
async fn an_upgrade_introducing_a_required_kind_is_listed_and_blocks_apply_until_bound() {
    let e = env().await;
    let f = job(&e).await;
    let tok = design_contract::release_manager(&e, &f).await;
    let old: Vec<templates::TemplatePin> = serde_json::from_value(
        sqlx::query_scalar::<_, Value>("SELECT pins FROM jobs WHERE id = $1")
            .bind(f.job.job_id)
            .fetch_one(&e.owner)
            .await
            .unwrap(),
    )
    .unwrap();

    // the template's customer-service department makes a new kind required
    must_write(
        &e,
        CS,
        "support|kh.t8_new",
        json!({"label": "New policy", "required": true}),
    )
    .await;
    tag(&e, CS).await;
    must_write(&e, CCA, "support|kh.t8_up", json!({"label": "T8 upgrade"})).await;
    let v = tag(&e, CCA).await;
    let new = {
        let mut c = e.state.db.acquire().await.unwrap();
        templates::resolve_template(&mut c, CCA, v)
            .await
            .unwrap()
            .pins
    };
    assert_ne!(old, new);
    // the job adopts the new pins and cuts from them (a release is immutable; its working rows lack the new kind)
    sqlx::query("UPDATE jobs SET pins = $2 WHERE id = $1")
        .bind(f.job.job_id)
        .bind(serde_json::to_value(&new).unwrap())
        .execute(&e.owner)
        .await
        .unwrap();
    let r1 = design_contract::cut(&e, &f, &tok, false).await;
    let bundle: Value =
        sqlx::query_scalar("SELECT bundle FROM job_release WHERE job_id = $1 AND release_id = $2")
            .bind(f.job.job_id)
            .bind(&r1)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    let jr = f.job_ref();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    agent_api::release::conflicts::raise_upgrade(
        &mut tx,
        &e.state.conflict_detectors,
        f.tenant,
        &jr,
        &r1,
        &old,
        &new,
        None,
        &bundle,
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let key = format!("{CS}:support|kh.t8_new");
    let found: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT kind, object, tenant_row, detail FROM release_conflict \
          WHERE job_id = $1 AND status = 'open' AND key = $2",
    )
    .bind(f.job.job_id)
    .bind(&key)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(
        (
            found[0].0.as_str(),
            found[0].1.as_str(),
            found[0].2.as_str()
        ),
        ("required_kind_introduced", "knowledge_kind", "none")
    );
    assert!(found[0].3.contains("kh.t8_new"), "{}", found[0].3);
    // the kinds that were already required are not listed
    let others: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM release_conflict WHERE job_id = $1 AND kind = 'required_kind_introduced'",
    )
    .bind(f.job.job_id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(others, 1);

    // apply of the upgraded cut is refused until the kind resolves
    let (s, b) = crate::common::send(
        &e.app,
        crate::common::json(
            "POST",
            &format!("/api/jobs/{}/releases/{r1}/apply", f.job.job_key),
            Some(&tok),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert!(
        b["error"]["blocking"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["subject"] == key.as_str() && i["code"] == "knowledge_unbound"),
        "{b}"
    );

    // bound (its working row from the new pins, then the fixture leaf): a new cut applies
    design_contract::bind_required_knowledge(&e, &f).await;
    let (s, b) = design_contract::approve(&e, &f, &tok, true).await;
    assert!(s == StatusCode::CREATED || s == StatusCode::OK, "{s}: {b}");
    assert_eq!(b["applied"], true, "{b}");
}
