//! B6a T8: the tag → pin contract (crate::platform::templates) over the platform offering the tests author
//! themselves (`common::platform::offering`): pinned compile refusals, the `job_template_tag` projection
//! `tag_family` runs, and job pins resolving to layer tags. Real Postgres 18 (one container for this binary, a
//! fresh database per test).
use crate::common;

use agent_api::audit::Actor;
use agent_api::error::ApiError;
use agent_api::platform::db::{rows, tags};
use agent_api::platform::hooks::NoHooks;
use agent_api::platform::templates::{self, JobTemplateTag, TemplatePin};
use agent_api::platform_db::begin_platform;
use agent_api::release::compile::{LayerCompiler, TemplateCompiler, db_options};
use agent_model::compile::Selection;
use agent_model::kinds::{Kind, Op};
use axum::http::StatusCode;
use common::{insert_tenant, json, send, super_admin};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

const CCA: &str = "job/customer_care_agent";

fn actor() -> Actor {
    Actor::system("service:agent-api-t8-test")
}

/// (status, code) of a refusal.
fn refusal<T: std::fmt::Debug>(r: Result<T, ApiError>) -> (u16, &'static str) {
    let e = r.expect_err("expected a refusal");
    let (status, code) = e.status_and_code();
    (status.as_u16(), code)
}

fn pin(layer: &str, version: i32) -> TemplatePin {
    TemplatePin {
        layer: layer.to_owned(),
        version,
    }
}

async fn job_templates(db: &PgPool) -> Vec<JobTemplateTag> {
    let mut c = db.acquire().await.expect("a connection");
    templates::job_templates(&mut c)
        .await
        .expect("job templates")
}

/// `tag_family` in its own platform transaction, committed.
async fn cut(db: &PgPool, family: &str) -> tags::TemplateTag {
    cut_with(db, family, false).await
}

async fn cut_with(db: &PgPool, family: &str, provisional: bool) -> tags::TemplateTag {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    let t = tags::tag_family(
        &mut tx,
        &actor(),
        family,
        "t8 tag",
        "t8-owner",
        &NoHooks,
        provisional,
    )
    .await
    .expect("tag the family");
    tx.commit().await.expect("commit the tag");
    t
}

async fn profile_choice(db: &PgPool, key: &str) -> Option<(String, Option<String>)> {
    let mut c = db.acquire().await.unwrap();
    db_options(&mut c)
        .await
        .unwrap()
        .profiles
        .into_iter()
        .find(|p| p.key == key)
        .map(|p| (p.label, p.note))
}

#[tokio::test]
async fn profile_options_follow_published_versions_and_tombstones() {
    let e = common::env().await;
    let db = &e.state.db;
    let key = "tjd_published";
    let first = serde_json::json!({
        "label": "First published profile", "industry": "retail", "description": "First version",
        "selection": {"order_type": ["discrete"]}
    });
    write(db, "base", Kind::Profile, key, Op::Add, first).await;
    assert_eq!(profile_choice(db, key).await, None, "a new draft is hidden");
    cut_with(db, "base", true).await;
    assert_eq!(profile_choice(db, key).await, None, "v0 is still a draft");
    cut(db, "base").await;
    let published = Some((
        "First published profile".to_owned(),
        Some("First version".to_owned()),
    ));
    assert_eq!(profile_choice(db, key).await, published);

    let next = serde_json::json!({
        "label": "Second published profile", "industry": "retail", "description": "Second version",
        "selection": {"order_type": ["continuous"]}
    });
    write(db, "base", Kind::Profile, key, Op::Patch, next).await;
    assert_eq!(
        profile_choice(db, key).await,
        published,
        "draft edits do not replace published options"
    );
    cut_with(db, "base", true).await;
    assert_eq!(
        profile_choice(db, key).await,
        published,
        "a provisional edit is not published"
    );
    cut(db, "base").await;
    let published = Some((
        "Second published profile".to_owned(),
        Some("Second version".to_owned()),
    ));
    assert_eq!(profile_choice(db, key).await, published);

    write(
        db,
        "base",
        Kind::Profile,
        key,
        Op::Tombstone,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        profile_choice(db, key).await,
        published,
        "a draft deletion is not published"
    );
    cut(db, "base").await;
    assert_eq!(
        profile_choice(db, key).await,
        None,
        "the published tombstone removes the profile"
    );
    write(
        db,
        "base",
        Kind::Entity,
        "LaterThing",
        Op::Add,
        serde_json::json!({"label": "Later thing"}),
    )
    .await;
    cut(db, "base").await;
    assert_eq!(
        profile_choice(db, key).await,
        None,
        "later tags never resurrect an older profile"
    );
}

