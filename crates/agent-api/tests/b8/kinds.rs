//! B8 phase 2 T1 (migration 0191): the layer-row kind CHECK takes `journey_channel`, `stage_scope` and
//! `use_case_card` and still refuses everything else; the scope rules (agent-model) put each kind where the plan's
//! kinds table says and keep `use_case_card` out of the base layer. Real Postgres 18; rows go in through the owner
//! pool (the CHECK) and through `write_revision` (the scope rule, the field schema).
use agent_api::audit::Actor;
use agent_api::error::ApiError;
use agent_api::platform::db::rows;
use agent_api::platform_db::begin_platform;
use agent_model::kinds::{Kind, Op};
use serde_json::{Value, json};
use sqlx::PgPool;
use uuid::Uuid;

use crate::common;

const AUTHOR: &str = "b8-t1-author";

fn actor() -> Actor {
    Actor::system("service:agent-api-b8-t1-test")
}

fn hash(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

async fn job_layer(db: &PgPool, job: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO layer (key, scope, job, family) VALUES ('job/' || $1, 'job', $1, 'job/' || $1) RETURNING id",
    )
    .bind(job)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn insert_row(db: &PgPool, layer: Uuid, kind: &str, key: &str) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO layer_row (layer_id, kind, key, op, fields, rev, is_current, author, fields_hash) \
         VALUES ($1, $2, $3, 'add', $4, 1, true, 'test', $5) RETURNING id",
    )
    .bind(layer)
    .bind(kind)
    .bind(key)
    .bind(json!({ "label": key }))
    .bind(hash('a'))
    .fetch_one(db)
    .await
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

async fn dept_layer(db: &PgPool, industry: &str, dept: &str) -> String {
    let key = format!("industry_dept/{industry}/{dept}");
    sqlx::query(
        "INSERT INTO layer (key, scope, industry, department, family) \
         VALUES ($1, 'industry_dept', $2, $3, $1)",
    )
    .bind(&key)
    .bind(industry)
    .bind(dept)
    .execute(db)
    .await
    .expect("insert an industry_dept layer");
    key
}

async fn write(
    db: &PgPool,
    layer: &str,
    kind: Kind,
    key: &str,
    fields: Value,
) -> Result<rows::Row, ApiError> {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    let r =
        rows::write_revision(&mut tx, &actor(), layer, kind, key, Op::Add, fields, AUTHOR).await;
    if r.is_ok() {
        tx.commit().await.expect("commit the revision");
    }
    r
}

fn refusal<T: std::fmt::Debug>(r: Result<T, ApiError>) -> (u16, &'static str) {
    let (status, code) = r.expect_err("expected a refusal").status_and_code();
    (status.as_u16(), code)
}

#[tokio::test]
async fn layer_row_accepts_new_kinds() {
    let e = common::env().await;
    let l = job_layer(&e.owner, "b8t1_kinds").await;
    for kind in ["journey_channel", "stage_scope", "use_case_card"] {
        insert_row(&e.owner, l, kind, "k")
            .await
            .unwrap_or_else(|err| panic!("{kind}: {err}"));
    }
    let err = insert_row(&e.owner, l, "bogus", "k2").await.unwrap_err();
    assert_eq!(common::sqlstate(&err).as_deref(), Some("23514"), "{err}");

    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
          WHERE conname = 'layer_row_kind_check' AND conrelid = 'layer_row'::regclass",
    )
    .fetch_one(&e.owner)
    .await
    .expect("the named CHECK exists");
    assert_eq!(
        def.matches("::text").count(),
        Kind::ALL.len(),
        "the CHECK lists exactly the agent-model kinds: {def}"
    );
    for kind in Kind::ALL {
        assert!(def.contains(&format!("'{kind}'")), "{kind} in {def}");
    }
}

#[tokio::test]
async fn new_kinds_are_written_where_the_scope_rules_say() {
    let e = common::env().await;
    let db = &e.state.db;
    let industry = industry_layer(&e.owner, "b8t1ind").await;
    let dept = dept_layer(&e.owner, "b8t1ind", "b8t1dept").await;

    let channel = json!({"label": "Chat", "status": "v1", "note": "web chat"});
    write(db, &industry, Kind::JourneyChannel, "chat", channel)
        .await
        .expect("a channel at industry");
    let scope = json!({"label": "Billing", "out_of_scope": true});
    write(db, &dept, Kind::StageScope, "billing", scope)
        .await
        .expect("a stage scope at industry_dept");
    let card = json!({"summary": "Start a return", "examples": ["return my shoes"]});
    write(db, &dept, Kind::UseCaseCard, "support:start_return", card)
        .await
        .expect("a card at industry_dept");

    // the field schema runs on the API write path
    let bad = write(
        db,
        &dept,
        Kind::StageScope,
        "support",
        json!({"label": "x", "stage": "support"}),
    )
    .await;
    assert_eq!(refusal(bad), (422, "invalid_row"));
    // a channel lives in base/industry only
    let wrong = write(
        db,
        &dept,
        Kind::JourneyChannel,
        "email",
        json!({"label": "Email"}),
    )
    .await;
    assert_eq!(refusal(wrong), (422, "kind_not_allowed_in_layer"));
}

#[tokio::test]
async fn use_case_card_rejected_at_base() {
    let e = common::env().await;
    let db = &e.state.db;
    // the seeded base layer: the scope rule refuses before anything is written
    let refused = write(
        db,
        "base",
        Kind::UseCaseCard,
        "support:start_return",
        json!({"summary": "x"}),
    )
    .await;
    assert_eq!(refusal(refused), (422, "kind_not_allowed_in_layer"));
    // a card lives in the job layer too (the tenant-facing refinement is a separate table, T7a)
    let job = format!("job/{}", "b8t1_card");
    sqlx::query(
        "INSERT INTO layer (key, scope, job, family, label, template_selection) \
         VALUES ($1, 'job', 'b8t1_card', $1, 'Job', $2)",
    )
    .bind(&job)
    .bind(json!({"industry": "b8t1_card_ind",
                 "departments": ["industry_dept/b8t1_card_ind/b8t1_card_dept"], "profile": null}))
    .execute(&e.owner)
    .await
    .expect("insert a job layer");
    write(
        db,
        &job,
        Kind::UseCaseCard,
        "support:start_return",
        json!({"summary": "x"}),
    )
    .await
    .expect("a card at job");
}
