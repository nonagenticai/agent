//! B8 phase 2 T2b: the industry journey grid loader (`journey::grid`) over the content `support` authors and over layers
//! the tests create: the KH fold of the journey kinds with each item's source layer, the status / tags / draft
//! revisions, the pinned `load_at`, the industry summaries and the rule-4 stage scope. Real Postgres 18; layers and
//! tags go in through the owner pool (as `tests/platform_schema.rs` does), revisions through `write_revision`.
use agent_api::audit::Actor;
use agent_api::error::ApiResult;
use agent_api::journey::grid::{self, SummaryStatus};
use agent_api::journey::model::{IndustryJourney, JourneyStatus, Severity};
use agent_api::platform::db::rows;
use agent_api::platform::templates::TemplatePin;
use agent_api::platform_db::begin_platform;
use agent_model::compile::Selection;
use agent_model::kinds::{Kind, Op};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::common;

const AUTHOR: &str = "b8-t2b-author";

fn actor() -> Actor {
    Actor::system("service:agent-api-b8-t2b-test")
}

fn hash(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

async fn industry_layer(db: &PgPool, industry: &str) -> String {
    let key = format!("industry/{industry}");
    sqlx::query("INSERT INTO layer (key, scope, industry, family) VALUES ($1, 'industry', $2, $1)")
        .bind(&key)
        .bind(industry)
        .execute(db)
        .await
        .expect("insert an industry layer");
    key
}

async fn dept_layer(db: &PgPool, industry: &str, dept: &str, extends: Option<&str>) -> String {
    let key = format!("industry_dept/{industry}/{dept}");
    sqlx::query(
        "INSERT INTO layer (key, scope, industry, department, family, extends) \
         VALUES ($1, 'industry_dept', $2, $3, $1, $4)",
    )
    .bind(&key)
    .bind(industry)
    .bind(dept)
    .bind(extends)
    .execute(db)
    .await
    .expect("insert an industry_dept layer");
    key
}

/// One industry with one department, no rows yet: (industry layer key, department layer key).
async fn pair(db: &PgPool, industry: &str, dept: &str) -> (String, String) {
    (
        industry_layer(db, industry).await,
        dept_layer(db, industry, dept, None).await,
    )
}

async fn write(db: &PgPool, layer: &str, kind: Kind, key: &str, op: Op, fields: Value) {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    rows::write_revision(&mut tx, &actor(), layer, kind, key, op, fields, AUTHOR)
        .await
        .unwrap_or_else(|e| panic!("write {kind} {key} in {layer}: {e:?}"));
    tx.commit().await.expect("commit the revision");
}

/// `family@v<version>` holding the family's current rows (owner insert, as `tests/platform_schema.rs`).
async fn tag(db: &PgPool, family: &str, version: i32) {
    let mut tx = db.begin().await.expect("begin");
    sqlx::query(
        "INSERT INTO layer_tag (family, version, content_hash, provisional, tagged_by) \
         VALUES ($1, $2, $3, false, 'test')",
    )
    .bind(family)
    .bind(version)
    .bind(hash('d'))
    .execute(&mut *tx)
    .await
    .expect("insert the tag");
    sqlx::query(
        "INSERT INTO layer_tag_row (family, version, row_id) \
         SELECT $1::text, $2::int, r.id FROM layer_row r JOIN layer l ON l.id = r.layer_id \
          WHERE l.family = $1 AND r.is_current",
    )
    .bind(family)
    .bind(version)
    .execute(&mut *tx)
    .await
    .expect("insert the tag members");
    tx.commit().await.expect("commit the tag");
}

async fn load(db: &PgPool, industry: &str, dept: &str) -> ApiResult<IndustryJourney> {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    let r = grid::load(&mut tx, industry, dept).await;
    tx.rollback().await.expect("rollback");
    r
}

fn code<T: std::fmt::Debug>(r: ApiResult<T>) -> (u16, &'static str) {
    let (status, code) = r.expect_err("expected a refusal").status_and_code();
    (status.as_u16(), code)
}

#[tokio::test]
async fn grid_loads_authored_retail_customer_service() {
    let e = common::env().await;
    crate::b8::support::content(&e).await;
    let j = load(&e.state.db, "retail", "customer_service")
        .await
        .expect("the authored retail customer_service grid loads");
    assert_eq!(j.industry, "retail");
    assert_eq!(j.department, "customer_service");
    assert!(!j.themes.is_empty(), "themes");
    assert!(!j.stages.is_empty(), "stages");
    assert!(!j.activities.is_empty(), "activities");
    let known = [
        "base",
        "industry/retail",
        "base_dept/",
        "industry_dept/retail/customer_service",
        "department_archetype/",
    ];
    let from_known = |layer: &str| known.iter().any(|k| layer.starts_with(k));
    for a in &j.activities {
        let layer = a.layer.as_deref().expect("an activity carries its layer");
        assert!(from_known(layer), "{}: {layer}", a.key);
    }
    for s in &j.stages {
        let layer = s.layer.as_deref().expect("a stage carries its layer");
        assert!(from_known(layer), "{}: {layer}", s.key);
    }
    assert!(
        j.tag
            .as_deref()
            .is_some_and(|t| t.starts_with("industry/retail@v")),
        "{:?}",
        j.tag
    );
    assert!(
        j.dept_tag
            .as_deref()
            .is_some_and(|t| t.starts_with("industry_dept/retail/customer_service@v")),
        "{:?}",
        j.dept_tag
    );
    // the wire shape serialises (the web reads it)
    let v = serde_json::to_value(&j).expect("serialise");
    assert!(v["activities"].is_array() && v["findings"].is_array());
}

#[tokio::test]
async fn status_draft_when_never_tagged() {
    let e = common::env().await;
    let (ind, dept) = pair(&e.owner, "b8t2b_draft", "b8t2b_dept").await;
    write(
        &e.state.db,
        &ind,
        Kind::JourneyStage,
        "b8t2b_stage",
        Op::Add,
        json!({"label": "Stage", "description": "the stage"}),
    )
    .await;
    write(
        &e.state.db,
        &dept,
        Kind::Activity,
        "b8t2b_stage:look_up",
        Op::Add,
        json!({"label": "Look it up", "actor": "customer", "trigger": "asks"}),
    )
    .await;
    let j = load(&e.state.db, "b8t2b_draft", "b8t2b_dept")
        .await
        .expect("load");
    assert_eq!(j.status, JourneyStatus::Draft);
    assert_eq!((j.tag.as_deref(), j.dept_tag.as_deref()), (None, None));
    assert_eq!(j.draft_rev, 2);
    assert_eq!(j.updated_by, AUTHOR);
    assert_eq!(j.source_run, None);
    let stage = j
        .stages
        .iter()
        .find(|s| s.key == *"b8t2b_stage")
        .expect("the stage");
    assert_eq!(stage.layer.as_deref(), Some(ind.as_str()));
    assert_eq!(stage.description, "the stage");
    let act = j
        .activities
        .iter()
        .find(|a| a.key == "b8t2b_stage:look_up")
        .expect("the activity");
    assert_eq!(act.layer.as_deref(), Some(dept.as_str()));
    assert_eq!(act.trigger, "asks");
}

#[tokio::test]
async fn status_partial_after_untagged_revision() {
    let e = common::env().await;
    let (ind, dept) = pair(&e.owner, "b8t2b_part", "b8t2b_dept").await;
    write(
        &e.state.db,
        &ind,
        Kind::JourneyStage,
        "b8t2b_stage",
        Op::Add,
        json!({"label": "Stage"}),
    )
    .await;
    write(
        &e.state.db,
        &dept,
        Kind::Activity,
        "b8t2b_stage:look_up",
        Op::Add,
        json!({"label": "Look it up"}),
    )
    .await;
    tag(&e.owner, &ind, 1).await;
    tag(&e.owner, &dept, 1).await;
    let tagged = load(&e.state.db, "b8t2b_part", "b8t2b_dept")
        .await
        .expect("load");
    assert_eq!(tagged.status, JourneyStatus::Tagged);
    assert_eq!(tagged.tag.as_deref(), Some("industry/b8t2b_part@v1"));
    assert_eq!(
        tagged.dept_tag.as_deref(),
        Some("industry_dept/b8t2b_part/b8t2b_dept@v1")
    );
    assert_eq!(tagged.draft_rev, 0);

    write(
        &e.state.db,
        &ind,
        Kind::JourneyStage,
        "b8t2b_stage",
        Op::Patch,
        json!({"label": "Stage, renamed"}),
    )
    .await;
    let j = load(&e.state.db, "b8t2b_part", "b8t2b_dept")
        .await
        .expect("load");
    assert_eq!(j.status, JourneyStatus::Partial);
    assert_eq!(j.draft_rev, 1);
    // the tags stay the latest tags; the grid shows the untagged label
    assert_eq!(j.tag.as_deref(), Some("industry/b8t2b_part@v1"));
    let stage = j
        .stages
        .iter()
        .find(|s| s.key == *"b8t2b_stage")
        .expect("the stage");
    assert_eq!(stage.label, "Stage, renamed");
}

#[tokio::test]
async fn department_fold_overrides_base_dept_label() {
    let e = common::env().await;
    // two activities in a base_dept/customer_service layer (the archetype's department layer the fold extends)
    sqlx::query(
        "INSERT INTO layer (key, scope, department, family) \
         VALUES ('base_dept/customer_service', 'base_dept', 'customer_service', 'base_dept/customer_service')",
    )
    .execute(&e.owner)
    .await
    .expect("insert the base_dept layer");
    for (key, label) in [
        ("b8t2b_stage:ask", "Ask a question"),
        ("b8t2b_stage:complain", "Make a complaint"),
    ] {
        write(
            &e.state.db,
            "base_dept/customer_service",
            Kind::Activity,
            key,
            Op::Add,
            json!({"label": label, "actor": "customer", "trigger": "asks"}),
        )
        .await;
    }
    let keys: Vec<(String, String)> = sqlx::query_as(
        "SELECT key, fields ->> 'label' FROM layer_head \
          WHERE layer_key = 'base_dept/customer_service' AND kind = 'activity' AND op <> 'tombstone' \
          ORDER BY key LIMIT 2",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(keys.len(), 2, "the base_dept layer holds two activities");
    let ((over, over_label), (kept, kept_label)) = (keys[0].clone(), keys[1].clone());

    let ind = industry_layer(&e.owner, "b8t2b_fold").await;
    write(
        &e.state.db,
        &ind,
        Kind::JourneyStage,
        "b8t2b_stage",
        Op::Add,
        json!({"label": "Stage", "description": "the stage"}),
    )
    .await;
    let dept = dept_layer(
        &e.owner,
        "b8t2b_fold",
        "customer_service",
        Some("customer_service"),
    )
    .await;
    let before = load(&e.state.db, "b8t2b_fold", "customer_service")
        .await
        .expect("load");
    let find = |j: &IndustryJourney, k: &str| {
        j.activities
            .iter()
            .find(|a| a.key == k)
            .map(|a| (a.label.clone(), a.layer.clone()))
    };
    assert_eq!(
        find(&before, &over),
        Some((
            over_label.clone(),
            Some("base_dept/customer_service".into())
        ))
    );

    write(
        &e.state.db,
        &dept,
        Kind::Activity,
        &over,
        Op::Patch,
        json!({"label": "Label set by the department"}),
    )
    .await;
    let after = load(&e.state.db, "b8t2b_fold", "customer_service")
        .await
        .expect("load");
    assert_eq!(
        find(&after, &over),
        Some(("Label set by the department".into(), Some(dept.clone())))
    );
    assert_eq!(
        find(&after, &kept),
        Some((kept_label, Some("base_dept/customer_service".into()))),
        "the other base_dept activity is untouched"
    );
}

#[tokio::test]
async fn industries_lists_not_started() {
    let e = common::env().await;
    crate::b8::support::content(&e).await;
    industry_layer(&e.owner, "b8t2b_empty").await;
    industry_layer(&e.owner, "b8t2b_nodata").await;
    dept_layer(&e.owner, "b8t2b_nodata", "b8t2b_dept", None).await;

    let mut tx = begin_platform(&e.state.db).await.expect("begin_platform");
    let all = grid::industries(&mut tx).await.expect("industries");
    tx.rollback().await.expect("rollback");

    let keys: Vec<&str> = all.iter().map(|i| i.industry.as_str()).collect();
    let mut sorted = keys.clone();
    sorted.sort_unstable();
    assert_eq!(keys, sorted, "by industry key");

    let by = |k: &str| {
        all.iter()
            .find(|i| i.industry == k)
            .unwrap_or_else(|| panic!("{k}"))
    };
    let empty = by("b8t2b_empty");
    assert_eq!(empty.status, SummaryStatus::NotStarted);
    assert!(empty.departments.is_empty());
    assert_eq!(
        (empty.activities, empty.red, empty.tag.as_deref()),
        (0, 0, None)
    );

    let nodata = by("b8t2b_nodata");
    assert_eq!(nodata.status, SummaryStatus::NotStarted);
    assert_eq!(nodata.departments.len(), 1);
    assert_eq!(nodata.departments[0].department, "b8t2b_dept");
    assert_eq!(nodata.departments[0].status, SummaryStatus::NotStarted);

    let retail = by("retail");
    assert_ne!(retail.status, SummaryStatus::NotStarted);
    assert!(retail.tag.is_some() && retail.activities > 0);
    let cs = retail
        .departments
        .iter()
        .find(|d| d.department == "customer_service")
        .expect("retail customer_service");
    assert_ne!(cs.status, SummaryStatus::NotStarted);
    assert!(cs.activities > 0);
}

#[tokio::test]
async fn rule4_stage_without_activity_red_unless_out_of_scope() {
    let e = common::env().await;
    let (ind, dept) = pair(&e.owner, "b8t2b_scope", "b8t2b_dept").await;
    write(
        &e.state.db,
        &ind,
        Kind::JourneyStage,
        "b8t2b_gate",
        Op::Add,
        json!({"label": "Gate"}),
    )
    .await;
    let of_gate = |j: &IndustryJourney| {
        j.findings
            .iter()
            .filter(|f| f.rule == 4 && f.stage.as_deref() == Some("b8t2b_gate"))
            .cloned()
            .collect::<Vec<_>>()
    };

    let j = load(&e.state.db, "b8t2b_scope", "b8t2b_dept")
        .await
        .expect("load");
    let stage = j
        .stages
        .iter()
        .find(|s| s.key == *"b8t2b_gate")
        .expect("the stage");
    assert!(!stage.out_of_scope);
    let f = of_gate(&j);
    assert!(
        f.iter()
            .any(|x| x.severity == Severity::Red && x.message.contains("has no activity")),
        "{f:?}"
    );

    write(
        &e.state.db,
        &dept,
        Kind::StageScope,
        "b8t2b_gate",
        Op::Add,
        json!({"out_of_scope": true}),
    )
    .await;
    let j = load(&e.state.db, "b8t2b_scope", "b8t2b_dept")
        .await
        .expect("load");
    let stage = j
        .stages
        .iter()
        .find(|s| s.key == *"b8t2b_gate")
        .expect("the stage");
    assert!(stage.out_of_scope, "the department's stage_scope row");
    assert!(of_gate(&j).is_empty(), "{:?}", of_gate(&j));
}

#[tokio::test]
async fn load_at_ignores_untagged_edits() {
    let e = common::env().await;
    let (ind, dept) = pair(&e.owner, "b8t2b_pin", "b8t2b_dept").await;
    write(
        &e.state.db,
        &ind,
        Kind::JourneyStage,
        "b8t2b_stage",
        Op::Add,
        json!({"label": "Stage"}),
    )
    .await;
    write(
        &e.state.db,
        &dept,
        Kind::Activity,
        "b8t2b_stage:look_up",
        Op::Add,
        json!({"label": "Look it up"}),
    )
    .await;
    tag(&e.owner, &ind, 1).await;
    tag(&e.owner, &dept, 1).await;
    let base: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM layer_tag WHERE family = 'base' AND NOT provisional",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let pin = |layer: &str, version: i32| TemplatePin {
        layer: layer.to_owned(),
        version,
    };
    let pins = [pin("base", base), pin(&ind, 1), pin(&dept, 1)];
    let sel = Selection {
        industry: "b8t2b_pin".into(),
        departments: vec![dept.clone()],
        ..Selection::default()
    };
    let at = async || {
        let mut tx = begin_platform(&e.state.db).await.expect("begin_platform");
        let r = grid::load_at(&mut tx, &pins, &sel, "b8t2b_dept").await;
        tx.rollback().await.expect("rollback");
        r.expect("load_at")
    };

    let pinned = at().await;
    assert_eq!(pinned.status, JourneyStatus::Tagged);
    assert_eq!(pinned.tag.as_deref(), Some("industry/b8t2b_pin@v1"));
    assert_eq!(pinned.draft_rev, 0);
    assert!(
        pinned
            .activities
            .iter()
            .any(|a| a.key == "b8t2b_stage:look_up")
    );

    write(
        &e.state.db,
        &dept,
        Kind::Activity,
        "b8t2b_stage:newer",
        Op::Add,
        json!({"label": "Added after the tag"}),
    )
    .await;
    let still = at().await;
    assert!(
        !still
            .activities
            .iter()
            .any(|a| a.key == "b8t2b_stage:newer"),
        "a pinned grid never moves"
    );
    let head = load(&e.state.db, "b8t2b_pin", "b8t2b_dept")
        .await
        .expect("load");
    assert!(head.activities.iter().any(|a| a.key == "b8t2b_stage:newer"));
    assert_eq!(head.status, JourneyStatus::Partial);
}

#[tokio::test]
async fn unknown_industry_or_department_is_journey_not_found() {
    let e = common::env().await;
    assert_eq!(
        code(load(&e.state.db, "no_such_industry", "customer_service").await),
        (404, "journey_not_found")
    );
    assert_eq!(
        code(load(&e.state.db, "retail", "no_such_department").await),
        (404, "journey_not_found")
    );
}
