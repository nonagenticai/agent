//! B7 T12: tenant field-mapping overrides (0094 `field_mapping_override`, draft working rows) and the system
//! insight, on a real Postgres 18; plus the 0094 `operation_route` target-shape CHECK and RESTRICT FK.
//!
//! A save or revert runs inside B2's `design::track_tenant`: one ledger row per job of the tenant and every job's
//! `draft_rev` bumped (asserted here). The Production half of `prod_runtime_reads_snapshot_override_test_reads_working`
//! is `tests/connector_runtime.rs::prod_call_ignores_unpublished_route_change` (an unpublished override does not
//! apply in prod); the test half is `test_call_uses_draft_override` there. Not here: `errors_30d_counts_only_this_instance`
//! (B4 `operation_call`).
use crate::common;

use axum::http::StatusCode;
use common::platform::{Op, publish_template};
use common::{TestEnv, get, insert_tenant, insert_user, json, person, send};
use serde_json::{Value, json};
use uuid::Uuid;

use agent_api::connectors::overrides;

const SYS: &str = "/api/connections/systems";

/// (env, tenant, integrations-manager token, observer token).
async fn setup() -> (TestEnv, Uuid, String, String) {
    let e = common::env().await;
    let (t, im, obs) = tenant(&e, "a").await;
    (e, t, im, obs)
}

async fn tenant(e: &TestEnv, n: &str) -> (Uuid, String, String) {
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, &format!("Tenant {n}")).await;
    let im = format!("u-im-{n}");
    let obs = format!("u-obs-{n}");
    insert_user(&e.owner, &im, t, "integrations_manager").await;
    insert_user(&e.owner, &obs, t, "tenant_user").await;
    (t, person(&im, &[]), person(&obs, &[]))
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

/// The platform template the stub systems pin: `test_shop@3` and its newer `@9` (the same four operations).
async fn shop_templates(e: &TestEnv) {
    for version in ["3", "9"] {
        publish_template(
            e,
            "test_shop",
            version,
            &[
                Op::star("order", "read").returns(json!({"id": "redacted", "status": "ok"})),
                Op::star("order", "list"),
                Op::star("order", "cancel"),
                Op::new("case", "create", "return"),
            ],
        )
        .await
        .unwrap();
    }
}

