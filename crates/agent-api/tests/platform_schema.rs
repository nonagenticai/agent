//! B6a T4: the platform layer store (migrations 0041–0043): no tenant column and no RLS, the role grants, revision
//! and tag immutability, the row-kind CHECK, the `job_template_tag` → `layer_tag` foreign key and the cross-tenant pin
//! read. Real Postgres 18 (one container for this binary, a fresh database per test). Fixtures use their own layer
//! keys (`job/t4_*`) and vocabulary keys (`t4_*`), never the seeded ones, so a boot seeder cannot collide with them.
use crate::common;

use agent_api::platform_db::begin_platform;
use common::classes::{Class, class_of};
use common::{insert_tenant, sqlstate};
use serde_json::json;
use sqlx::PgPool;
use uuid::Uuid;

/// The tables of 0041 + 0042, each with a column `agent_platform` may NOT update.
const TABLES: [(&str, &str); 12] = [
    ("layer", "key"),
    ("layer_row", "fields"),
    ("layer_tag", "note"),
    ("layer_tag_row", "row_id"),
    ("axis", "label"),
    ("axis_value", "label"),
    ("axis_rule", "condition"),
    ("dimension", "label"),
    ("dimension_value", "label"),
    ("renewal_preset", "label"),
    ("folded_concept", "becomes"),
    ("compile_reference_seed", "label"),
];

const VIEWS: [&str; 2] = ["layer_head", "layer_untagged"];

/// The 26 row kinds: plan B6 §3's 24, PK-1's `knowledge_pack` (0125) and KH T4's `instruction` (0133, D108).
const KINDS: [&str; 30] = [
    "entity",
    "attribute",
    "relation",
    "job",
    "context_space",
    "procedure",
    "connector_slot",
    "knowledge_store",
    "object",
    "operation",
    "error_state",
    "gate_question",
    "agent_role",
    "profile",
    "axis_rule",
    "department_archetype",
    "journey_theme",
    "journey_stage",
    "theme_stage_prior",
    "stage_knowledge_kind",
    "activity",
    "activity_object",
    "operation_expectation",
    "job_setting",
    "knowledge_pack",
    "instruction",
    "journey_channel",
    "stage_scope",
    "use_case_card",
    "rule_slot",
];

/// The message of a database error (the trigger's RAISE text).
fn db_message(e: &sqlx::Error) -> String {
    e.as_database_error()
        .map(|d| d.message().to_string())
        .unwrap_or_default()
}

/// A `sha256:` hash made of one repeated hex digit.
fn hash(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

/// A job head layer `job/<job>` (its own family); returns its id.
async fn job_layer(db: &PgPool, job: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO layer (key, scope, job, family) VALUES ('job/' || $1, 'job', $1, 'job/' || $1) RETURNING id",
    )
    .bind(job)
    .fetch_one(db)
    .await
    .unwrap()
}

async fn insert_row(
    db: &PgPool,
    layer: Uuid,
    kind: &str,
    key: &str,
    rev: i32,
    current: bool,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO layer_row (layer_id, kind, key, op, fields, rev, is_current, author, fields_hash) \
         VALUES ($1, $2, $3, 'add', $4, $5, $6, 'test', $7) RETURNING id",
    )
    .bind(layer)
    .bind(kind)
    .bind(key)
    .bind(json!({ "label": key }))
    .bind(rev)
    .bind(current)
    .bind(hash('a'))
    .fetch_one(db)
    .await
}

/// Tags `family@v<version>` with `members`, in one owner transaction (v0 = provisional).
async fn tag(db: &PgPool, family: &str, version: i32, members: &[Uuid]) {
    let mut tx = db.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO layer_tag (family, version, content_hash, provisional, tagged_by) \
         VALUES ($1, $2, $3, $4, 'test')",
    )
    .bind(family)
    .bind(version)
    .bind(hash('d'))
    .bind(version == 0)
    .execute(&mut *tx)
    .await
    .unwrap();
    for &m in members {
        sqlx::query("INSERT INTO layer_tag_row (family, version, row_id) VALUES ($1, $2, $3)")
            .bind(family)
            .bind(version)
            .bind(m)
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
}

/// The untagged current rows of `family`, by key.
async fn untagged(db: &PgPool, family: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT key FROM layer_untagged WHERE family = $1 ORDER BY key")
        .bind(family)
        .fetch_all(db)
        .await
        .unwrap()
}

