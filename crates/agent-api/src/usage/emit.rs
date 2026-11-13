//! `usage::emit`: one `usage_event` ledger row per metered call (0064), plus its `metering_outbox` row when
//! `METERING_EMIT_ENABLED` is on, both on the caller's tenant-scoped (and, for a job, job-scoped) connection so they
//! commit together or not at all.
//!
//! Attribution comes from `agent_licence_attribution(actor)` (a member's active entitlement, else the tenant licence).
//! Money path, fail loud: a ledger write that fails is the caller's error. A cost event billing could not attribute
//! (no licence id) is never sent: with emit on it is refused with an error log and the
//! `agent_metering_unattributed_total` counter, and the ledger row keeps the usage for a later replay.
use std::collections::BTreeMap;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::json;
use sqlx::PgConnection;
use uuid::Uuid;

use super::{COST_SOURCE, LlmUsage, MeteringConfig};
use crate::error::ApiResult;

/// The sources of 0064's `usage_event_source_check`.
pub const SOURCES: [&str; 6] = [
    "turn",
    "staff_test",
    "generator",
    "discovery",
    "validation",
    "gate",
];

/// One metered call.
#[derive(Clone, Debug)]
pub struct UsageEvent<'a> {
    /// `None` only for a run outside any job (a B8 generator run); the connection must then carry no job scope.
    pub job_id: Option<Uuid>,
    /// One of [`SOURCES`].
    pub source: &'static str,
    /// The metering label (`qualify`, `reply`, `assistant`, …): `^[a-z][a-z0-9_]{0,39}$`.
    pub use_: &'static str,
    /// The qualified activity the call served, when known.
    pub activity: Option<&'a str>,
    pub conversation_id: Option<&'a str>,
    pub case_id: Option<&'a str>,
    /// A staff sub, or `callers:<channel>`.
    pub actor: &'a str,
    pub usage: &'a LlmUsage,
}

/// Write the ledger row (and, with emit on, its outbox row) on `conn`, which must be scoped to the tenant (and to
/// `e.job_id`'s job when it has one). Returns the row's `event_key`.
pub async fn emit(
    conn: &mut PgConnection,
    metering: &MeteringConfig,
    e: &UsageEvent<'_>,
) -> ApiResult<Uuid> {
    let lic: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT license_id, subject_id FROM agent_licence_attribution($1)")
            .bind(e.actor)
            .fetch_optional(&mut *conn)
            .await?;
    let (license_id, subject_id) = lic.unwrap_or_default();
    let key = Uuid::now_v7();
    // no RETURNING: the runtime role may INSERT usage_event but not SELECT it (0065); `now()` is the transaction
    // timestamp, i.e. exactly the row's `occurred_at` default
    sqlx::query(
        "INSERT INTO usage_event (tenant_id, job_id, event_key, source, use, activity, conversation_id, case_id, \
                                  actor, provider_id, model, funding, tokens_in, tokens_out, calls, latency_ms, \
                                  license_id, subject_id) \
         VALUES (app_current_tenant(), $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, 1, $14, $15, $16)",
    )
    .bind(e.job_id)
    .bind(key)
    .bind(e.source)
    .bind(e.use_)
    .bind(e.activity)
    .bind(e.conversation_id)
    .bind(e.case_id)
    .bind(e.actor)
    .bind(&e.usage.provider_id)
    .bind(&e.usage.model)
    .bind(&e.usage.funding)
    .bind(e.usage.tokens_in.max(0))
    .bind(e.usage.tokens_out.max(0))
    .bind(e.usage.latency_ms.max(0))
    .bind(license_id.as_deref())
    .bind(subject_id.as_deref())
    .execute(&mut *conn)
    .await?;
    let (tenant, at): (Uuid, DateTime<Utc>) = sqlx::query_as("SELECT app_current_tenant(), now()")
        .fetch_one(&mut *conn)
        .await?;
    if !metering.emit_enabled {
        return Ok(key);
    }
    let Some(license_id) = license_id.filter(|l| !l.trim().is_empty()) else {
        metrics::counter!("agent_metering_unattributed_total", "source" => e.source).increment(1);
        tracing::error!(
            tenant = %tenant, event_key = %key, source = e.source, use_ = e.use_,
            "metering_unattributed: no licence id for this usage; the cost event is not sent (the ledger row keeps it)"
        );
        return Ok(key);
    };
    let job_key: Option<String> = match e.job_id {
        Some(j) => {
            sqlx::query_scalar("SELECT key FROM jobs WHERE id = $1")
                .bind(j)
                .fetch_optional(&mut *conn)
                .await?
        }
        None => None,
    };
    let fields = cost_event_fields(&CostEvent {
        event_key: key,
        at,
        tenant,
        job_key: job_key.as_deref().unwrap_or_default(),
        license_id: &license_id,
        subject_id: subject_id.as_deref().unwrap_or_default(),
        e,
    });
    sqlx::query("INSERT INTO metering_outbox (tenant_id, job_id, event_key, fields) VALUES ($1, $2, $3, $4)")
        .bind(tenant)
        .bind(e.job_id)
        .bind(key)
        .bind(json!(fields))
        .execute(&mut *conn)
        .await?;
    Ok(key)
}