/// A stub test_shop@3 system on the test env, with the order.read sample fetched (`{"id", "status"}`).
async fn stub_with_sample(e: &TestEnv, im: &str, id: &str) {
    shop_templates(e).await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            SYS,
            Some(im),
            json!({"id": id, "label": "Shop", "template_key": "test_shop", "template_version": "3",
                   "env": "test", "base_url": "", "env_ids": {}, "max_rps": null, "stub": true}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("{SYS}/{id}/samples"),
            Some(im),
            json!({"op": "order.read"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["body"]["status"], "ok", "{b}");
}

async fn put_override(
    e: &TestEnv,
    tok: &str,
    id: &str,
    op: &str,
    field: &str,
    path: &str,
) -> (StatusCode, Value) {
    send(
        &e.app,
        json(
            "PUT",
            &format!("{SYS}/{id}/mappings/{op}/{field}"),
            Some(tok),
            json!({"path": path}),
        ),
    )
    .await
}

async fn delete_override(
    e: &TestEnv,
    tok: &str,
    id: &str,
    op: &str,
    field: &str,
) -> (StatusCode, Value) {
    send(
        &e.app,
        json(
            "DELETE",
            &format!("{SYS}/{id}/mappings/{op}/{field}"),
            Some(tok),
            json!({}),
        ),
    )
    .await
}

async fn audit_count(e: &TestEnv, action: &str) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = $1")
        .bind(action)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

#[tokio::test]
async fn override_checked_against_sample() {
    let (e, _t, im, obs) = setup().await;
    stub_with_sample(&e, &im, "shop-test").await;

    // observers read, only an integrations manager writes
    let (s, b) = put_override(&e, &obs, "shop-test", "order.read", "status", "$.status").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "integrations_manager");

    // the sample is of order.read: the path must match it
    let (s, b) = put_override(&e, &im, "shop-test", "order.read", "status", "$.nope").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "path_no_match"),
        "{b}"
    );
    let (s, b) = put_override(&e, &im, "shop-test", "order.read", "status", " $.status ").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["id"], "shop-test");
    let ov = b["overrides"].as_array().expect("overrides");
    assert_eq!(ov.len(), 1, "{b}");
    assert_eq!(ov[0]["op"], "order.read");
    assert_eq!(ov[0]["field"], "status");
    assert_eq!(ov[0]["default_path"], "$.status");
    assert_eq!(ov[0]["path"], "$.status");
    assert_eq!(ov[0]["updated_by"], "u-im-a");

    // no sample of order.list: any path starting with $ is accepted, anything else is invalid_path
    let (s, b) = put_override(&e, &im, "shop-test", "order.list", "status", "status").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "invalid_path"),
        "{b}"
    );
    let (s, b) = put_override(&e, &im, "shop-test", "order.list", "status", "$.data.state").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["overrides"].as_array().unwrap().len(), 2);

    // op and field must be the pinned template's
    let (s, b) = put_override(&e, &im, "shop-test", "order.explode", "status", "$.status").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "operation_not_implemented"),
        "{b}"
    );
    let (s, b) = put_override(&e, &im, "shop-test", "order.read", "no_such", "$.status").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::UNPROCESSABLE_ENTITY, "unknown_field"),
        "{b}"
    );
    let (s, b) = put_override(&e, &im, "nope-nope", "order.read", "status", "$.status").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "instance_not_found"),
        "{b}"
    );

    // a variant op key travels percent-encoded in the path
    let (s, b) = put_override(
        &e,
        &im,
        "shop-test",
        "case.create%7Breturn%7D",
        "label",
        "$.label",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // GET (detail and list) carry the working rows; the observer reads them
    let (s, b) = send(&e.app, get(&format!("{SYS}/shop-test"), Some(&obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["instance"]["overrides"].as_array().unwrap().len(),
        3,
        "{b}"
    );
    assert!(b["names"]["u-im-a"].is_string(), "{b}");
    let (s, b) = send(&e.app, get(SYS, Some(&obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["items"][0]["overrides"].as_array().unwrap().len(),
        3,
        "{b}"
    );

    // saving the same field again replaces it
    let (s, b) = put_override(&e, &im, "shop-test", "order.read", "status", "$.id").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = b["overrides"]
        .as_array()
        .unwrap()
        .iter()
        .find(|o| o["op"] == "order.read")
        .cloned()
        .unwrap();
    assert_eq!(row["path"], "$.id");
    assert_eq!(audit_count(&e, "connector.mapping_override.save").await, 4);
}

#[tokio::test]
async fn override_is_a_tenant_draft_row() {
    let (e, t, im, _obs) = setup().await;
    stub_with_sample(&e, &im, "shop-test").await;
    let (s, b) = put_override(&e, &im, "shop-test", "order.read", "status", "$.status").await;
    assert_eq!(s, StatusCode::OK, "{b}");

    let rows: Vec<(Uuid, String, String, String)> =
        sqlx::query_as("SELECT tenant_id, instance_id, op, field FROM field_mapping_override")
            .fetch_all(&e.owner)
            .await
            .unwrap();
    assert_eq!(
        rows,
        vec![(t, "shop-test".into(), "order.read".into(), "status".into())]
    );
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'connector.mapping_override.save'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(details["draft"], "mapping_override", "{details}");
    assert_eq!(details["op"], "order.read");
    assert_eq!(details["field"], "status");

    // another tenant sees neither the system nor its overrides (RLS)
    let (_t2, im2, _) = tenant(&e, "b").await;
    let (s, b) = put_override(&e, &im2, "shop-test", "order.read", "status", "$.status").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "instance_not_found"),
        "{b}"
    );
    let (s, b) = send(&e.app, get(SYS, Some(&im2))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["items"], json!([]));

    // a job-less call (a contract test) applies the working rows on test and none on prod (never the draft)
    let mut c = e.owner.acquire().await.unwrap();
    let test = overrides::for_env(&mut c, "shop-test", "test", "order.read")
        .await
        .unwrap();
    assert_eq!(test.get("status").map(String::as_str), Some("$.status"));
    let prod = overrides::for_env(&mut c, "shop-test", "prod", "order.read")
        .await
        .unwrap();
    assert!(prod.is_empty());
}

/// A job of `t` on the latest tagged customer-care template; its id.
async fn insert_job(e: &TestEnv, t: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, template_layer, template_version, created_by) \
         SELECT $1, $2, 'Customer care', pins, layer, version, 'test' FROM job_template_tag \
          WHERE layer = 'job/customer_care_agent' AND status = 'tagged' ORDER BY version DESC LIMIT 1 \
         RETURNING id",
    )
    .bind(t)
    .bind(key)
    .fetch_one(&e.owner)
    .await
    .expect("a tagged customer-care template")
}

