//! B6a T7: the platform store db layer (crate::platform::db) over migrations 0041–0043: revision writes and their
//! history, the family lock, validation refusals, family tags (membership snapshot, stack, content hash,
//! `nothing_to_tag`, `validation_red`), the untagged drafts and the override targets, one platform audit row per
//! write. Real Postgres 18 (one container for this binary, a fresh database per test). Fixtures create their own
//! layers (`job/t7_*`, `industry/t7*`) through the owner pool on an empty platform (`env_bare`: no boot seeder, no
//! test offering), so the global audit and tag counts are the fixtures' own.
use crate::common;

use agent_api::audit::Actor;
use agent_api::error::ApiError;
use agent_api::platform::db::{TagMember, layers, rows, tags};
use agent_api::platform::hooks::{BoxFuture, GeneratorGuard, NoHooks};
use agent_api::platform_db::begin_platform;
use agent_model::kinds::{Kind, Op};
use agent_model::tags::{draft_op, member_keys};
use serde_json::{Map, Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

const AUTHOR: &str = "t7-author";

fn actor() -> Actor {
    Actor::system("service:agent-api-t7-test")
}

/// A job head layer `job/<job>`; its key. Without a `selection` the head names its own industry and department
/// (`<job>_ind`, `industry_dept/<job>_ind/<job>_dept`): D106 refuses to register a template without both.
async fn job_layer(owner: &PgPool, job: &str, selection: Option<Value>) -> String {
    let key = format!("job/{job}");
    let selection = selection.unwrap_or_else(|| {
        json!({"industry": format!("{job}_ind"),
               "departments": [format!("industry_dept/{job}_ind/{job}_dept")], "profile": null})
    });
    sqlx::query(
        "INSERT INTO layer (key, scope, job, family, label, template_selection) \
         VALUES ($1, 'job', $2, $1, $3, $4)",
    )
    .bind(&key)
    .bind(job)
    .bind(format!("Job {job}"))
    .bind(selection)
    .execute(owner)
    .await
    .expect("insert a job layer");
    key
}

/// A job×dimension layer `job/<job>/<k>=<v>` of the family `job/<job>` (insert the head first); its key.
async fn dimension_layer(owner: &PgPool, job: &str, k: &str, v: &str) -> String {
    let key = format!("job/{job}/{k}={v}");
    sqlx::query(
        "INSERT INTO layer (key, scope, job, dimension_key, dimension_value, family) \
         VALUES ($1, 'job', $2, $3, $4, $5)",
    )
    .bind(&key)
    .bind(job)
    .bind(k)
    .bind(v)
    .bind(format!("job/{job}"))
    .execute(owner)
    .await
    .expect("insert a dimension layer");
    key
}

/// An industry head `industry/<i>`; its key.
async fn industry_layer(owner: &PgPool, industry: &str) -> String {
    let key = format!("industry/{industry}");
    sqlx::query("INSERT INTO layer (key, scope, industry, family) VALUES ($1, 'industry', $2, $1)")
        .bind(&key)
        .bind(industry)
        .execute(owner)
        .await
        .expect("insert an industry layer");
    key
}

/// An industry_dept head `industry_dept/<i>/<d>`; its key.
async fn dept_layer(owner: &PgPool, industry: &str, dept: &str) -> String {
    let key = format!("industry_dept/{industry}/{dept}");
    sqlx::query(
        "INSERT INTO layer (key, scope, industry, department, family) \
         VALUES ($1, 'industry_dept', $2, $3, $1)",
    )
    .bind(&key)
    .bind(industry)
    .bind(dept)
    .execute(owner)
    .await
    .expect("insert an industry_dept layer");
    key
}

/// `write_revision` in its own platform transaction, committed when it succeeds.
async fn write(
    db: &PgPool,
    layer: &str,
    kind: Kind,
    key: &str,
    op: Op,
    fields: Value,
) -> Result<rows::Row, ApiError> {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    let r = rows::write_revision(&mut tx, &actor(), layer, kind, key, op, fields, AUTHOR).await;
    if r.is_ok() {
        tx.commit().await.expect("commit the revision");
    }
    r
}

/// `tag_family` in its own platform transaction, committed when it succeeds.
async fn cut(
    db: &PgPool,
    family: &str,
    guard: &dyn GeneratorGuard,
) -> Result<tags::TemplateTag, ApiError> {
    let mut tx = begin_platform(db).await.expect("begin_platform");
    let r = tags::tag_family(
        &mut tx,
        &actor(),
        family,
        "t7 tag",
        "t7-owner",
        guard,
        false,
    )
    .await;
    if r.is_ok() {
        tx.commit().await.expect("commit the tag");
    }
    r
}

/// A platform transaction for reads (dropped = rolled back).
async fn reader(db: &PgPool) -> Transaction<'static, Postgres> {
    begin_platform(db).await.expect("begin_platform")
}

