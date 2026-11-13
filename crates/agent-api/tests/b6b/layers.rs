//! B6b T9a: the six P4 layer reads (`/api/layers*`, `/api/layer-rows/{id}`) over the booted platform seed —
//! super_admin only, the UI `Layer` shape (tag = family@latest), overlay kinds only on P4 (§10), retired revisions on
//! request, revisions newest first, override targets, and the 404s. Comparisons sort in Rust, never by SQL collation.
use std::collections::BTreeSet;

use agent_api::audit::Actor;
use agent_api::platform::db::rows;
use agent_api::platform_db::begin_platform;
use agent_model::kinds::{Kind, Op};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{
    TestEnv, get, insert_tenant, insert_user, json as json_req, person, send, super_admin,
};

const OVERLAY: [&str; 8] = [
    "entity",
    "attribute",
    "relation",
    "job",
    "context_space",
    "procedure",
    "connector_slot",
    "knowledge_store",
];

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn sa_get(e: &TestEnv, uri: &str) -> (StatusCode, Value) {
    send(&e.app, get(uri, Some(&super_admin()))).await
}

async fn ok(e: &TestEnv, uri: &str) -> Value {
    let (s, b) = sa_get(e, uri).await;
    assert_eq!(s, StatusCode::OK, "{uri}: {b}");
    b
}

async fn layer_id(e: &TestEnv, key: &str) -> Uuid {
    sqlx::query_scalar("SELECT id FROM layer WHERE key = $1")
        .bind(key)
        .fetch_one(&e.owner)
        .await
        .unwrap_or_else(|_| panic!("seed layer {key}"))
}

fn overlay_kinds() -> Vec<String> {
    OVERLAY.iter().map(|k| k.to_string()).collect()
}