async fn draft_revs(e: &TestEnv, jobs: &[Uuid]) -> Vec<i64> {
    let mut out = Vec::new();
    for j in jobs {
        out.push(
            sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
                .bind(j)
                .fetch_one(&e.owner)
                .await
                .unwrap(),
        );
    }
    out
}

async fn ledger_jobs(e: &TestEnv, key: &str) -> Vec<Uuid> {
    sqlx::query_scalar(
        "SELECT job_id FROM draft_ledger WHERE key = $1 AND kind = 'mapping_override' \
            AND area = 'connections' AND page = 'T11' ORDER BY job_id",
    )
    .bind(key)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

#[tokio::test]
async fn save_and_revert_record_a_ledger_row_in_every_job_and_bump_draft_rev() {
    let (e, t, im, _obs) = setup().await;
    let mut jobs = vec![
        insert_job(&e, t, "care_one").await,
        insert_job(&e, t, "care_two").await,
    ];
    jobs.sort();
    stub_with_sample(&e, &im, "shop-test").await;
    let key = "mapping:shop-test:order.read:status";
    let before = draft_revs(&e, &jobs).await;

    let (s, b) = put_override(&e, &im, "shop-test", "order.read", "status", "$.status").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        ledger_jobs(&e, key).await,
        jobs,
        "one row per job of the tenant"
    );
    let saved = draft_revs(&e, &jobs).await;
    assert_eq!(
        saved,
        before.iter().map(|r| r + 1).collect::<Vec<_>>(),
        "every job's draft moved"
    );

    // back to the template default (the live value): the rows are dropped, the drafts move again
    let (s, b) = delete_override(&e, &im, "shop-test", "order.read", "status").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(ledger_jobs(&e, key).await.is_empty());
    assert_eq!(
        draft_revs(&e, &jobs).await,
        saved.iter().map(|r| r + 1).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn revert_unknown_404() {
    let (e, _t, im, obs) = setup().await;
    stub_with_sample(&e, &im, "shop-test").await;
    let (s, b) = delete_override(&e, &im, "shop-test", "order.read", "status").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "override_not_found"),
        "{b}"
    );
    let (s, b) = put_override(&e, &im, "shop-test", "order.read", "status", "$.status").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let (s, b) = delete_override(&e, &obs, "shop-test", "order.read", "status").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    let (s, b) = delete_override(&e, &im, "shop-test", "order.read", "status").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["overrides"], json!([]));
    assert_eq!(
        audit_count(&e, "connector.mapping_override.revert").await,
        1
    );
    let (s, b) = delete_override(&e, &im, "shop-test", "order.read", "status").await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "override_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn insight_of_a_stub_system() {
    let (e, _t, im, obs) = setup().await;
    stub_with_sample(&e, &im, "shop-test").await;
    let (s, b) = send(&e.app, get(&format!("{SYS}/shop-test/insight"), Some(&obs))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let ops = b["ops"].as_array().expect("ops");
    assert_eq!(ops.len(), 4, "test_shop@3 has 4 ops");
    let read = ops.iter().find(|o| o["op"] == "order.read").unwrap();
    assert_eq!(read["object"], "order");
    assert_eq!(read["side_effect"], "R");
    assert_eq!(read["idempotency"], "natural (read)");
    assert_eq!(read["errors_30d"], json!([]), "B4 seam");
    for f in read["fault_map"].as_array().unwrap() {
        assert!(
            ["reply", "escalate", "unlisted"].contains(&f["handling"].as_str().unwrap()),
            "{f}"
        );
    }
    // test_shop@9 is the newest active version
    assert_eq!(b["upgrade"]["from"], "3", "{}", b["upgrade"]);
    assert_eq!(b["upgrade"]["to"], "9", "{}", b["upgrade"]);
    assert_eq!(b["vendor_api_pin"], "test pin");
    assert!(b["taxonomy"].is_array());
    assert!(b["webhooks"].is_array());
    let (s, b) = send(&e.app, get(&format!("{SYS}/nope-nope/insight"), Some(&obs))).await;
    assert_eq!(
        (s, code(&b)),
        (StatusCode::NOT_FOUND, "instance_not_found"),
        "{b}"
    );
}