#[tokio::test]
async fn platform_tables_have_no_tenant_id_and_no_rls() {
    let e = common::env().await;
    for t in TABLES.iter().map(|(t, _)| *t).chain(VIEWS) {
        let (columns, tenant_cols): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE column_name::text = 'tenant_id') \
               FROM information_schema.columns WHERE table_schema = 'public' AND table_name::text = $1",
        )
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        assert!(columns > 0, "{t} exists");
        assert_eq!(tenant_cols, 0, "{t} has no tenant_id");
    }
    for (t, _) in TABLES {
        let (rls, force): (bool, bool) = sqlx::query_as(
            "SELECT relrowsecurity, relforcerowsecurity FROM pg_class WHERE oid = ('public.' || $1)::regclass",
        )
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        assert!(!rls && !force, "{t}: no RLS on a platform table");
        let policies: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_policies WHERE schemaname = 'public' AND tablename::text = $1",
        )
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        assert_eq!(policies, 0, "{t}: no policy");
        assert_eq!(
            class_of(t),
            Some(Class::Platform),
            "{t}: Platform in classes.rs"
        );
    }
    // the views run with the caller's rights, so the role grants on the base tables still apply
    for v in VIEWS {
        let opts: Option<Vec<String>> = sqlx::query_scalar(
            "SELECT reloptions::text[] FROM pg_class WHERE oid = ('public.' || $1)::regclass AND relkind = 'v'",
        )
        .bind(v)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        assert!(
            opts.unwrap_or_default()
                .iter()
                .any(|o| o == "security_invoker=true"),
            "{v}: security_invoker"
        );
    }
}

#[tokio::test]
async fn app_role_reads_only() {
    let e = common::env().await;
    let l = job_layer(&e.owner, "t4_reads").await;
    insert_row(&e.owner, l, "entity", "Party", 1, true)
        .await
        .unwrap();
    let db = &e.state.db;
    for t in TABLES.iter().map(|(t, _)| *t).chain(VIEWS) {
        let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {t}")))
            .fetch_one(db)
            .await
            .unwrap_or_else(|err| panic!("agent_app reads {t}: {err}"));
        if t == "layer_head" || t == "layer_untagged" {
            assert!(n >= 1, "{t} shows the current row");
        }
    }
    for (t, c) in TABLES {
        for sql in [
            format!("INSERT INTO {t} DEFAULT VALUES"),
            format!("UPDATE {t} SET {c} = {c}"),
            format!("DELETE FROM {t}"),
        ] {
            let err = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .execute(db)
                .await
                .unwrap_err();
            assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{sql}: {err}");
        }
    }
    // the allowed agent_platform column UPDATEs are not agent_app's
    for sql in [
        "UPDATE layer SET label = 'x'",
        "UPDATE layer_row SET is_current = false",
        "UPDATE axis_rule SET priority = 1",
    ] {
        let err = sqlx::query(sql).execute(db).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{sql}: {err}");
    }
    for v in VIEWS {
        for role in ["agent_app", "agent_platform"] {
            for p in ["INSERT", "UPDATE", "DELETE"] {
                let has: bool = sqlx::query_scalar(
                    "SELECT has_table_privilege($1::text::name, 'public.' || $2, $3)",
                )
                .bind(role)
                .bind(v)
                .bind(p)
                .fetch_one(&e.owner)
                .await
                .unwrap();
                assert!(!has, "{role} may not {p} {v}");
            }
        }
    }
}