fn entity(label: &str) -> Value {
    json!({"label": label, "synonyms": ["thing"]})
}

/// (status, code) of a refusal.
fn refusal<T: std::fmt::Debug>(r: Result<T, ApiError>) -> (u16, &'static str) {
    let e = r.expect_err("expected a refusal");
    let (status, code) = e.status_and_code();
    (status.as_u16(), code)
}

/// The extra fields of a `Detailed` refusal.
fn extra<T: std::fmt::Debug>(r: Result<T, ApiError>) -> Map<String, Value> {
    match r.expect_err("expected a refusal") {
        ApiError::Detailed { extra, .. } => extra,
        other => panic!("expected a detailed refusal, got {other:?}"),
    }
}

async fn count(owner: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql)
        .fetch_one(owner)
        .await
        .expect("count")
}

#[tokio::test]
async fn revision_history_kept() {
    let e = common::env_bare().await;
    let db = &e.state.db;
    let layer = job_layer(&e.owner, "t7_hist", None).await;

    let r1 = write(db, &layer, Kind::Entity, "Order", Op::Add, entity("Order"))
        .await
        .expect("rev 1");
    assert_eq!((r1.rev, r1.is_current, r1.op.as_str()), (1, true, "add"));
    assert_eq!(r1.author, AUTHOR);
    // API writes recompute the summary; the hash covers the stored fields
    assert_eq!(r1.fields["summary"], "Order · thing");
    assert_eq!(
        r1.fields_hash,
        agent_model::canonical::fields_hash(&r1.fields)
    );

    let r2 = write(
        db,
        &layer,
        Kind::Entity,
        "Order",
        Op::Add,
        entity("Purchase"),
    )
    .await
    .expect("rev 2");
    let r3 = write(db, &layer, Kind::Entity, "Order", Op::Tombstone, json!({}))
        .await
        .expect("rev 3");
    assert_eq!((r2.rev, r3.rev, r3.op.as_str()), (2, 3, "tombstone"));
    assert_eq!(r3.fields, json!({}), "a tombstone carries no summary");

    let mut tx = reader(db).await;
    let l = layers::layer_by_key(&mut tx, &layer).await.unwrap();
    let revs = rows::revisions(&mut tx, l.id, Kind::Entity, "Order")
        .await
        .unwrap();
    assert_eq!(
        revs.iter()
            .map(|r| (r.rev, r.is_current))
            .collect::<Vec<_>>(),
        vec![(1, false), (2, false), (3, true)]
    );
    assert_eq!(revs[0].id, r1.id);
    assert_eq!(revs[0].fields, r1.fields, "history rows never change");
    assert_eq!(revs[0].fields_hash, r1.fields_hash);
    let head = rows::head_rows(&mut tx, l.id, None).await.unwrap();
    assert_eq!(head.len(), 1);
    assert_eq!(head[0].id, r3.id);
    let all = rows::layer_rows(&mut tx, l.id, Some(Kind::Entity), true)
        .await
        .unwrap();
    assert_eq!(all.len(), 3);
    assert!(
        rows::head_rows(&mut tx, l.id, Some(Kind::Attribute))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(rows::row_by_id(&mut tx, r1.id).await.unwrap().rev, 1);
    assert_eq!(
        refusal(rows::row_by_id(&mut tx, Uuid::new_v4()).await),
        (404, "row_not_found")
    );
    assert_eq!(
        refusal(rows::head_rows(&mut tx, Uuid::new_v4(), None).await),
        (404, "layer_not_found")
    );
    drop(tx);

    assert_eq!(
        refusal(
            write(
                db,
                "job/t7_missing",
                Kind::Entity,
                "Order",
                Op::Add,
                entity("O")
            )
            .await
        ),
        (404, "layer_not_found")
    );
}

#[tokio::test]
async fn concurrent_writes_one_current() {
    let e = common::env_bare().await;
    let layer = job_layer(&e.owner, "t7_conc", None).await;
    write(
        &e.state.db,
        &layer,
        Kind::Entity,
        "Order",
        Op::Add,
        entity("Order"),
    )
    .await
    .expect("rev 1");

    // two writers of the same key on two connections: the family lock serialises them
    let spawn = |label: &'static str| {
        let db = e.state.db.clone();
        let layer = layer.clone();
        tokio::spawn(async move {
            write(&db, &layer, Kind::Entity, "Order", Op::Add, entity(label))
                .await
                .map(|r| r.rev)
        })
    };
    let a = spawn("Order A");
    let b = spawn("Order B");
    let mut revs = vec![
        a.await.expect("join a").expect("write a"),
        b.await.expect("join b").expect("write b"),
    ];
    revs.sort_unstable();
    assert_eq!(revs, vec![2, 3]);

    let current = count(
        &e.owner,
        "SELECT count(*) FROM layer_row r JOIN layer l ON l.id = r.layer_id \
          WHERE l.key = 'job/t7_conc' AND r.key = 'Order' AND r.is_current",
    )
    .await;
    assert_eq!(current, 1, "exactly one current revision");
    let (rev, is_current): (i32, bool) = sqlx::query_as(
        "SELECT r.rev, r.is_current FROM layer_row r JOIN layer l ON l.id = r.layer_id \
          WHERE l.key = 'job/t7_conc' AND r.key = 'Order' ORDER BY r.rev DESC LIMIT 1",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!((rev, is_current), (3, true));
}

#[tokio::test]
async fn tag_freezes_members() {
    let e = common::env_bare().await;
    let db = &e.state.db;
    let family = job_layer(&e.owner, "t7_tag", None).await;
    let email = dimension_layer(&e.owner, "t7_tag", "channel", "email").await;
    write(db, &family, Kind::Entity, "Order", Op::Add, entity("Order"))
        .await
        .unwrap();
    write(db, &family, Kind::Entity, "Party", Op::Add, entity("Party"))
        .await
        .unwrap();
    write(
        db,
        &email,
        Kind::Entity,
        "Order",
        Op::Patch,
        json!({"hidden": true}),
    )
    .await
    .unwrap();

    let mut tx = reader(db).await;
    let before = layers::layer_summaries(&mut tx, Some(family.as_str()))
        .await
        .unwrap();
    assert_eq!(
        before
            .iter()
            .map(|s| (s.layer_key.as_str(), s.untagged, s.latest_tag))
            .collect::<Vec<_>>(),
        vec![(family.as_str(), 2, None), (email.as_str(), 1, None)]
    );
    assert_eq!((before[0].adds, before[1].patches), (2, 1));
    drop(tx);

    // a sublayer is not a family
    assert_eq!(
        refusal(cut(db, &email, &NoHooks).await),
        (404, "layer_not_found")
    );

    let v1 = cut(db, &family, &NoHooks).await.expect("tag v1");
    assert_eq!((v1.version, v1.provisional), (1, false));
    assert_eq!(v1.tagged_by, "t7-owner");
    let mut tx = reader(db).await;
    let members_v1 = tags::tag_members(&mut tx, &family, 1).await.unwrap();
    assert_eq!(
        members_v1.len(),
        3,
        "every current row of every family layer"
    );
    let as_members: Vec<TagMember> = members_v1
        .iter()
        .map(|r| TagMember {
            layer: r.layer_key.clone(),
            kind: r.kind.clone(),
            key: r.key.clone(),
            op: r.op.clone(),
            fields_hash: r.fields_hash.clone(),
        })
        .collect();
    assert_eq!(v1.content_hash, tags::tag_content_hash(&as_members));
    let after = layers::layer_summaries(&mut tx, Some(family.as_str()))
        .await
        .unwrap();
    assert!(
        after
            .iter()
            .all(|s| s.untagged == 0 && s.latest_tag == Some(1)),
        "{after:?}"
    );
    assert!(
        rows::untagged_rows(&mut tx, &family)
            .await
            .unwrap()
            .is_empty()
    );
    drop(tx);

    // writes after the tag are drafts; the tag does not move
    write(
        db,
        &family,
        Kind::Entity,
        "Order",
        Op::Add,
        entity("Purchase"),
    )
    .await
    .unwrap();
    let mut tx = reader(db).await;
    let s = layers::layer_summaries(&mut tx, Some(family.as_str()))
        .await
        .unwrap();
    assert_eq!(s[0].untagged, 1);
    drop(tx);
    write(
        db,
        &family,
        Kind::Entity,
        "Basket",
        Op::Add,
        entity("Basket"),
    )
    .await
    .unwrap();
    write(
        db,
        &family,
        Kind::Entity,
        "Party",
        Op::Tombstone,
        json!({"reason": "merged"}),
    )
    .await
    .unwrap();

    let mut tx = reader(db).await;
    let drafts = rows::untagged_rows(&mut tx, &family).await.unwrap();
    assert_eq!(
        drafts
            .iter()
            .map(|d| (d.key.as_str(), d.draft_op.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("Basket", "add"),
            ("Order", "patch"),
            ("Party", "tombstone")
        ]
    );
    let still = tags::tag_members(&mut tx, &family, 1).await.unwrap();
    assert_eq!(
        still.iter().map(|r| r.id).collect::<Vec<_>>(),
        members_v1.iter().map(|r| r.id).collect::<Vec<_>>(),
        "a tag's members never change"
    );
    assert_eq!(
        tags::tag(&mut tx, &family, 1).await.unwrap().content_hash,
        v1.content_hash
    );
    assert_eq!(
        refusal(tags::tag(&mut tx, &family, 9).await),
        (404, "tag_not_found")
    );
    drop(tx);

    let v2 = cut(db, &family, &NoHooks).await.expect("tag v2");
    assert_eq!(v2.version, 2);
    assert_ne!(v2.content_hash, v1.content_hash);
    let mut tx = reader(db).await;
    assert_eq!(
        tags::tag_members(&mut tx, &family, 2).await.unwrap().len(),
        4
    );
    assert_eq!(
        tags::tag_members(&mut tx, &family, 1).await.unwrap().len(),
        3
    );
    assert_eq!(
        tags::latest_tags(&mut tx).await.unwrap().get(&family),
        Some(&2)
    );
    assert_eq!(
        tags::latest_tag(&mut tx, &family)
            .await
            .unwrap()
            .map(|t| t.version),
        Some(2)
    );
    let fams = layers::families(&mut tx).await.unwrap();
    let fam = fams.iter().find(|f| f.key == family).expect("the family");
    assert_eq!(fam.layers, vec![family.clone(), email.clone()]);
    assert_eq!((fam.latest_tag, fam.untagged), (Some(2), 0));
    assert_eq!(tags::tags_of(&mut tx, &family).await.unwrap().len(), 2);
}

#[tokio::test]
async fn nothing_to_tag_409() {
    let e = common::env_bare().await;
    let db = &e.state.db;
    let family = job_layer(&e.owner, "t7_empty", None).await;
    assert_eq!(
        refusal(cut(db, &family, &NoHooks).await),
        (409, "nothing_to_tag")
    );
    write(db, &family, Kind::Entity, "Order", Op::Add, entity("Order"))
        .await
        .unwrap();
    assert_eq!(cut(db, &family, &NoHooks).await.unwrap().version, 1);
    assert_eq!(
        refusal(cut(db, &family, &NoHooks).await),
        (409, "nothing_to_tag")
    );
    assert_eq!(
        count(
            &e.owner,
            "SELECT count(*) FROM platform_audit WHERE action = 'template.tag'"
        )
        .await,
        1,
        "a refused tag writes nothing"
    );
    assert_eq!(
        count(
            &e.owner,
            "SELECT count(*) FROM layer_tag WHERE family = 'job/t7_empty'"
        )
        .await,
        1
    );
}

struct Red;

impl GeneratorGuard for Red {
    fn red_items<'a>(
        &'a self,
        _conn: &'a mut sqlx::PgConnection,
        family: &'a str,
    ) -> BoxFuture<'a, agent_api::error::ApiResult<Vec<String>>> {
        Box::pin(async move { Ok(vec![format!("{family}: gate GT has no options")]) })
    }
}

