//! B8 authoring is an untagged platform edit; job refinements are draft design writes. These checks exercise
//! the real router and frozen release snapshots together, including the hand-off from the activity target.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::b8::support;
use crate::common::design_contract::{self, DESIGNER};
use crate::common::{self, get, insert_user, person, send, super_admin};

async fn platform(
    e: &common::TestEnv,
    method: &str,
    tail: &str,
    body: Value,
) -> (StatusCode, Value) {
    support::content(e).await;
    send(
        &e.app,
        common::json(
            method,
            &format!("/api/platform/journeys/retail{tail}?department=customer_service"),
            Some(&super_admin()),
            body,
        ),
    )
    .await
}

#[tokio::test]
async fn designer_writes_untagged_activity_bindings_and_card() {
    let e = common::env().await;
    let (status, grid) = platform(
        &e,
        "POST",
        "/activities",
        json!({
            "stage":"support", "slug":"order_question", "label":"Order question", "actor":"customer"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{grid}");
    let key = "support:order_question";
    let (status, bound) = platform(
        &e,
        "PUT",
        &format!("/activities/{key}/objects/order"),
        json!({
            "role":"subject", "fields":["status"], "required_inputs":[]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{bound}");
    let (status, grid) = platform(
        &e,
        "PUT",
        &format!("/activities/{key}/operations/order.read"),
        json!({"expectation":"R"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{grid}");
    let a = grid["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == key)
        .unwrap();
    assert_eq!(a["operations"][0]["op"], "order.read");
    assert_eq!(a["operations"][0]["required"], true);
    let (status, grid) = platform(
        &e,
        "PUT",
        &format!("/activities/{key}/card"),
        json!({"summary":"Ask about an order"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{grid}");
    let untagged: i64 = sqlx::query_scalar("SELECT count(*) FROM layer_row r JOIN layer l ON l.id=r.layer_id WHERE l.key='industry_dept/retail/customer_service' AND r.key=$1 AND r.is_current AND NOT EXISTS (SELECT 1 FROM layer_tag_row t WHERE t.row_id=r.id)")
        .bind(key).fetch_one(&e.owner).await.unwrap();
    assert_eq!(untagged, 2, "activity and card stay outside published tags");
    let actions: Vec<String> = sqlx::query_scalar(
        "SELECT action FROM platform_audit WHERE action LIKE 'journey.%' ORDER BY id",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert!(
        actions.contains(&"journey.card_update".into()),
        "{actions:?}"
    );
}

#[tokio::test]
async fn designer_rejects_invalid_fields_and_non_super_admin() {
    let e = common::env().await;
    let f = support::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    let (s, b) = send(
        &e.app,
        get(
            "/api/platform/journeys/retail?department=customer_service",
            Some(&person(DESIGNER, &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["code"], "super_admin_required");
    let (s, b) = platform(
        &e,
        "POST",
        "/activities",
        json!({"stage":"support", "slug":"bad_fields", "label":"Question", "actor":"customer"}),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    for (tail, body, code) in [
        (
            "objects/order",
            json!({"role":"subject","fields":["not_a_field"],"required_inputs":[]}),
            "unknown_field",
        ),
        (
            "operations/order.read",
            json!({"expectation":"R"}),
            "object_not_bound",
        ),
        ("card", json!({"scenarios":"invalid"}), "invalid_card"),
    ] {
        let (s, b) = platform(
            &e,
            "PUT",
            &format!("/activities/support:bad_fields/{tail}"),
            body,
        )
        .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{b}");
        assert_eq!(b["error"]["code"], code, "{b}");
    }
}

#[tokio::test]
async fn journey_draft_refinements_survive_publication_and_keep_older_release_frozen() {
    let e = common::env().await;
    let f = support::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    let token = person(DESIGNER, &[]);
    let path = format!("/api/jobs/{}/journey", f.job.job_key);
    let (s, initial) = send(&e.app, get(&path, Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "{initial}");
    assert!(
        initial["activities"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "{initial}"
    );
    assert!(initial["outcomes"].is_null());
    let rm = design_contract::release_manager(&e, &f).await;
    let first = design_contract::publish(&e, &f).await;
    let key = "support:local_question";
    let write = format!("/api/jobs/{}/refinements/journey/extensions", f.job.job_key);
    let (s,draft)=send(&e.app,common::json("POST",&write,Some(&token),json!({"stage":"support","slug":"local_question","label":"Local question","actor":"customer"}))).await;
    assert_eq!(s, StatusCode::OK, "{draft}");
    let ext = draft["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == key)
        .expect("write returns the new journey");
    assert_eq!(ext["refinement"], "extension");
    assert_eq!(ext["pending"], true);
    let (s, live) = send(&e.app, get(&format!("{path}?of=live"), Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "{live}");
    assert!(
        !live["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == key)
    );
    let card = format!(
        "/api/jobs/{}/refinements/journey/cards/{key}",
        f.job.job_key
    );
    let (s,b)=send(&e.app,common::json("PUT",&card,Some(&token),json!({"summary":"Our local procedure","sample_dialogue":[{"who":"customer","text":"Help?"}]}))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let detail = format!("{path}/activities/{key}");
    let (s, b) = send(&e.app, get(&detail, Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["use_case"]["summary"], "Our local procedure");
    assert!(
        b["use_case"]["lineage"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["kind"] == "tenant")
    );
    design_contract::cut(&e, &f, &rm, true).await;
    let (s, live) = send(&e.app, get(&format!("{path}?of=live"), Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "{live}");
    let ext = live["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["key"] == key)
        .unwrap();
    assert_eq!(ext["pending"], false);
    let (s, old) = send(&e.app, get(&format!("{path}?of={first}"), Some(&token))).await;
    assert_eq!(s, StatusCode::OK, "{old}");
    assert!(
        !old["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == key)
    );
    let stranger = support::new_job(&e).await;
    insert_user(&e.owner, "other-designer", stranger.tenant, "job_designer").await;
    let (s, own) = send(&e.app, get(&path, Some(&person("other-designer", &[])))).await;
    assert_eq!(s, StatusCode::OK, "{own}");
    assert!(
        !own["activities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["key"] == key),
        "same job keys stay tenant scoped"
    );
    sqlx::query("UPDATE jobs SET key='another_care_agent' WHERE id=$1")
        .bind(stranger.job.job_id)
        .execute(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(&e.app, get(&path, Some(&person("other-designer", &[])))).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
}

#[tokio::test]
async fn finished_test_database_is_removed_after_last_environment() {
    let e = common::env().await;
    let options = e.owner.connect_options();
    let name = options.get_database().unwrap().to_owned();
    let control = sqlx::PgPool::connect_with(options.as_ref().clone().database("postgres"))
        .await
        .unwrap();
    let other = common::TestEnv::new(e.state.clone(), e.app.clone(), e.owner.clone());
    drop(e);
    sqlx::query("SELECT 1")
        .execute(&other.owner)
        .await
        .expect("a reboot sharing the database keeps it alive");
    drop(other);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let exists: bool =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_database WHERE datname=$1)")
                    .bind(&name)
                    .fetch_one(&control)
                    .await
                    .unwrap();
            if !exists {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("completed test database removed");
    control.close().await;
}
