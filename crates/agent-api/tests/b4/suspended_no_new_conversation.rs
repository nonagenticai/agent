//! Live acceptance A10: a suspended tenant opens no NEW conversation. Its turns are refused at the drain (gate 2,
//! crate::turn::drain); a start is refused by the same gate (crate::turn::admit_start) — the staff test chat's
//! `POST /api/jobs/{job}/conversations` (a super_admin may act in a suspended tenant) and the customer channels'
//! `POST /api/internal/channel/inbound` are 409 `tenant_not_active` with the customer's neutral text, store no
//! `conversation` row, and share the drain's `channel_turn_refused` throttle (one row per tenant per hour). A resume
//! lets both create again.
use axum::http::StatusCode;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::conv::{self, Ctx};
use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv, json as req, send, super_admin};
use crate::redis_env as r;

const REFUSED_AUDITS: &str =
    "SELECT count(*) FROM audit_event WHERE tenant_id = $1 AND action = 'channel_turn_refused'";

async fn set_status(e: &TestEnv, tenant: Uuid, status: &str) {
    sqlx::query("UPDATE tenants SET status = $2 WHERE id = $1")
        .bind(tenant)
        .bind(status)
        .execute(&e.owner)
        .await
        .unwrap();
}

async fn conversations(e: &TestEnv, tenant: Uuid) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM conversation WHERE tenant_id = $1")
        .bind(tenant)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

async fn refused_audits(e: &TestEnv, tenant: Uuid) -> i64 {
    sqlx::query_scalar(REFUSED_AUDITS)
        .bind(tenant)
        .fetch_one(&e.owner)
        .await
        .unwrap()
}

/// `startChat` as a super_admin acting in the job's tenant.
async fn start_chat(e: &TestEnv, f: &ContractJob) -> (StatusCode, Value) {
    let mut q = req(
        "POST",
        &format!("/api/jobs/{}/conversations", f.job.job_key),
        Some(&super_admin()),
        json!({"env": "test", "customer_email": "c-1@test.example"}),
    );
    q.headers_mut()
        .insert("x-active-tenant-id", f.tenant.to_string().parse().unwrap());
    send(&e.app, q).await
}

#[tokio::test]
async fn suspended_tenant_staff_chat_start_is_refused_and_stores_nothing() {
    let vars = r::api_vars("a10-staff-start").await;
    let e = common::env_with(|v| v.extend(vars.clone())).await;
    let f = design_contract::new_job(&e).await;
    set_status(&e, f.tenant, "suspended").await;
    for _ in 0..2 {
        let (s, b) = start_chat(&e, &f).await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert_eq!(b["error"]["code"], "tenant_not_active", "{b}");
        assert_eq!(b["error"]["message"], agent_api::turn::REFUSAL_TEXT, "{b}");
    }
    assert_eq!(conversations(&e, f.tenant).await, 0, "no conversation row");
    assert_eq!(
        refused_audits(&e, f.tenant).await,
        1,
        "one channel_turn_refused per tenant per hour"
    );
    // resume: the start creates again
    set_status(&e, f.tenant, "active").await;
    let (s, b) = start_chat(&e, &f).await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    assert_eq!(b["channel"], "staff");
    assert_eq!(conversations(&e, f.tenant).await, 1);
}

/// The B12 web chat fixture (tenant Alpha, job `j1`, binding `wk_test`) on the shared Redis — D109: `j1` is live and
/// served, since a customer message reaches no fallback backend.
async fn on_redis(test: &str) -> Ctx {
    conv::setup_runtime(r::api_vars(test).await).await
}

#[tokio::test]
async fn suspended_tenant_customer_inbound_is_refused_and_upserts_no_conversation() {
    let c = on_redis("a10-inbound-start").await;
    set_status(&c.e, c.t, "suspended").await;
    for msg in ["m-1", "m-2"] {
        let (s, b) = c.inbound("wc-a10-1", msg, "Where is my order?").await;
        assert_eq!(s, StatusCode::CONFLICT, "{b}");
        assert_eq!(conv::code(&b), "tenant_not_active", "{b}");
        assert_eq!(b["error"]["message"], agent_api::turn::REFUSAL_TEXT, "{b}");
    }
    assert_eq!(
        conversations(&c.e, c.t).await,
        0,
        "no conversation upserted"
    );
    assert_eq!(c.backend.calls(), 0, "nothing answered");
    assert_eq!(
        refused_audits(&c.e, c.t).await,
        1,
        "one channel_turn_refused per tenant per hour"
    );
    // resume: the customer's message opens its conversation
    set_status(&c.e, c.t, "active").await;
    let (s, b) = c.inbound("wc-a10-2", "m-3", "Where is my order?").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(conversations(&c.e, c.t).await, 1);
}