#[tokio::test]
async fn platform_role_inserts_never_deletes() {
    let e = common::env().await;
    let db = &e.state.db;
    let mut tx = begin_platform(db).await.unwrap();
    let layer: Uuid = sqlx::query_scalar(
        "INSERT INTO layer (key, scope, job, family, label) \
         VALUES ('job/t4_plat', 'job', 't4_plat', 'job/t4_plat', 'Plat') RETURNING id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("agent_platform inserts a layer");
    let row: Uuid = sqlx::query_scalar(
        "INSERT INTO layer_row (layer_id, kind, key, op, fields, rev, is_current, author, fields_hash) \
         VALUES ($1, 'object', 'order', 'add', '{}', 1, true, 'sa', $2) RETURNING id",
    )
    .bind(layer)
    .bind(hash('a'))
    .fetch_one(&mut *tx)
    .await
    .expect("agent_platform inserts a row");
    sqlx::query(
        "INSERT INTO layer_tag (family, version, content_hash, tagged_by) VALUES ('job/t4_plat', 1, $1, 'sa')",
    )
    .bind(hash('d'))
    .execute(&mut *tx)
    .await
    .expect("agent_platform tags");
    sqlx::query(
        "INSERT INTO layer_tag_row (family, version, row_id) VALUES ('job/t4_plat', 1, $1)",
    )
    .bind(row)
    .execute(&mut *tx)
    .await
    .expect("agent_platform adds the tag members");
    for sql in [
        "INSERT INTO axis (key, label, cardinality, object, milestone) VALUES ('t4_axis', 'A', 'one', 'Party', 'M1')",
        "INSERT INTO axis_value (axis, key, label, added_by) VALUES ('t4_axis', 'v', 'V', 'sa')",
        "INSERT INTO dimension (key, label) VALUES ('t4_dim', 'D')",
        "INSERT INTO dimension_value (dimension, key, label, added_by) VALUES ('t4_dim', 'v', 'V', 'sa')",
        "INSERT INTO renewal_preset (key, label, order_type, initiator) VALUES ('t4_p', 'P', 'continuous', 'provider')",
        "INSERT INTO folded_concept (concept, becomes, object, cite) VALUES ('t4_c', 'b', 'o', 'c')",
        "INSERT INTO compile_reference_seed (key, label, selection, job) VALUES ('t4_r', 'R', '{}', 'care')",
    ] {
        sqlx::query(sql)
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    sqlx::query(
        "INSERT INTO axis_rule (key, priority, condition, layer_id, owner, created_by) \
         VALUES ('t4_rule', 10, '{\"eq\":[\"t4_axis\",\"v\"]}', $1, 'job/t4_plat', 'sa')",
    )
    .bind(layer)
    .execute(&mut *tx)
    .await
    .expect("agent_platform inserts an axis rule");
    // the named column UPDATEs
    for sql in [
        "UPDATE layer SET label = 'Plat 2', rollout_wave = 2, template_selection = '{}' WHERE key = 'job/t4_plat'",
        "UPDATE layer_row SET is_current = false \
         WHERE key = 'order' AND is_current AND layer_id = (SELECT id FROM layer WHERE key = 'job/t4_plat')",
        "UPDATE axis_rule SET priority = 20 WHERE key = 't4_rule'",
    ] {
        sqlx::query(sql)
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|err| panic!("{sql}: {err}"));
    }
    tx.commit().await.unwrap();
    let n: i64 =
        sqlx::query_scalar("SELECT count(*) FROM layer_row WHERE layer_id = $1 AND NOT is_current")
            .bind(layer)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(n, 1, "the platform writes committed");
    // never DELETE, and no UPDATE outside the named columns (one transaction per attempt: an error aborts it)
    for (t, c) in TABLES {
        for sql in [
            format!("DELETE FROM {t}"),
            format!("UPDATE {t} SET {c} = {c}"),
        ] {
            let mut tx = begin_platform(db).await.unwrap();
            let err = sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .execute(&mut *tx)
                .await
                .unwrap_err();
            assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{sql}: {err}");
        }
    }
}

#[tokio::test]
async fn row_fields_are_immutable() {
    let e = common::env().await;
    let l = job_layer(&e.owner, "t4_immutable").await;
    let r = insert_row(&e.owner, l, "object", "order", 1, true)
        .await
        .unwrap();
    // the trigger holds for the owner too
    for sql in [
        "UPDATE layer_row SET fields = '{\"label\":\"changed\"}' WHERE id = $1",
        "UPDATE layer_row SET fields_hash = 'sha256:' || repeat('b', 64) WHERE id = $1",
        "UPDATE layer_row SET rev = 2 WHERE id = $1",
        "UPDATE layer_row SET op = 'patch' WHERE id = $1",
        "UPDATE layer_row SET author = 'someone else' WHERE id = $1",
        "UPDATE layer_row SET is_current = true WHERE id = $1",
        "UPDATE layer_row SET is_current = false, fields = '{}' WHERE id = $1",
    ] {
        let err = sqlx::query(sql)
            .bind(r)
            .execute(&e.owner)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("P0001"), "{sql}: {err}");
        assert_eq!(db_message(&err), "layer_row_immutable", "{sql}");
    }
    // agent_platform has no column grant on fields at all
    let mut tx = begin_platform(&e.state.db).await.unwrap();
    let err = sqlx::query("UPDATE layer_row SET fields = '{}' WHERE id = $1")
        .bind(r)
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    drop(tx);
    // the one allowed change: retire the current revision, as agent_platform
    let mut tx = begin_platform(&e.state.db).await.unwrap();
    let n = sqlx::query("UPDATE layer_row SET is_current = false WHERE id = $1")
        .bind(r)
        .execute(&mut *tx)
        .await
        .expect("is_current true -> false")
        .rows_affected();
    assert_eq!(n, 1);
    tx.commit().await.unwrap();
    // and never back
    let err = sqlx::query("UPDATE layer_row SET is_current = true WHERE id = $1")
        .bind(r)
        .execute(&e.owner)
        .await
        .unwrap_err();
    assert_eq!(db_message(&err), "layer_row_immutable", "{err}");
    let fields: serde_json::Value =
        sqlx::query_scalar("SELECT fields FROM layer_row WHERE id = $1")
            .bind(r)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(fields, json!({ "label": "order" }), "fields unchanged");
    // a fields_hash that is not sha256:<64 hex> is refused
    let err = sqlx::query(
        "INSERT INTO layer_row (layer_id, kind, key, op, rev, is_current, author, fields_hash) \
         VALUES ($1, 'object', 'case', 'add', 1, true, 't', 'sha256:ABC')",
    )
    .bind(l)
    .execute(&e.owner)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{err}");
}