#[tokio::test]
async fn release_profile_values_follow_exact_pins_after_edits_and_deletion() {
    let e = common::env().await;
    let off = common::platform::offering(&e).await;
    let db = &e.state.db;
    let mut c = db.acquire().await.unwrap();
    let resolved = templates::resolve_template(&mut c, off.job_layer, off.version)
        .await
        .unwrap();
    let compiler = LayerCompiler::new();
    let before = compiler
        .compile(&mut c, &resolved.default_selection, &resolved.pins)
        .await
        .unwrap();
    assert_eq!(before.bundle.profile_values["order_type"], "discrete");

    write(db, "base", Kind::Profile, off.profile, Op::Patch, serde_json::json!({
        "label": "Continuous orders", "industry": "retail", "selection": {"order_type": ["continuous"]}
    })).await;
    let draft = compiler
        .compile(&mut c, &resolved.default_selection, &resolved.pins)
        .await
        .unwrap();
    assert_eq!(
        draft, before,
        "an untagged edit cannot change a pinned bundle"
    );
    let published = cut(db, "base").await;
    let after = LayerCompiler::new()
        .compile(&mut c, &resolved.default_selection, &resolved.pins)
        .await
        .unwrap();
    assert_eq!(
        after, before,
        "a newer published profile cannot change an older release, even without a cache"
    );

    let mut updated_pins = resolved.pins.clone();
    updated_pins
        .iter_mut()
        .find(|p| p.layer == "base")
        .unwrap()
        .version = published.version;
    let updated = compiler
        .compile(&mut c, &resolved.default_selection, &updated_pins)
        .await
        .unwrap();
    assert_eq!(updated.bundle.profile_values["order_type"], "continuous");
    assert_ne!(
        updated.bundle_hash, before.bundle_hash,
        "adopting the new pin changes the bundle"
    );

    write(
        db,
        "base",
        Kind::Profile,
        off.profile,
        Op::Tombstone,
        serde_json::json!({}),
    )
    .await;
    cut(db, "base").await;
    assert_eq!(profile_choice(db, off.profile).await, None);
    let old = compiler
        .compile(&mut c, &resolved.default_selection, &resolved.pins)
        .await
        .unwrap();
    assert_eq!(
        old, before,
        "a published deletion cannot erase an older release's profile values"
    );
    let still_published = compiler
        .compile(&mut c, &resolved.default_selection, &updated_pins)
        .await
        .unwrap();
    assert_eq!(still_published, updated);
}

/// `write_revision` in its own platform transaction, committed.
async fn write(db: &PgPool, layer: &str, kind: Kind, key: &str, op: Op, fields: Value) {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    rows::write_revision(&mut tx, &actor(), layer, kind, key, op, fields, "t8-author")
        .await
        .expect("write the revision");
    tx.commit().await.expect("commit the revision");
}

#[tokio::test]
async fn compile_pinned_unknown_tag_404() {
    let e = common::env().await;
    let mut c = e.state.db.acquire().await.unwrap();
    assert_eq!(
        refusal(
            templates::compile_pinned(&mut c, &[pin("base", 99)], &Selection::default(), None)
                .await
        ),
        (404, "tag_not_found")
    );
    assert_eq!(
        refusal(
            templates::compile_pinned(
                &mut c,
                &[pin("base", 3), pin("job/no_such_job", 1)],
                &Selection::default(),
                None
            )
            .await
        ),
        (404, "tag_not_found")
    );
}

#[tokio::test]
async fn tag_upserts_job_template_tag_projection() {
    let e = common::env().await;
    let off = common::platform::offering(&e).await;
    let db = &e.state.db;
    write(
        db,
        CCA,
        Kind::JobSetting,
        "stance",
        Op::Add,
        serde_json::json!({"label": "Stance", "summary": "warm, concise, exact"}),
    )
    .await;
    let t = cut(db, CCA).await;
    assert_eq!(
        t.version,
        off.version + 1,
        "the offering cut v1..v{}",
        off.version
    );

    let got = job_templates(db).await;
    let row = |v: i32| {
        got.iter()
            .find(|t| t.layer == CCA && t.version == v)
            .unwrap_or_else(|| panic!("{CCA}@v{v} is projected: {got:#?}"))
    };
    let newest = row(t.version);
    assert_eq!(newest.key, "customer_care_agent");
    assert_eq!(newest.industry.as_deref(), Some("retail"));
    assert_eq!(newest.status, "tagged");
    let mut want_pins: Vec<TemplatePin> = serde_json::from_value(t.stack.clone()).unwrap();
    want_pins.push(pin(CCA, t.version));
    assert_eq!(newest.pins, want_pins);
    assert_eq!(
        row(off.version).label,
        format!("{} (previous tag)", newest.label),
        "the previous tag's label gets the suffix"
    );

    // a department a job head specialises is no job template of its own
    write(
        db,
        off.department,
        Kind::Entity,
        "Gizmo",
        Op::Add,
        serde_json::json!({"label": "Gizmo", "synonyms": ["thing"]}),
    )
    .await;
    cut(db, off.department).await;
    assert!(
        job_templates(db)
            .await
            .iter()
            .all(|t| t.layer != off.department),
        "the department is specialised by the job head"
    );
}

#[tokio::test]
async fn job_pins_resolve_to_tags() {
    let e = common::env().await;
    let off = common::platform::offering(&e).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "T8 pins").await;
    let (s, body) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/tenants/{t}/jobs"),
            Some(&super_admin()),
            serde_json::json!({"template": off.template(), "key": "pins_cca", "label": "pins_cca"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
    let pins: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs j CROSS JOIN LATERAL jsonb_array_elements(j.pins) p WHERE j.tenant_id = $1",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(pins >= 1, "the job pins its template's stack");
    let dangling: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM jobs j CROSS JOIN LATERAL jsonb_array_elements(j.pins) p \
          WHERE NOT EXISTS (SELECT 1 FROM layer_tag lt \
                             WHERE lt.family = p ->> 'layer' AND lt.version = (p ->> 'version')::integer)",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(dangling, 0, "every jobs.pins entry is a layer_tag");
}
