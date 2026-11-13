//! B1 T10: the tenant registry, portfolio, one tenant and its users, licence, provisioning rows, stored offboard and
//! footprint (crate::routes::tenants). Real Postgres 18 (one container for this binary).
use crate::common;
use crate::redis_env;

use axum::http::StatusCode;
use common::{get, insert_tenant, insert_user, person, send, super_admin};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

async fn insert_job(db: &PgPool, tenant: Uuid, key: &str) {
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, $2, '[]', 'test')",
    )
    .bind(tenant)
    .bind(key)
    .execute(db)
    .await
    .unwrap();
}

async fn insert_row(db: &PgPool, tenant: Uuid, target: &str, key: &str, status: &str) {
    let op = if target == "licences" {
        "licence_ensure"
    } else {
        "store_ensure"
    };
    sqlx::query(
        "INSERT INTO provision_outbox (tenant_id, target, op, dedupe_key, status, last_error_code, created_by) \
         VALUES ($1, $2, $3, $4, $5::provision_status, \
                 CASE WHEN $5 IN ('failed', 'dead') THEN 'upstream_5xx' END, 'test')",
    )
    .bind(tenant)
    .bind(target)
    .bind(op)
    .bind(key)
    .bind(status)
    .execute(db)
    .await
    .unwrap();
}

async fn set_user(db: &PgPool, sub: &str, set: &str) {
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "UPDATE users SET {set} WHERE sub = $1"
    )))
    .bind(sub)
    .execute(db)
    .await
    .unwrap();
}

fn item(body: &Value, id: Uuid) -> &Value {
    body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == id.to_string())
        .unwrap_or_else(|| panic!("{id} not listed: {body}"))
}

fn portfolio_of(rows: &[Value], t: Uuid) -> &Value {
    rows.iter()
        .find(|r| r["tenant_id"] == t.to_string())
        .unwrap_or_else(|| panic!("{t} not in the portfolio"))
}