#[tokio::test]
async fn insight_upgrade_is_computed_from_the_two_versions() {
    let (e, t, im, _obs) = setup().await;
    // v1 -> v2 only adds an operation
    publish_template(
        &e,
        "test_shop_u",
        "1",
        &[Op::star("order", "read"), Op::star("order", "cancel")],
    )
    .await
    .unwrap();
    publish_template(
        &e,
        "test_shop_u",
        "2",
        &[
            Op::star("order", "read"),
            Op::star("order", "cancel"),
            Op::new("case", "create", "return"),
        ],
    )
    .await
    .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            SYS,
            Some(&im),
            json!({"id": "shop-prod", "label": "Shop", "template_key": "test_shop_u",
                   "template_version": "1", "env": "prod", "base_url": "https://shop.example.com",
                   "env_ids": {}, "max_rps": null, "stub": false}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let (s, b) = send(&e.app, get(&format!("{SYS}/shop-prod/insight"), Some(&im))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let up = &b["upgrade"];
    assert_eq!(up["from"], "1");
    assert_eq!(up["to"], "2");
    assert_eq!(
        up["verdict"],
        json!({"kind": "can_upgrade", "to": "2", "warnings": ["T3 contract tests re-run on re-pin"]})
    );

    // a job routing an op the target drops blocks the upgrade (working route rows, 0094)
    let job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'care', 'Care', '[]', 'test') \
         RETURNING id",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, instance_id, updated_by) \
         VALUES ($1, $2, 'order', 'explode', '*', 'prod', 'instance', 'shop-prod', 'test')",
    )
    .bind(t)
    .bind(job)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, b) = send(&e.app, get(&format!("{SYS}/shop-prod/insight"), Some(&im))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["upgrade"]["verdict"],
        json!({"kind": "blocked", "to": "2",
               "reasons": ["@2 drops order.explode, which a job routes here"]})
    );
}

#[tokio::test]
async fn operation_route_target_shape_and_restrict() {
    let (e, t, im, _obs) = setup().await;
    stub_with_sample(&e, &im, "shop-test").await;
    let job: Uuid = sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'care', 'Care', '[]', 'test') \
         RETURNING id",
    )
    .bind(t)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let insert = |verb: &'static str,
                  kind: &'static str,
                  instance: Option<&'static str>,
                  url: Option<&'static str>| {
        sqlx::query(
            "INSERT INTO operation_route \
                    (tenant_id, job_id, object, verb, variant, env, target_kind, instance_id, url, updated_by) \
             VALUES ($1, $2, 'order', $3, '*', 'test', $4, $5, $6, 'test')",
        )
        .bind(t)
        .bind(job)
        .bind(verb)
        .bind(kind)
        .bind(instance)
        .bind(url)
        .execute(&e.owner)
    };
    for (verb, kind, instance, url) in [
        ("a", "instance", None, None),
        ("b", "builtin", Some("shop-test"), None),
        ("c", "deep_link", None, Some("http://shop.example.com/x")),
        ("d", "deep_link", None, None),
        ("e", "elsewhere", None, None),
    ] {
        let err = insert(verb, kind, instance, url)
            .await
            .expect_err(&format!("{verb} {kind} must be refused"));
        assert_eq!(
            common::sqlstate(&err).as_deref(),
            Some("23514"),
            "{verb}: {err}"
        );
    }
    insert("read", "instance", Some("shop-test"), None)
        .await
        .unwrap();
    insert(
        "list",
        "deep_link",
        None,
        Some("https://shop.example.com/orders"),
    )
    .await
    .unwrap();
    insert("cancel", "not_offered", None, None).await.unwrap();

    // a routed system cannot be deleted from under its route; nor can one with an override
    let err =
        sqlx::query("DELETE FROM connector_instance WHERE tenant_id = $1 AND id = 'shop-test'")
            .bind(t)
            .execute(&e.owner)
            .await
            .expect_err("RESTRICT");
    // ON DELETE RESTRICT raises restrict_violation (23001), not foreign_key_violation (23503)
    assert_eq!(common::sqlstate(&err).as_deref(), Some("23001"), "{err}");
}