/// What one cost event is built from.
pub struct CostEvent<'a> {
    pub event_key: Uuid,
    pub at: DateTime<Utc>,
    pub tenant: Uuid,
    pub job_key: &'a str,
    pub license_id: &'a str,
    pub subject_id: &'a str,
    pub e: &'a UsageEvent<'a>,
}

/// The flat string map of one event on `billing:costs` (EMITTER_GUIDE.md "The event shape"). USD is billing's to
/// compute from its pricing, so `cost_usd` is `0`. `funding`, `project` (the tenant id: a projectless app), `actor_id`
/// and `app_id` are always set (owner direction 2026-10-07).
pub fn cost_event_fields(c: &CostEvent<'_>) -> BTreeMap<String, String> {
    let s = |v: &str| v.to_string();
    let u = c.e.usage;
    let tenant = c.tenant.to_string();
    BTreeMap::from([
        (s("tenant_id"), tenant.clone()),
        (s("project"), tenant),
        (s("job"), s(c.job_key)),
        (s("license_id"), s(c.license_id)),
        (s("subject_id"), s(c.subject_id)),
        (s("actor_id"), s(c.e.actor)),
        (
            s("correlation_id"),
            s(c.e.conversation_id.unwrap_or_default()),
        ),
        (s("stage"), s(c.e.use_)),
        (s("app_id"), s(COST_SOURCE)),
        (s("service_key"), s(COST_SOURCE)),
        (s("cost_source"), s(COST_SOURCE)),
        (s("funding"), u.funding.clone()),
        (s("provider"), u.provider_id.clone()),
        (s("model"), u.model.clone()),
        (s("served_model"), u.model.clone()),
        (s("input_tokens"), u.tokens_in.max(0).to_string()),
        (s("output_tokens"), u.tokens_out.max(0).to_string()),
        (s("cache_creation_input_tokens"), s("0")),
        (s("cache_read_input_tokens"), s("0")),
        (s("cost_usd"), s("0.00000000")),
        (s("latency_ms"), u.latency_ms.max(0).to_string()),
        (s("provider_request_id"), String::new()),
        (s("event_key"), c.event_key.to_string()),
        (
            s("usage_extra"),
            json!({
                "job": c.job_key,
                "source": c.e.source,
                "activity": c.e.activity,
                "case_id": c.e.case_id,
            })
            .to_string(),
        ),
        (
            s("timestamp"),
            c.at.to_rfc3339_opts(SecondsFormat::Millis, true),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_event_carries_attribution_funding_and_the_ledger_key() {
        let usage = LlmUsage {
            provider_id: "internal".into(),
            model: "local-llm".into(),
            funding: "local_gpu".into(),
            tokens_in: 812,
            tokens_out: 64,
            latency_ms: 900,
        };
        let e = UsageEvent {
            job_id: Some(Uuid::now_v7()),
            source: "staff_test",
            use_: "qualify",
            activity: None,
            conversation_id: Some("conv-1"),
            case_id: None,
            actor: "u-staff",
            usage: &usage,
        };
        let key = Uuid::now_v7();
        let tenant = Uuid::now_v7();
        let f = cost_event_fields(&CostEvent {
            event_key: key,
            at: Utc::now(),
            tenant,
            job_key: "customer_care",
            license_id: "lic-1",
            subject_id: "",
            e: &e,
        });
        assert_eq!(f["tenant_id"], tenant.to_string());
        assert_eq!(f["project"], tenant.to_string());
        assert_eq!(f["event_key"], key.to_string());
        assert_eq!(f["funding"], "local_gpu");
        assert_eq!(f["actor_id"], "u-staff");
        assert_eq!(f["license_id"], "lic-1");
        assert_eq!(f["input_tokens"], "812");
        assert_eq!(f["cost_source"], "agent");
        let extra: serde_json::Value = serde_json::from_str(&f["usage_extra"]).unwrap();
        assert_eq!(extra["source"], "staff_test");
    }
}