#[tokio::test]
async fn registry_counts_jobs_users_and_worst_badge() {
    let e = common::env().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_tenant(&e.owner, b, "Beta").await;
    insert_job(&e.owner, a, "faq_agent").await;
    insert_job(&e.owner, a, "billing").await;
    insert_user(&e.owner, "u-admin", a, "tenant_admin").await;
    insert_user(&e.owner, "u-user", a, "tenant_user").await;
    insert_user(&e.owner, "u-off", a, "tenant_user").await;
    insert_user(&e.owner, "u-new", a, "tenant_user").await;
    set_user(&e.owner, "u-off", "disabled_at = now()").await;
    set_user(&e.owner, "u-new", "invited_at = now()").await;
    insert_row(&e.owner, a, "strata_rag", "k1", "pending").await;
    insert_row(&e.owner, a, "strata_ingest", "k2", "failed").await;
    insert_row(&e.owner, a, "licences", "k3", "dead").await;
    insert_row(&e.owner, a, "licences", "k4", "dead").await;
    insert_row(&e.owner, b, "strata_rag", "k1", "succeeded").await;

    let (s, body) = send(&e.app, get("/api/tenants", Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let ra = item(&body, a);
    assert_eq!(ra["name"], "Alpha");
    assert_eq!(ra["jobs"], 2);
    assert_eq!(ra["users"], 3, "enabled members, invited included: {ra}");
    assert_eq!(ra["provisioning"], "dead");
    assert_eq!(ra["provisioning_count"], 2);
    assert_eq!(ra["residency"], "eu");
    assert_eq!(ra["licence"]["status"], "none");
    let rb = item(&body, b);
    assert_eq!(rb["jobs"], 0);
    assert_eq!(rb["provisioning"], "ok");
    assert_eq!(rb["provisioning_count"], 0);
    assert!(body["next"].is_null());

    // failed beats pending once nothing is dead
    sqlx::query("UPDATE provision_outbox SET status = 'cancelled' WHERE status = 'dead'")
        .execute(&e.owner)
        .await
        .unwrap();
    let (_, body) = send(&e.app, get("/api/tenants", Some(&super_admin()))).await;
    assert_eq!(item(&body, a)["provisioning"], "failed");
    assert_eq!(item(&body, a)["provisioning_count"], 1);
}

#[tokio::test]
async fn archived_hidden_unless_included() {
    let e = common::env().await;
    let (live, gone) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, live, "Live").await;
    insert_tenant(&e.owner, gone, "Gone").await;
    sqlx::query("UPDATE tenants SET status = 'archived', archived_at = now() WHERE id = $1")
        .bind(gone)
        .execute(&e.owner)
        .await
        .unwrap();
    let ids = |b: &Value| -> Vec<String> {
        b["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    let (_, body) = send(&e.app, get("/api/tenants", Some(&super_admin()))).await;
    assert_eq!(ids(&body), [live.to_string()]);
    let (_, body) = send(
        &e.app,
        get("/api/tenants?include_archived=true", Some(&super_admin())),
    )
    .await;
    assert_eq!(ids(&body).len(), 2);
    assert_eq!(item(&body, gone)["status"], "archived");
    let (s, body) = send(
        &e.app,
        get("/api/tenants?include_archived=maybe", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

/// The portfolio's `on_bus` is the bus fact the job status reports: true when the bus answers, false when it does not
/// (here: no `REDIS_URL` at all, and an address nothing listens on).
#[tokio::test]
async fn portfolio_on_bus_follows_the_bus() {
    let vars = redis_env::api_vars("tenants-portfolio-bus").await;
    let up = common::env_with(|v| v.extend(vars.clone())).await;
    let dead = redis_env::dead_addr().await;
    let down = common::env_with(|v| {
        v.insert("REDIS_URL".to_string(), format!("redis://{dead}"));
    })
    .await;
    for (e, want) in [(up, true), (down, false)] {
        let t = Uuid::new_v4();
        insert_tenant(&e.owner, t, "Bus").await;
        insert_job(&e.owner, t, "faq_agent").await;
        for path in [
            "/api/tenants?view=portfolio".to_string(),
            format!("/api/tenants/{t}/footprint"),
        ] {
            let (s, body) = send(&e.app, get(&path, Some(&super_admin()))).await;
            assert_eq!(s, StatusCode::OK, "{body}");
            let p = if body.is_array() {
                portfolio_of(body.as_array().unwrap(), t).clone()
            } else {
                body["portfolio"].clone()
            };
            assert_eq!(p["jobs"][0]["on_bus"], want, "{path}: {body}");
        }
    }
}

#[tokio::test]
async fn portfolio_stage_provisioning_while_rows_open() {
    let e = common::env().await;
    let (busy, done) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, busy, "Busy").await;
    insert_tenant(&e.owner, done, "Done").await;
    insert_user(&e.owner, "u-owner", busy, "tenant_admin").await;
    insert_job(&e.owner, busy, "faq_agent").await;
    insert_row(&e.owner, busy, "strata_rag", "k1", "pending").await;
    insert_row(&e.owner, done, "strata_rag", "k1", "succeeded").await;

    let (s, body) = send(
        &e.app,
        get("/api/tenants?view=portfolio", Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let rows = body.as_array().expect("portfolio is an array");
    let p = portfolio_of(rows, busy);
    assert_eq!(p["stage"], "provisioning");
    assert_eq!(p["owners"][0]["sub"], "u-owner");
    assert_eq!(p["members"]["admins"], 1);
    assert_eq!(p["jobs"][0]["key"], "faq_agent");
    assert_eq!(p["jobs"][0]["on_bus"], false);
    let rag = p["provision"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["target"] == "strata_rag")
        .unwrap();
    assert_eq!(rag["status"], "pending");
    assert!(
        p["provision"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["target"] == "licences" && r["status"].is_null())
    );
    assert!(
        p["attention"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["kind"] == "provisioning")
    );
    assert_eq!(portfolio_of(rows, done)["stage"], "active");

    // once the row succeeded the stage is the tenant status
    sqlx::query("UPDATE provision_outbox SET status = 'succeeded' WHERE tenant_id = $1")
        .bind(busy)
        .execute(&e.owner)
        .await
        .unwrap();
    let (_, body) = send(
        &e.app,
        get("/api/tenants?view=portfolio", Some(&super_admin())),
    )
    .await;
    assert_eq!(
        portfolio_of(body.as_array().unwrap(), busy)["stage"],
        "active"
    );
    let (s, body) = send(&e.app, get("/api/tenants?view=nope", Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "invalid_view");
}

#[tokio::test]
async fn tenant_read_member_own_only() {
    let e = common::env().await;
    let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_tenant(&e.owner, b, "Beta").await;
    insert_user(&e.owner, "u-a", a, "tenant_user").await;
    let own = format!("/api/tenants/{a}");
    let (s, body) = send(&e.app, get(&own, Some(&person("u-a", &[])))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["id"], a.to_string());
    assert_eq!(body["residency"], "eu");
    assert_eq!(body["industry"], "unassigned");
    assert_eq!(body["licence"]["status"], "none");
    let (s, body) = send(
        &e.app,
        get(&format!("/api/tenants/{b}"), Some(&person("u-a", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "forbidden");
    assert_eq!(
        send(&e.app, get(&own, Some(&person("u-nobody", &[]))))
            .await
            .1["error"]["code"],
        "not_provisioned"
    );
    let (s, _) = send(
        &e.app,
        get(&format!("/api/tenants/{b}"), Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, body) = send(
        &e.app,
        get(
            &format!("/api/tenants/{}", Uuid::new_v4()),
            Some(&super_admin()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "tenant_not_found");
    let (s, body) = send(&e.app, get("/api/tenants/nope", Some(&super_admin()))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_tenant_id");
}

#[tokio::test]
async fn users_list_includes_disabled_and_invited() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_user(&e.owner, "u-admin", a, "tenant_admin").await;
    insert_user(&e.owner, "u-off", a, "tenant_user").await;
    insert_user(&e.owner, "u-new", a, "tenant_user").await;
    set_user(
        &e.owner,
        "u-off",
        "disabled_at = now(), email = 'off@example.test', title = 'Agent'",
    )
    .await;
    set_user(&e.owner, "u-new", "invited_at = now()").await;
    let (s, body) = send(
        &e.app,
        get(
            &format!("/api/tenants/{a}/users"),
            Some(&person("u-admin", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    let off = items.iter().find(|u| u["sub"] == "u-off").unwrap();
    assert!(off["disabled_at"].is_string());
    assert_eq!(off["email"], "off@example.test");
    assert_eq!(off["title"], "Agent");
    assert_eq!(off["tenant_id"], a.to_string());
    assert!(off["created_at"].is_string());
    let new = items.iter().find(|u| u["sub"] == "u-new").unwrap();
    assert!(new["invited_at"].is_string());
    assert!(new["disabled_at"].is_null());
    assert_eq!(new["email"], "", "a missing address reads as empty");
}

#[tokio::test]
async fn licence_never_returns_the_token() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_user(&e.owner, "u-a", a, "tenant_admin").await;
    let secret = "v2:gone:c2VhbGVkLXRva2VuLW5ldmVyLXNlbnQtdG8tYS1jbGllbnQ";
    sqlx::query(
        "UPDATE tenants SET licence_id = 'lic-tenant-1', licence_token_encrypted = $2 WHERE id = $1",
    )
    .bind(a)
    .bind(secret)
    .execute(&e.owner)
    .await
    .unwrap();
    let (s, body) = send(
        &e.app,
        get(&format!("/api/tenants/{a}/licence"), Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["licence"]["licence_id"], "lic-tenant-1");
    assert_eq!(body["licence"]["app"], "agent");
    assert_eq!(body["verdict"]["status"], "active");
    assert_eq!(body["verdict"]["fails_closed"], false);
    for uri in [
        format!("/api/tenants/{a}/licence"),
        format!("/api/tenants/{a}"),
        "/api/tenants".to_string(),
        format!("/api/tenants/{a}/footprint"),
    ] {
        let (s, body) = send(&e.app, get(&uri, Some(&super_admin()))).await;
        assert_eq!(s, StatusCode::OK, "{uri}: {body}");
        let text = body.to_string();
        assert!(
            !text.contains(secret) && !text.contains("c2VhbGVk"),
            "{uri} leaked the token: {text}"
        );
    }
    let (_, body) = send(
        &e.app,
        get(&format!("/api/tenants/{a}"), Some(&person("u-a", &[]))),
    )
    .await;
    assert_eq!(body["licence"]["status"], "active");
    assert!(!body.to_string().contains("c2VhbGVk"));
    // no licence stored: both null
    let b = Uuid::new_v4();
    insert_tenant(&e.owner, b, "Beta").await;
    let (_, body) = send(
        &e.app,
        get(&format!("/api/tenants/{b}/licence"), Some(&super_admin())),
    )
    .await;
    assert!(
        body["licence"].is_null() && body["verdict"].is_null(),
        "{body}"
    );
}

#[tokio::test]
async fn footprint_history_filters_lifecycle_actions() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    for (action, status) in [
        ("tenant.create", "success"),
        ("user.add", "success"),
        ("execute", "success"),
        ("provision.failed", "failure"),
        ("connection.test", "success"),
        ("job.create", "success"),
    ] {
        sqlx::query(
            "INSERT INTO audit_event (tenant_id, actor_id, actor_type, action, resource_type, resource_id, status) \
             VALUES ($1, 'u-super', 'human', $2, 'x', 'r-1', $3)",
        )
        .bind(a)
        .bind(action)
        .bind(status)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let (s, body) = send(
        &e.app,
        get(&format!("/api/tenants/{a}/footprint"), Some(&super_admin())),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let actions: Vec<&str> = body["history"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["action"].as_str().unwrap())
        .collect();
    assert_eq!(
        actions,
        [
            "job.create",
            "provision.failed",
            "user.add",
            "tenant.create"
        ],
        "newest first, lifecycle actions only"
    );
    let failed = &body["history"][1];
    assert_eq!(failed["ok"], false);
    assert_eq!(body["tenant_id"], a.to_string());
    assert_eq!(body["portfolio"]["tenant_id"], a.to_string());
    assert_eq!(body["slots"], serde_json::json!([]));
    assert!(body["bundle"].is_null());
    let (s, body) = send(
        &e.app,
        get(
            &format!("/api/tenants/{}/footprint", Uuid::new_v4()),
            Some(&super_admin()),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "tenant_not_found");
}

#[tokio::test]
async fn platform_reads_need_super_admin() {
    let e = common::env().await;
    let a = Uuid::new_v4();
    insert_tenant(&e.owner, a, "Alpha").await;
    insert_user(&e.owner, "u-admin", a, "tenant_admin").await;
    insert_job(&e.owner, a, "faq_agent").await;
    insert_row(&e.owner, a, "strata_rag", "k1", "failed").await;
    let admin = person("u-admin", &[]);
    for uri in [
        "/api/tenants".to_string(),
        "/api/tenants?view=portfolio".to_string(),
        format!("/api/tenants/{a}/jobs"),
        format!("/api/tenants/{a}/licence"),
        format!("/api/tenants/{a}/provisioning"),
        format!("/api/tenants/{a}/offboard"),
        format!("/api/tenants/{a}/footprint"),
    ] {
        let (s, body) = send(&e.app, get(&uri, Some(&admin))).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{uri}: {body}");
        assert_eq!(body["error"]["code"], "super_admin_required", "{uri}");
    }
    let sa = super_admin();
    let (s, body) = send(&e.app, get(&format!("/api/tenants/{a}/jobs"), Some(&sa))).await;
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["key"], "faq_agent");
    assert_eq!(body["items"][0]["current_release"], Value::Null);
    let (s, body) = send(
        &e.app,
        get(&format!("/api/tenants/{a}/provisioning"), Some(&sa)),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{body}");
    let row = &body[0];
    assert_eq!(row["status"], "failed");
    assert_eq!(row["last_error"]["code"], "upstream_5xx");
    assert!(row.get("payload").is_none());
    let (s, body) = send(
        &e.app,
        get(&format!("/api/tenants/{a}/offboard"), Some(&sa)),
    )
    .await;
    // B1b T9: never offboarded = 200 null; an unknown tenant stays 404
    assert_eq!(s, StatusCode::OK, "{body}");
    assert_eq!(body, Value::Null);
    let ghost = Uuid::new_v4();
    let (s, body) = send(
        &e.app,
        get(&format!("/api/tenants/{ghost}/offboard"), Some(&sa)),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], "tenant_not_found");
}