#[tokio::test]
async fn one_current_revision_per_key() {
    let e = common::env().await;
    let l = job_layer(&e.owner, "t4_current").await;
    let other = job_layer(&e.owner, "t4_current_other").await;
    let r1 = insert_row(&e.owner, l, "entity", "Party", 1, true)
        .await
        .unwrap();
    // a second current revision of the same (layer, kind, key)
    let err = insert_row(&e.owner, l, "entity", "Party", 2, true)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
    // the same rev twice
    let err = insert_row(&e.owner, l, "entity", "Party", 1, false)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23505"), "{err}");
    // the same key in another layer, or of another kind, is another item
    insert_row(&e.owner, other, "entity", "Party", 1, true)
        .await
        .expect("another layer");
    insert_row(&e.owner, l, "object", "Party", 1, true)
        .await
        .expect("another kind");
    // the revision write: flip the current one, insert rev + 1
    let mut tx = e.owner.begin().await.unwrap();
    sqlx::query("UPDATE layer_row SET is_current = false WHERE id = $1")
        .bind(r1)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO layer_row (layer_id, kind, key, op, fields, rev, is_current, author, fields_hash) \
         VALUES ($1, 'entity', 'Party', 'patch', '{}', 2, true, 'sa', $2)",
    )
    .bind(l)
    .bind(hash('b'))
    .execute(&mut *tx)
    .await
    .expect("rev 2 once rev 1 is retired");
    tx.commit().await.unwrap();
    let head: Vec<(String, String, i32)> = sqlx::query_as(
        "SELECT kind, key, rev FROM layer_head WHERE layer_id = $1 ORDER BY kind, key",
    )
    .bind(l)
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        head,
        [
            ("entity".to_string(), "Party".to_string(), 2),
            ("object".to_string(), "Party".to_string(), 1),
        ]
    );
}