#[tokio::test]
async fn validation_red_422() {
    let e = common::env_bare().await;
    let db = &e.state.db;
    let family = job_layer(&e.owner, "t7_red", None).await;
    write(db, &family, Kind::Entity, "Order", Op::Add, entity("Order"))
        .await
        .unwrap();
    let refused = cut(db, &family, &Red).await;
    assert!(
        matches!(&refused, Err(e) if e.code() == "validation_red"),
        "{refused:?}"
    );
    assert_eq!(
        extra(refused)["items"],
        json!(["job/t7_red: gate GT has no options"])
    );
    assert_eq!(
        count(
            &e.owner,
            "SELECT count(*) FROM layer_tag WHERE family = 'job/t7_red'"
        )
        .await,
        0
    );
    assert_eq!(cut(db, &family, &NoHooks).await.unwrap().version, 1);
}

#[tokio::test]
async fn validation_422() {
    let e = common::env_bare().await;
    let db = &e.state.db;
    let layer = job_layer(&e.owner, "t7_valid", None).await;

    let bad_fields = write(db, &layer, Kind::Entity, "Order", Op::Add, json!({})).await;
    assert!(
        matches!(&bad_fields, Err(e) if e.code() == "invalid_row"),
        "{bad_fields:?}"
    );
    assert_eq!(extra(bad_fields)["errors"][0]["code"], "missing_field");

    let bad_key = write(db, &layer, Kind::Entity, "order", Op::Add, entity("Order")).await;
    assert!(
        matches!(&bad_key, Err(e) if e.code() == "invalid_key"),
        "{bad_key:?}"
    );
    assert_eq!(extra(bad_key)["errors"][0]["field"], "key");

    let bad_tombstone = write(
        db,
        &layer,
        Kind::Entity,
        "Order",
        Op::Tombstone,
        entity("x"),
    )
    .await;
    assert_eq!(refusal(bad_tombstone), (422, "invalid_row"));

    assert_eq!(
        count(
            &e.owner,
            "SELECT count(*) FROM layer_row r JOIN layer l ON l.id = r.layer_id WHERE l.key = 'job/t7_valid'"
        )
        .await,
        0
    );
    assert_eq!(
        count(
            &e.owner,
            "SELECT count(*) FROM platform_audit WHERE action = 'layer.revision'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn kind_not_allowed_in_layer_422() {
    let e = common::env_bare().await;
    let db = &e.state.db;
    let job = job_layer(&e.owner, "t7_scope", None).await;
    let industry = industry_layer(&e.owner, "t7scope").await;

    // error states live in base only, procedures in job layers only
    let refused = write(
        db,
        &job,
        Kind::ErrorState,
        "not_found",
        Op::Add,
        json!({"description": "No such thing."}),
    )
    .await;
    assert!(
        matches!(&refused, Err(e) if e.code() == "kind_not_allowed_in_layer"),
        "{refused:?}"
    );
    assert_eq!(extra(refused)["allowed_scopes"], json!(["base"]));
    assert_eq!(
        refusal(
            write(
                db,
                &industry,
                Kind::Procedure,
                "return_request@2",
                Op::Add,
                json!({"label": "Returns"}),
            )
            .await
        ),
        (422, "kind_not_allowed_in_layer")
    );
    assert_eq!(
        count(
            &e.owner,
            "SELECT count(*) FROM layer_row r JOIN layer l ON l.id = r.layer_id \
              WHERE l.key IN ('job/t7_scope', 'industry/t7scope')"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn one_audit_row_per_write() {
    let e = common::env_bare().await;
    let db = &e.state.db;
    let family = job_layer(&e.owner, "t7_audit", None).await;
    let email = dimension_layer(&e.owner, "t7_audit", "channel", "email").await;
    write(db, &family, Kind::Entity, "Order", Op::Add, entity("Order"))
        .await
        .unwrap();
    write(
        db,
        &family,
        Kind::Entity,
        "Order",
        Op::Add,
        entity("Purchase"),
    )
    .await
    .unwrap();
    write(
        db,
        &email,
        Kind::Entity,
        "Order",
        Op::Patch,
        json!({"hidden": true}),
    )
    .await
    .unwrap();

    let audited: Vec<(String, String, String, String, Value)> = sqlx::query_as(
        "SELECT actor_id, actor_type, subject_label, status, details FROM platform_audit \
          WHERE action = 'layer.revision' ORDER BY id",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(audited.len(), 3, "one row per write");
    let (who, kind, subject, status, details) = &audited[2];
    assert_eq!(
        (
            who.as_str(),
            kind.as_str(),
            subject.as_str(),
            status.as_str()
        ),
        ("service:agent-api-t7-test", "system", email.as_str(), "ok")
    );
    assert_eq!(
        details,
        &json!({"family": family, "layer": email, "kind": "entity", "key": "Order", "op": "patch", "rev": 1})
    );
    assert_eq!(audited[1].4["rev"], 2);

    cut(db, &family, &NoHooks).await.unwrap();
    let (subject, details): (String, Value) = sqlx::query_as(
        "SELECT subject_label, details FROM platform_audit WHERE action = 'template.tag'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(subject, family);
    assert_eq!(
        (&details["version"], &details["members"]),
        (&json!(1), &json!(2))
    );
    assert!(
        details["content_hash"]
            .as_str()
            .is_some_and(|h| h.starts_with("sha256:")),
        "{details}"
    );
}

#[tokio::test]
async fn tag_stack_names_lower_heads() {
    let e = common::env_bare().await;
    let db = &e.state.db;
    let ind = industry_layer(&e.owner, "t7ind").await;
    let other = industry_layer(&e.owner, "t7other").await;
    let dept = dept_layer(&e.owner, "t7ind", "t7dept").await;
    let job = job_layer(
        &e.owner,
        "t7_stack",
        Some(json!({"industry": "t7ind", "departments": ["industry_dept/t7ind/t7dept"], "profile": null})),
    )
    .await;
    for l in [&ind, &other, &dept, &job] {
        write(db, l, Kind::Entity, "Order", Op::Add, entity("Order"))
            .await
            .unwrap();
    }
    cut(db, &ind, &NoHooks).await.unwrap();
    cut(db, &other, &NoHooks).await.unwrap();
    let dept_tag = cut(db, &dept, &NoHooks).await.unwrap();
    let job_tag = cut(db, &job, &NoHooks).await.unwrap();

    // a seeded base (T6) and the seeded base departments (D116 S2) sit in every stack; these assertions look at the
    // industry heads only
    let industry_part = |stack: &Value| -> Vec<Value> {
        stack
            .as_array()
            .expect("stack is a list")
            .iter()
            .filter(|s| {
                s["layer"]
                    .as_str()
                    .is_some_and(|l| l != "base" && !l.starts_with("base_dept/"))
            })
            .cloned()
            .collect()
    };
    assert_eq!(
        industry_part(&dept_tag.stack),
        vec![json!({"layer": "industry/t7ind", "version": 1})]
    );
    assert_eq!(
        industry_part(&job_tag.stack),
        vec![
            json!({"layer": "industry/t7ind", "version": 1}),
            json!({"layer": "industry_dept/t7ind/t7dept", "version": 1}),
        ]
    );
}

#[tokio::test]
async fn override_targets_find_the_nearest_add() {
    let e = common::env_bare().await;
    let db = &e.state.db;
    let job = job_layer(&e.owner, "t7_over", None).await;
    let email = dimension_layer(&e.owner, "t7_over", "channel", "email").await;
    let add = write(db, &job, Kind::Entity, "Order", Op::Add, entity("Order"))
        .await
        .unwrap();
    let patch = write(
        db,
        &email,
        Kind::Entity,
        "Order",
        Op::Patch,
        json!({"hidden": true}),
    )
    .await
    .unwrap();
    let orphan = write(db, &email, Kind::Entity, "Ghost", Op::Tombstone, json!({}))
        .await
        .unwrap();

    let mut tx = reader(db).await;
    let l = layers::layer_by_key(&mut tx, &email).await.unwrap();
    let targets = rows::override_targets(&mut tx, l.id).await.unwrap();
    let found: Vec<(Uuid, Option<String>, Option<Uuid>)> = targets
        .iter()
        .map(|t| {
            (
                t.row_id,
                t.target.as_ref().map(|l| l.key.clone()),
                t.target_row_id,
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            (orphan.id, None, None),
            (patch.id, Some(job.clone()), Some(add.id)),
        ]
    );
    // the head's own add has nothing to override
    let head = layers::layer_by_key(&mut tx, &job).await.unwrap();
    assert!(
        rows::override_targets(&mut tx, head.id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        refusal(rows::override_targets(&mut tx, Uuid::new_v4()).await),
        (404, "layer_not_found")
    );
}

/// Ruling 16: the SQL draft op of `untagged_rows` equals `agent_model::tags::draft_op` over the latest tag's
/// members, including the case that fixes the order (a tombstone of a key the tag does not hold is `tombstone`).
#[tokio::test]
async fn sql_draft_op_equals_agent_model_draft_op() {
    let e = common::env_bare().await;
    let db = &e.state.db;
    let family = job_layer(&e.owner, "t3_draft", None).await;
    let email = dimension_layer(&e.owner, "t3_draft", "channel", "email").await;
    write(db, &family, Kind::Entity, "Order", Op::Add, entity("Order"))
        .await
        .unwrap();
    write(db, &family, Kind::Entity, "Party", Op::Add, entity("Party"))
        .await
        .unwrap();
    cut(db, &family, &NoHooks).await.expect("tag v1");

    write(
        db,
        &family,
        Kind::Entity,
        "Order",
        Op::Add,
        entity("Purchase"),
    )
    .await
    .unwrap();
    write(
        db,
        &family,
        Kind::Entity,
        "Basket",
        Op::Add,
        entity("Basket"),
    )
    .await
    .unwrap();
    write(db, &family, Kind::Entity, "Party", Op::Tombstone, json!({}))
        .await
        .unwrap();
    write(db, &family, Kind::Entity, "Ghost", Op::Tombstone, json!({}))
        .await
        .unwrap();
    write(
        db,
        &email,
        Kind::Entity,
        "Order",
        Op::Patch,
        json!({"hidden": true}),
    )
    .await
    .unwrap();

    let mut tx = reader(db).await;
    let members: Vec<TagMember> = tags::tag_members(&mut tx, &family, 1)
        .await
        .unwrap()
        .iter()
        .map(|r| TagMember {
            layer: r.layer_key.clone(),
            kind: r.kind.clone(),
            key: r.key.clone(),
            op: r.op.clone(),
            fields_hash: r.fields_hash.clone(),
        })
        .collect();
    let latest = member_keys(&members);
    let drafts = rows::untagged_rows(&mut tx, &family).await.unwrap();
    let got: Vec<(String, String, String, String)> = drafts
        .iter()
        .map(|d| {
            let op: Op = serde_json::from_value(Value::String(d.op.clone())).expect("row op");
            let model = draft_op(op, &d.layer_key, &d.kind, &d.key, &latest);
            assert_eq!(
                d.draft_op,
                model.as_str(),
                "SQL vs agent_model draft op of {} {}",
                d.layer_key,
                d.key
            );
            (
                d.layer_key.clone(),
                d.key.clone(),
                d.op.clone(),
                d.draft_op.clone(),
            )
        })
        .collect();
    let want = |layer: &str, key: &str, op: &str, draft: &str| {
        (
            layer.to_owned(),
            key.to_owned(),
            op.to_owned(),
            draft.to_owned(),
        )
    };
    assert_eq!(
        got,
        vec![
            want(family.as_str(), "Basket", "add", "add"),
            want(family.as_str(), "Ghost", "tombstone", "tombstone"),
            want(family.as_str(), "Order", "add", "patch"),
            want(family.as_str(), "Party", "tombstone", "tombstone"),
            want(email.as_str(), "Order", "patch", "add"),
        ]
    );
}