fn ids(list: &Value) -> BTreeSet<String> {
    list.as_array()
        .expect("a list")
        .iter()
        .map(|r| r["id"].as_str().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn layers_are_super_admin_only() {
    let e = crate::common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    let admin = person("u-admin", &[]);
    let base = layer_id(&e, "base").await;
    let row: Uuid = sqlx::query_scalar("SELECT id FROM layer_row LIMIT 1")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    for uri in [
        "/api/layers".to_string(),
        "/api/layers/summary".to_string(),
        format!("/api/layers/{base}/rows"),
        format!("/api/layers/{base}/rows/entity/Party/revisions"),
        format!("/api/layers/{base}/overrides"),
        format!("/api/layer-rows/{row}"),
    ] {
        let (s, b) = send(&e.app, get(&uri, Some(&admin))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{uri}: {b}");
        assert_eq!(code(&b), "super_admin_required", "{uri}");
        let (s, b) = send(&e.app, get(&uri, None)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{uri}: {b}");
    }
}

#[tokio::test]
async fn layer_catalog_is_every_layer_with_its_family_tag() {
    let e = crate::common::env().await;
    crate::b6b::support::ensure_layer(
        &e,
        "job/customer_care_agent/channel=email",
        json!({"scope": "job", "job": "customer_care_agent", "dimension": ["channel", "email"],
               "label": "Email"}),
    )
    .await;
    let b = ok(&e, "/api/layers").await;
    assert_eq!(b["next"], Value::Null);
    let items = b["items"].as_array().unwrap();
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM layer")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(items.len() as i64, total);

    let latest: Vec<(String, i32)> = sqlx::query_as(
        "SELECT family, max(version) FROM layer_tag WHERE NOT provisional GROUP BY family",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    for l in items {
        let family = l["family"].as_str().unwrap();
        match latest.iter().find(|(f, _)| f == family) {
            Some((_, v)) => {
                assert_eq!(l["tag"], json!(format!("{family}@v{v}")), "{l}");
                assert!(l["tagged_at"].is_string(), "{l}");
            }
            None => {
                assert_eq!(l["tag"], Value::Null, "{l}");
                assert!(l.get("tagged_at").is_none(), "{l}");
            }
        }
    }

    let email = items
        .iter()
        .find(|l| l["key"] == "job/customer_care_agent/channel=email")
        .expect("the job × channel layer");
    assert_eq!(email["scope"], "job");
    assert_eq!(email["job"], "customer_care_agent");
    assert_eq!(email["dimension"], json!(["channel", "email"]));
    assert_eq!(email["family"], "job/customer_care_agent");
    let base = items.iter().find(|l| l["key"] == "base").unwrap();
    assert!(base.get("industry").is_none() && base.get("dimension").is_none());
}

#[tokio::test]
async fn layer_rows_404() {
    let e = crate::common::env().await;
    for id in [Uuid::new_v4().to_string(), "not-a-uuid".to_string()] {
        for uri in [
            format!("/api/layers/{id}/rows"),
            format!("/api/layers/{id}/rows/entity/Party/revisions"),
            format!("/api/layers/{id}/overrides"),
        ] {
            let (s, b) = sa_get(&e, &uri).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{uri}: {b}");
            assert_eq!(code(&b), "layer_not_found", "{uri}");
        }
    }
    let base = layer_id(&e, "base").await;
    let (s, b) = sa_get(&e, &format!("/api/layers/{base}/rows/nope/Party/revisions")).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "invalid_row");
    let (s, b) = sa_get(
        &e,
        &format!("/api/layers/{base}/rows?include_retired=maybe"),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
}

#[tokio::test]
async fn find_row_404() {
    let e = crate::common::env().await;
    for id in [Uuid::new_v4().to_string(), "not-a-uuid".to_string()] {
        let (s, b) = sa_get(&e, &format!("/api/layer-rows/{id}")).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{id}: {b}");
        assert_eq!(code(&b), "row_not_found");
    }
}

#[tokio::test]
async fn layer_rows_are_overlay_kinds_current_unless_retired_asked() {
    let e = crate::common::env().await;
    let base = layer_id(&e, "base").await;
    let all: i64 = sqlx::query_scalar("SELECT count(*) FROM layer_row WHERE layer_id = $1")
        .bind(base)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let overlay_current: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM layer_row WHERE layer_id = $1 AND is_current AND kind = ANY($2)",
    )
    .bind(base)
    .bind(overlay_kinds())
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert!(
        (overlay_current.len() as i64) < all,
        "base also holds catalogue rows"
    );

    let b = ok(&e, &format!("/api/layers/{base}/rows")).await;
    let items = &b["items"];
    let want: BTreeSet<String> = overlay_current.iter().map(Uuid::to_string).collect();
    assert_eq!(ids(items), want);
    let list = items.as_array().unwrap();
    for r in list {
        assert!(OVERLAY.contains(&r["kind"].as_str().unwrap()), "{r}");
        assert_eq!(r["is_current"], true);
        assert_eq!(r["layer_id"], json!(base.to_string()));
    }
    let order: Vec<(String, String)> = list
        .iter()
        .map(|r| {
            (
                r["kind"].as_str().unwrap().into(),
                r["key"].as_str().unwrap().into(),
            )
        })
        .collect();
    let mut sorted = order.clone();
    sorted.sort();
    assert_eq!(order, sorted, "kind, key order");
}

/// A new revision of a current overlay row: retired revisions appear only on request, revisions are newest first,
/// the old revision is still findable, and the summary counts it as retired and untagged.
#[tokio::test]
async fn new_revision_retires_the_old_one() {
    let e = crate::common::env().await;
    crate::b6b::support::row(
        &e,
        "industry/retail",
        "entity",
        "Gizmo",
        json!({"label": "Gizmo"}),
    )
    .await;
    crate::b6b::support::tag(&e, "industry/retail").await;
    let retail = layer_id(&e, "industry/retail").await;
    let (old_id, kind, key, op, fields): (Uuid, String, String, String, Value) = sqlx::query_as(
        "SELECT r.id, r.kind, r.key, r.op, r.fields FROM layer_row r \
          WHERE r.layer_id = $1 AND r.is_current AND r.kind = 'entity' AND r.op = 'add' \
            AND EXISTS (SELECT 1 FROM layer_tag_row m \
                         WHERE m.row_id = r.id AND m.family = 'industry/retail' \
                           AND m.version = (SELECT max(t.version) FROM layer_tag t \
                                             WHERE t.family = 'industry/retail' AND NOT t.provisional)) \
          ORDER BY r.key LIMIT 1",
    )
    .bind(retail)
    .fetch_one(&e.owner)
    .await
    .expect("a tagged current retail entity add");
    assert_eq!(op, "add");
    let before = ok(&e, "/api/layers/summary").await;
    let before = before
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["layer_id"] == json!(retail.to_string()))
        .cloned()
        .unwrap();

    let mut tx = begin_platform(&e.owner).await.expect("begin_platform");
    let new = rows::write_revision(
        &mut tx,
        &Actor::system("service:agent-api-b6b-t9a-test"),
        "industry/retail",
        Kind::parse(&kind).unwrap(),
        &key,
        Op::Add,
        fields,
        "t9a-author",
    )
    .await
    .expect("write_revision");
    tx.commit().await.unwrap();

    let current = ok(&e, &format!("/api/layers/{retail}/rows")).await;
    let current = ids(&current["items"]);
    assert!(current.contains(&new.id.to_string()));
    assert!(!current.contains(&old_id.to_string()));
    let with_retired = ok(
        &e,
        &format!("/api/layers/{retail}/rows?include_retired=true"),
    )
    .await;
    let with_retired = ids(&with_retired["items"]);
    assert!(with_retired.contains(&old_id.to_string()));
    assert_eq!(
        with_retired.len(),
        current.len()
            + 1
            + {
                let r: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM layer_row WHERE layer_id = $1 AND NOT is_current AND kind = ANY($2) AND id <> $3",
        )
        .bind(retail)
        .bind(overlay_kinds())
        .bind(old_id)
        .fetch_one(&e.owner)
        .await
        .unwrap();
                r as usize
            }
    );

    let revs = ok(
        &e,
        &format!("/api/layers/{retail}/rows/{kind}/{key}/revisions"),
    )
    .await;
    let revs = revs.as_array().unwrap();
    assert!(revs.len() >= 2);
    assert_eq!(revs[0]["id"], json!(new.id.to_string()));
    assert_eq!(revs[0]["is_current"], true);
    let numbers: Vec<i64> = revs.iter().map(|r| r["rev"].as_i64().unwrap()).collect();
    assert!(numbers.windows(2).all(|w| w[0] > w[1]), "{numbers:?}");

    let found = ok(&e, &format!("/api/layer-rows/{old_id}")).await;
    assert_eq!(found["row"]["is_current"], false);
    assert_eq!(found["layer"]["key"], "industry/retail");
    assert_eq!(found["layer"]["id"], json!(retail.to_string()));

    let after = ok(&e, "/api/layers/summary").await;
    let after = after
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["layer_id"] == json!(retail.to_string()))
        .cloned()
        .unwrap();
    let n = |v: &Value, f: &str| v[f].as_i64().unwrap();
    assert_eq!(n(&after, "retired"), n(&before, "retired") + 1);
    assert_eq!(n(&after, "adds"), n(&before, "adds"));
    assert_eq!(n(&after, "untagged"), n(&before, "untagged") + 1);
    assert_eq!(after["authors"][0], "t9a-author");
}

#[tokio::test]
async fn summaries_count_overlay_rows_of_every_layer() {
    let e = crate::common::env().await;
    let b = ok(&e, "/api/layers/summary").await;
    let list = b.as_array().unwrap();
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM layer")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(list.len() as i64, total);
    let want: Vec<(Uuid, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT l.id, \
                count(r.id) FILTER (WHERE r.is_current AND r.op = 'add'), \
                count(r.id) FILTER (WHERE r.is_current AND r.op = 'patch'), \
                count(r.id) FILTER (WHERE r.is_current AND r.op = 'tombstone'), \
                count(r.id) FILTER (WHERE NOT r.is_current) \
           FROM layer l LEFT JOIN layer_row r ON r.layer_id = l.id AND r.kind = ANY($1) \
          GROUP BY l.id",
    )
    .bind(overlay_kinds())
    .fetch_all(&e.owner)
    .await
    .unwrap();
    for (id, adds, patches, tombstones, retired) in want {
        let s = list
            .iter()
            .find(|s| s["layer_id"] == json!(id.to_string()))
            .unwrap_or_else(|| panic!("summary of {id}"));
        assert_eq!(
            (
                s["adds"].as_i64().unwrap(),
                s["patches"].as_i64().unwrap(),
                s["tombstones"].as_i64().unwrap(),
                s["retired"].as_i64().unwrap()
            ),
            (adds, patches, tombstones, retired),
            "{s}"
        );
        assert!(
            s["untagged"].as_i64().unwrap() <= adds + patches + tombstones,
            "{s}"
        );
    }
}

#[tokio::test]
async fn override_targets_act_on_earlier_adds() {
    let e = crate::common::env().await;
    // base adds two entities; the retail layer patches one and tombstones the other (the routes cannot write a
    // patch over another layer's add, so the revisions go in as the platform's own transaction does)
    for key in ["Gizmo", "Widget"] {
        crate::b6b::support::row(&e, "base", "entity", key, json!({"label": key})).await;
    }
    let mut tx = begin_platform(&e.owner).await.expect("begin_platform");
    for (key, op, fields) in [
        ("Gizmo", Op::Patch, json!({"label": "Gizmo (retail)"})),
        ("Widget", Op::Tombstone, json!({})),
    ] {
        rows::write_revision(
            &mut tx,
            &Actor::system("service:agent-api-b6b-t9a-test"),
            "industry/retail",
            Kind::Entity,
            key,
            op,
            fields,
            "t9a-author",
        )
        .await
        .expect("write_revision");
    }
    tx.commit().await.unwrap();
    let retail = layer_id(&e, "industry/retail").await;
    let b = ok(&e, &format!("/api/layers/{retail}/overrides")).await;
    let list = b.as_array().unwrap();
    let want: BTreeSet<String> = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM layer_row WHERE layer_id = $1 AND op <> 'add' AND kind = ANY($2)",
    )
    .bind(retail)
    .bind(overlay_kinds())
    .fetch_all(&e.owner)
    .await
    .unwrap()
    .iter()
    .map(Uuid::to_string)
    .collect();
    assert!(
        !want.is_empty(),
        "the retail layer patches/tombstones base rows"
    );
    let got: BTreeSet<String> = list
        .iter()
        .map(|o| o["row_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(got, want);
    let mut hits = 0;
    for o in list {
        let Some(target) = o["target_row_id"].as_str() else {
            assert_eq!(o["target"], Value::Null);
            continue;
        };
        hits += 1;
        let (row_kind, row_key): (String, String) =
            sqlx::query_as("SELECT kind, key FROM layer_row WHERE id = $1::uuid")
                .bind(o["row_id"].as_str().unwrap())
                .fetch_one(&e.owner)
                .await
                .unwrap();
        let (t_layer, t_kind, t_key, t_op, t_current): (Uuid, String, String, String, bool) =
            sqlx::query_as(
                "SELECT layer_id, kind, key, op, is_current FROM layer_row WHERE id = $1::uuid",
            )
            .bind(target)
            .fetch_one(&e.owner)
            .await
            .unwrap();
        assert_eq!((t_kind, t_key), (row_kind, row_key));
        assert_eq!(t_op, "add");
        assert!(t_current);
        assert_eq!(o["target"]["id"], json!(t_layer.to_string()));
        let scope = o["target"]["scope"].as_str().unwrap();
        assert!(
            scope == "base" || scope == "profile",
            "a retail override acts on an earlier layer: {o}"
        );
    }
    assert!(
        hits > 0,
        "at least one retail override has an earlier target"
    );
}

#[tokio::test]
async fn reads_never_write() {
    let e = crate::common::env().await;
    let counts = |e: &TestEnv| {
        let db = e.owner.clone();
        async move {
            sqlx::query_as::<_, (i64, i64, i64)>(
                "SELECT (SELECT count(*) FROM layer), (SELECT count(*) FROM layer_row), \
                        (SELECT count(*) FROM platform_audit)",
            )
            .fetch_one(&db)
            .await
            .unwrap()
        }
    };
    let before = counts(&e).await;
    let base = layer_id(&e, "base").await;
    for uri in [
        "/api/layers".to_string(),
        "/api/layers/summary".to_string(),
        format!("/api/layers/{base}/rows?include_retired=true"),
        format!("/api/layers/{base}/overrides"),
    ] {
        ok(&e, &uri).await;
    }
    // A POST to a read route is 405, not a write.
    let (s, _) = send(
        &e.app,
        json_req(
            "POST",
            "/api/layers/summary",
            Some(&super_admin()),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(counts(&e).await, before);
}