#[tokio::test]
async fn tag_is_immutable_unless_provisional() {
    let e = common::env().await;
    let fam = "job/t4_tag";
    let l = job_layer(&e.owner, "t4_tag").await;
    let r1 = insert_row(&e.owner, l, "object", "order", 1, true)
        .await
        .unwrap();
    assert_eq!(untagged(&e.owner, fam).await, ["order"], "no tag yet");
    tag(&e.owner, fam, 1, &[r1]).await;
    assert!(untagged(&e.owner, fam).await.is_empty(), "order is in v1");
    // a tag and its members never change (the owner included)
    for sql in [
        "UPDATE layer_tag SET note = 'edited' WHERE family = $1 AND version = 1",
        "DELETE FROM layer_tag WHERE family = $1 AND version = 1",
        "DELETE FROM layer_tag_row WHERE family = $1 AND version = 1",
        "UPDATE layer_tag_row SET version = 1 WHERE family = $1 AND version = 1",
    ] {
        let err = sqlx::query(sql)
            .bind(fam)
            .execute(&e.owner)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("P0001"), "{sql}: {err}");
        assert_eq!(db_message(&err), "layer_tag_immutable", "{sql}");
    }
    // a member cannot join v1 after the transaction that tagged it
    let r2 = insert_row(&e.owner, l, "object", "case", 1, true)
        .await
        .unwrap();
    let err = sqlx::query("INSERT INTO layer_tag_row (family, version, row_id) VALUES ($1, 1, $2)")
        .bind(fam)
        .bind(r2)
        .execute(&e.owner)
        .await
        .unwrap_err();
    assert_eq!(db_message(&err), "layer_tag_immutable", "{err}");
    assert_eq!(untagged(&e.owner, fam).await, ["case"]);
    // nor can a row of another family
    let stranger = job_layer(&e.owner, "t4_tag_stranger").await;
    let r3 = insert_row(&e.owner, stranger, "object", "order", 1, true)
        .await
        .unwrap();
    let mut tx = e.owner.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO layer_tag (family, version, content_hash, tagged_by) VALUES ($1, 2, $2, 't')",
    )
    .bind(fam)
    .bind(hash('d'))
    .execute(&mut *tx)
    .await
    .unwrap();
    let err = sqlx::query("INSERT INTO layer_tag_row (family, version, row_id) VALUES ($1, 2, $2)")
        .bind(fam)
        .bind(r3)
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(db_message(&err), "layer_tag_row_family", "{err}");
    drop(tx);
    // provisional ⇔ v0
    for (version, provisional) in [(2, true), (0, false)] {
        let err = sqlx::query(
            "INSERT INTO layer_tag (family, version, content_hash, provisional, tagged_by) \
             VALUES ($1, $2, $3, $4, 't')",
        )
        .bind(fam)
        .bind(version)
        .bind(hash('d'))
        .bind(provisional)
        .execute(&e.owner)
        .await
        .unwrap_err();
        assert_eq!(
            sqlstate(&err).as_deref(),
            Some("23514"),
            "v{version}: {err}"
        );
    }
    // the provisional v0 is replaceable: edit it, drop its members and itself, tag it again
    tag(&e.owner, fam, 0, &[r2]).await;
    assert_eq!(
        untagged(&e.owner, fam).await,
        ["case"],
        "a provisional tag does not count as tagged"
    );
    sqlx::query("UPDATE layer_tag SET note = 'regenerated' WHERE family = $1 AND version = 0")
        .bind(fam)
        .execute(&e.owner)
        .await
        .expect("v0 may change");
    sqlx::query("DELETE FROM layer_tag_row WHERE family = $1 AND version = 0")
        .bind(fam)
        .execute(&e.owner)
        .await
        .expect("v0 members may go");
    sqlx::query("DELETE FROM layer_tag WHERE family = $1 AND version = 0")
        .bind(fam)
        .execute(&e.owner)
        .await
        .expect("v0 may go");
    tag(&e.owner, fam, 0, &[r1, r2]).await;
    let members: i64 =
        sqlx::query_scalar("SELECT count(*) FROM layer_tag_row WHERE family = $1 AND version = 0")
            .bind(fam)
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(members, 2, "v0 replaced");
}

#[tokio::test]
async fn kind_check_lists_30_kinds() {
    let e = common::env().await;
    let def: String = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint \
          WHERE conname = 'layer_row_kind_check' AND conrelid = 'layer_row'::regclass",
    )
    .fetch_one(&e.owner)
    .await
    .expect("the named CHECK exists");
    assert_eq!(
        def.matches("::text").count(),
        KINDS.len(),
        "exactly 30 kinds: {def}"
    );
    let l = job_layer(&e.owner, "t4_kinds").await;
    for kind in KINDS {
        assert!(def.contains(&format!("'{kind}'")), "{kind} in {def}");
        insert_row(&e.owner, l, kind, "k", 1, true)
            .await
            .unwrap_or_else(|err| panic!("{kind}: {err}"));
    }
    for kind in ["tenant", "overlay", "Entity", ""] {
        let err = insert_row(&e.owner, l, kind, "k2", 1, true)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{kind:?}: {err}");
    }
}

