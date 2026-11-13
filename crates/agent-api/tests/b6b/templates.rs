//! B6b T11 templates (the KH bucket): `templateCatalog`, `templateTagDetail`, `tagLayerRevisions` over the booted
//! seed — super_admin only; one summary per template family with tags / drafts / pins / tenant lag; a tag's items,
//! diff, stack and catch-up; tagging packages the untagged drafts through `tag_family` and the AppState
//! `GeneratorGuard` (async since B6b, R12).
use std::sync::Arc;

use agent_api::error::ApiResult;
use agent_api::platform::hooks::{BoxFuture, GeneratorGuard};
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{
    TestEnv, get, insert_tenant, insert_user, json as json_req, person, send, super_admin,
};

const T: &str = "/api/platform/templates";
const CCA: &str = "job/customer_care_agent";
const CCA_SLUG: &str = "job.customer_care_agent";

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

async fn ok(e: &TestEnv, uri: &str) -> Value {
    let (s, b) = send(&e.app, get(uri, Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::OK, "{uri}: {b}");
    b
}

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, json_req(method, uri, Some(&super_admin()), body)).await
}

async fn latest(e: &TestEnv, family: &str) -> i32 {
    sqlx::query_scalar("SELECT max(version) FROM layer_tag WHERE family = $1 AND NOT provisional")
        .bind(family)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn untagged(e: &TestEnv, family: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM layer_untagged WHERE family = $1")
        .bind(family)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

/// One entity add / tombstone on the CCA head through writeLayerRow.
async fn write_cca(e: &TestEnv, body: Value) -> Value {
    let id: Uuid = sqlx::query_scalar("SELECT id FROM layer WHERE key = $1")
        .bind(CCA)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let (s, b) = sa(e, "POST", &format!("/api/layers/{id}/rows"), body).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    b
}

async fn add_gizmo(e: &TestEnv) -> Value {
    write_cca(
        e,
        json!({"kind": "entity", "key": "Gizmo", "op": "add", "fields": {"label": "Gizmo", "synonyms": ["widget"]}}),
    )
    .await
}

fn family<'a>(cat: &'a Value, key: &str) -> &'a Value {
    cat["layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["layer_key"] == key)
        .unwrap_or_else(|| panic!("family {key} in the catalog"))
}

#[tokio::test]
async fn template_routes_are_super_admin_only() {
    let e = crate::common::env().await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant a").await;
    insert_user(&e.owner, "u-ta", t, "tenant_admin").await;
    let ta = person("u-ta", &[]);
    for (method, uri) in [
        ("GET", T.to_string()),
        ("GET", format!("{T}/{CCA_SLUG}/v1")),
        ("POST", format!("{T}/{CCA_SLUG}/tags")),
    ] {
        let (s, b) = send(&e.app, json_req(method, &uri, Some(&ta), json!({}))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {uri}: {b}");
        assert_eq!(code(&b), "super_admin_required", "{method} {uri}");
    }
}

#[tokio::test]
async fn template_catalog_lists_every_template_family() {
    let e = crate::common::env().await;
    let cat = ok(&e, T).await;
    let mut heads: Vec<String> =
        sqlx::query_scalar("SELECT key FROM layer WHERE family = key AND scope <> 'profile'")
            .fetch_all(&e.owner)
            .await
            .unwrap();
    heads.sort();
    let mut got: Vec<String> = cat["layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["layer_key"].as_str().unwrap().to_owned())
        .collect();
    got.sort();
    assert_eq!(got, heads);
    let cca = family(&cat, CCA);
    assert_eq!(cca["scope"], "job");
    assert_eq!(cca["generator"], Value::Null, "null until B8");
    let versions: Vec<i64> = cca["tags"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["version"].as_i64().unwrap())
        .collect();
    let mut newest_first = versions.clone();
    newest_first.sort_by(|a, b| b.cmp(a));
    assert_eq!(versions, newest_first, "tags newest first");
    assert_eq!(cca["latest"]["version"], latest(&e, CCA).await);
    assert!(cca["latest"]["item_count"].as_u64().unwrap() > 0, "{cca}");
    assert_eq!(
        cca["draft_revisions"].as_array().unwrap().len() as i64,
        untagged(&e, CCA).await
    );
    assert!(cat["tenant_lag"].is_array());
}

#[tokio::test]
async fn catalog_pins_and_tenant_lag() {
    let e = crate::common::env().await;
    let v = latest(&e, CCA).await;
    assert!(v >= 2, "the seed tags CCA more than once");
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Tenant lag").await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'care', 'Care', $2, 'test')",
    )
    .bind(t)
    .bind(json!([{"layer": CCA, "version": v - 1}]))
    .execute(&e.owner)
    .await
    .unwrap();
    let cat = ok(&e, T).await;
    let cca = family(&cat, CCA);
    let pins = cca["pins"].as_array().unwrap();
    let mine: Vec<&Value> = pins
        .iter()
        .filter(|p| p["tenant_id"] == t.to_string())
        .collect();
    assert_eq!(mine.len(), 1, "{pins:?}");
    assert_eq!(mine[0]["job"], "care");
    assert_eq!(mine[0]["version"], v - 1);
    assert_eq!(mine[0]["behind"], 1);
    assert!(cca["behind_jobs"].as_u64().unwrap() >= 1);
    let lag = cat["tenant_lag"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tenant_id"] == t.to_string())
        .expect("the tenant's lag row");
    assert_eq!(lag["tenant_name"], "Tenant lag");
    assert_eq!(
        lag["behind"],
        json!([{"job": "care", "layer_key": CCA, "pinned": v - 1, "latest": v}])
    );
}

#[tokio::test]
async fn tag_layer_packages_drafts() {
    let e = crate::common::env().await;
    let v = latest(&e, CCA).await;
    add_gizmo(&e).await;
    let drafts = untagged(&e, CCA).await;
    assert!(drafts >= 1);
    let tags_before: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform_audit WHERE action = 'template.tag'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    let (s, b) = sa(
        &e,
        "POST",
        &format!("{T}/{CCA_SLUG}/tags"),
        json!({"note": "  "}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["layer_key"], CCA);
    assert_eq!(b["version"], v + 1);
    assert_eq!(b["provisional"], false);
    assert_eq!(b["note"], format!("{drafts} revisions packaged."));
    assert!(b["content_hash"].as_str().unwrap().starts_with("sha256:"));
    assert!(b["item_count"].as_u64().unwrap() > 0);
    assert_eq!(untagged(&e, CCA).await, 0);
    let tags_after: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform_audit WHERE action = 'template.tag'")
            .fetch_one(&e.owner)
            .await
            .unwrap();
    assert_eq!(tags_after, tags_before + 1);

    let (s, b) = sa(
        &e,
        "POST",
        &format!("{T}/{CCA_SLUG}/tags"),
        json!({"note": "again"}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "nothing_to_tag");
    let (s, b) = sa(&e, "POST", &format!("{T}/job.b6b_nobody/tags"), json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "layer_not_found");
    // a sublayer is not a template layer
    let (s, b) = sa(
        &e,
        "POST",
        &format!("{T}/job.customer_care_agent.channel=email/tags"),
        json!({}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(code(&b), "layer_not_found");
}

#[tokio::test]
async fn tag_detail_diff_and_context() {
    let e = crate::common::env().await;
    let v = latest(&e, CCA).await;
    let d = ok(&e, &format!("{T}/{CCA_SLUG}/v{v}")).await;
    assert_eq!(d["tag"]["version"], v);
    assert_eq!(d["layer"]["layer_key"], CCA);
    assert!(d["layer"].get("tags").is_none() && d["layer"].get("pins").is_none());
    let versions: Vec<i64> = d["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_i64().unwrap())
        .collect();
    let mut sorted = versions.clone();
    sorted.sort();
    assert_eq!(versions, sorted, "versions oldest first");
    assert_eq!(d["previous"], v - 1);
    assert_eq!(
        d["items"].as_array().unwrap().len() as u64,
        d["tag"]["item_count"].as_u64().unwrap()
    );

    // the diff is the tag → pin contract's `tag_diff` (same rows, same order)
    let mut c = e.state.db.acquire().await.unwrap();
    let want = agent_api::platform::templates::tag_diff(&mut c, CCA, v - 1, v)
        .await
        .unwrap();
    assert_eq!(d["diff_vs_previous"], json!(want));

    let stack = d["stack"].as_array().unwrap();
    let last = stack.last().unwrap();
    assert_eq!(last["layer_key"], CCA);
    assert_eq!(last["here"], true);
    assert_eq!(
        stack[0]["layer_key"], "base",
        "base composes under everything: {stack:?}"
    );
    assert!(stack[..stack.len() - 1].iter().all(|s| s["here"] == false));
    assert!(
        d["composed_on_by"].as_array().unwrap().is_empty(),
        "nothing composes on a job"
    );
    assert!(d["catch_up"].is_array());

    let base = ok(&e, &format!("{T}/base/v{}", latest(&e, "base").await)).await;
    let on_base = base["composed_on_by"].as_array().unwrap();
    assert!(on_base.iter().any(|k| k == CCA), "{on_base:?}");
}

#[tokio::test]
async fn tag_detail_404s() {
    let e = crate::common::env().await;
    for (uri, want) in [
        (format!("{T}/job.b6b_nobody/v1"), "layer_not_found"),
        (format!("{T}/{CCA_SLUG}/v999"), "tag_not_found"),
        (format!("{T}/{CCA_SLUG}/latest"), "tag_not_found"),
    ] {
        let (s, b) = send(&e.app, get(&uri, Some(&super_admin()))).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "{uri}: {b}");
        assert_eq!(code(&b), want, "{uri}");
    }
}

#[tokio::test]
async fn tombstoned_key_absent_from_new_tag_present_in_old_tag() {
    let e = crate::common::env().await;
    let v = latest(&e, CCA).await;
    let added = add_gizmo(&e).await;
    let (s, b) = sa(
        &e,
        "POST",
        &format!("{T}/{CCA_SLUG}/tags"),
        json!({"note": "gizmo in"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    write_cca(
        &e,
        json!({"kind": "entity", "key": "Gizmo", "op": "tombstone", "expected_rev": added["rev"]}),
    )
    .await;
    let (s, b) = sa(
        &e,
        "POST",
        &format!("{T}/{CCA_SLUG}/tags"),
        json!({"note": "gizmo out"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let has_gizmo = |d: &Value| {
        d["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["kind"] == "entity" && i["key"] == "Gizmo")
    };
    let with = ok(&e, &format!("{T}/{CCA_SLUG}/v{}", v + 1)).await;
    assert!(has_gizmo(&with));
    let without = ok(&e, &format!("{T}/{CCA_SLUG}/v{}", v + 2)).await;
    assert!(!has_gizmo(&without));
    assert!(
        without["diff_vs_previous"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["key"] == "Gizmo" && r["change"] == "removed"),
        "{}",
        without["diff_vs_previous"]
    );
}

struct Red;

impl GeneratorGuard for Red {
    fn red_items<'a>(
        &'a self,
        conn: &'a mut sqlx::PgConnection,
        family: &'a str,
    ) -> BoxFuture<'a, ApiResult<Vec<String>>> {
        Box::pin(async move {
            // the guard reads in the tagging transaction (R12): the family's drafts are visible to it
            let n: i64 =
                sqlx::query_scalar("SELECT count(*) FROM layer_untagged WHERE family = $1")
                    .bind(family)
                    .fetch_one(&mut *conn)
                    .await?;
            Ok(vec![format!(
                "{family}: {n} drafts, gate GT has no options"
            )])
        })
    }
}

#[tokio::test]
async fn tag_layer_validation_red_422() {
    let e = crate::common::env().await;
    add_gizmo(&e).await;
    let mut st = e.state.clone();
    st.generator_guard = Arc::new(Red);
    let app = agent_api::router(st);
    let (s, b) = send(
        &app,
        json_req(
            "POST",
            &format!("{T}/{CCA_SLUG}/tags"),
            Some(&super_admin()),
            json!({"note": "x"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
    assert_eq!(code(&b), "validation_red");
    let items = b["error"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert!(
        items[0]
            .as_str()
            .unwrap()
            .contains("gate GT has no options")
    );
    assert!(untagged(&e, CCA).await >= 1, "nothing was tagged");
}