#[tokio::test]
async fn pin_fk_rejects_unknown_tag() {
    let e = common::env().await;
    let insert = "INSERT INTO job_template_tag (layer, version, key, label, pins, status, created_by) \
                  VALUES ($1, $2, 't4_pin', 'Pin', '[]', 'tagged', 'test')";
    let err = sqlx::query(insert)
        .bind("job/t4_unknown")
        .bind(7)
        .execute(&e.owner)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23503"), "{err}");
    // added NOT VALID (ledger Ruling 5): B1's 0033 seed rows predate the seeded layer tags
    let validated: bool = sqlx::query_scalar(
        "SELECT convalidated FROM pg_constraint \
          WHERE conname = 'job_template_tag_layer_tag_fk' AND conrelid = 'job_template_tag'::regclass",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(!validated);
    // a known tag: agent_platform writes the projection (insert + upsert)
    let l = job_layer(&e.owner, "t4_pin").await;
    let r = insert_row(&e.owner, l, "job", "t4_pin", 1, true)
        .await
        .unwrap();
    tag(&e.owner, "job/t4_pin", 1, &[r]).await;
    let mut tx = begin_platform(&e.state.db).await.unwrap();
    sqlx::query(insert)
        .bind("job/t4_pin")
        .bind(1)
        .execute(&mut *tx)
        .await
        .expect("agent_platform inserts a job template tag");
    sqlx::query(
        "INSERT INTO job_template_tag (layer, version, key, label, pins, status, created_by) \
         VALUES ('job/t4_pin', 1, 't4_pin', 'Pin 2', '[]', 'tagged', 'test') \
         ON CONFLICT (layer, version) DO UPDATE SET label = EXCLUDED.label, pins = EXCLUDED.pins",
    )
    .execute(&mut *tx)
    .await
    .expect("agent_platform upserts the projection");
    tx.commit().await.unwrap();
    // an UPDATE onto an unknown tag is checked too
    let err = sqlx::query("UPDATE job_template_tag SET version = 2 WHERE layer = 'job/t4_pin'")
        .execute(&e.owner)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("23503"), "{err}");
}

#[tokio::test]
async fn template_pins_all_reads_across_tenants() {
    let e = common::env().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_tenant(&e.owner, b, "Beta").await;
    for (t, pins) in [
        (
            a,
            json!([{"layer": "base", "version": 3}, {"layer": "job/customer_care_agent", "version": 4}]),
        ),
        (b, json!([{"layer": "base", "version": 2}])),
    ] {
        sqlx::query(
            "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'care', 'Care', $2, 'test')",
        )
        .bind(t)
        .bind(pins)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let err = sqlx::query("SELECT * FROM agent_template_pins_all()")
        .execute(&e.state.db)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    let mut tx = begin_platform(&e.state.db).await.unwrap();
    let rows: Vec<(String, String, String, String, i32)> = sqlx::query_as(
        "SELECT tenant_name, tenant_status, job, layer_key, version FROM agent_template_pins_all() \
          WHERE tenant_id = ANY ($1) ORDER BY tenant_name, layer_key",
    )
    .bind(vec![a, b])
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let s = |v: &str| v.to_string();
    assert_eq!(
        rows,
        [
            (s("Alpha"), s("active"), s("care"), s("base"), 3),
            (
                s("Alpha"),
                s("active"),
                s("care"),
                s("job/customer_care_agent"),
                4
            ),
            (s("Beta"), s("active"), s("care"), s("base"), 2),
        ]
    );
}

#[tokio::test]
async fn platform_drops_only_a_provisional_v0_through_the_definer() {
    // 0195: no DELETE grant on tags; `platform_drop_provisional_tag` removes a family's provisional v0 and nothing else
    let e = common::env().await;
    let db = &e.state.db;
    let mut tx = begin_platform(db).await.unwrap();
    sqlx::query(
        "INSERT INTO layer (key, scope, job, family, label) VALUES ('job/t4_prov', 'job', 't4_prov', 'job/t4_prov', 'P')",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    for (v, provisional, h) in [(0, true, 'e'), (1, false, 'f')] {
        sqlx::query(
            "INSERT INTO layer_tag (family, version, content_hash, tagged_by, provisional) \
             VALUES ('job/t4_prov', $1, $2, 'sa', $3)",
        )
        .bind(v)
        .bind(hash(h))
        .bind(provisional)
        .execute(&mut *tx)
        .await
        .unwrap_or_else(|err| panic!("tag v{v}: {err}"));
    }
    sqlx::query("SELECT platform_drop_provisional_tag('job/t4_prov')")
        .execute(&mut *tx)
        .await
        .expect("agent_platform may call the definer");
    let versions: Vec<i32> = sqlx::query_scalar(
        "SELECT version FROM layer_tag WHERE family = 'job/t4_prov' ORDER BY version",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        versions,
        [1],
        "the provisional v0 is gone, the real tag stays"
    );
    tx.commit().await.unwrap();
    // and a direct DELETE of the real tag is still refused
    let mut tx = begin_platform(db).await.unwrap();
    let err = sqlx::query("DELETE FROM layer_tag WHERE family = 'job/t4_prov' AND version = 1")
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
}
