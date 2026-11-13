//! T13 `GET /api/tenant/channels` (`ChannelsView`, web/src/lib/api/types/tenant.ts): the doors customers use to reach
//! the acting tenant's agent, with gate 2 and the refusal customers get. Any member; read only.
//!
//! - Every **customer-side channel binding** of every job (crate::conversations::bindings, `connector_bindings`): `live`
//!   when enabled and verified, else `not_ready`; `answered_by` = its job and that job's current release (live only);
//!   `webhook` = what agent-edge last reported (`channel_webhook_status`, via `bindings::webhook_view`, incl.
//!   `pending_update_count` and `down_reason` from the edge's webhook-info report) or `null`. No webhook URL: it is never
//!   stored. `conversations_7d` = the job's conversations of that kind and env in the last 7 days: a conversation
//!   names no binding (`conversation` has no `binding_id`), so the count is per job + kind + env. It sits on the first
//!   binding of that key; any further binding of the same key gets `null` and says so in its `note` (no double count).
//!   `edge_host` = the web chat widget host (`WEBCHAT_PUBLIC_BASE`); `null` for Telegram (the edge sets its webhook
//!   and the API stores neither URL nor host). `widget_key` = a web chat binding's public `wk_` key (its
//!   `external_id`, embedded in the customer's site HTML anyway) when the binding is live (enabled and verified), so
//!   "Try it" opens `<edge_host>/webchat/demo?key=<widget_key>` (the per-job Channels page's `webChatDemoUrl`); `null`
//!   for every other row. No credential, webhook secret or other secret is ever returned.
//! - The **staff test chat** (always live, test env; `conversations_7d` counts its test-env conversations), answered by the first job, `also_answers` the others.
//! - A static catalogue for the rest: a customer kind no job has bound is `available` (set up on a job's Channels
//!   page); voice is `later` (no inbound phone door).
//! - `gate` = the stored licence verdict (crate::licensing::verdict) in the tenant `LicenceVerdict` shape, read the
//!   way gate 2 reads it: a platform-owned tenant is `active`; no verdict row is `none`/`verdict_missing`; an `active`
//!   older than `MAX_AGE_SECONDS` is `inactive`/`verdict_stale`. `tenant_status` = `tenants.status`.
//! - `refusal_text` = crate::turn::REFUSAL_TEXT; `refusals_24h` = `channel_turn_refused` audit rows of the last 24 h
//!   (written at most once per tenant per hour, so it counts refusing hours, not refused messages).
//! - `job_operations` is empty and `operations_job` null: no per-channel identity coverage is served yet.
use std::collections::HashMap;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

use crate::conversations::bindings::{public_base, webhook_view};
use crate::conversations::iso;
use crate::error::ApiResult;
use crate::licensing::verdict::{self, MAX_AGE_SECONDS, Verdict};
use crate::routes::tenants::plan;
use crate::state::AppState;
use crate::tenancy::Member;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/tenant/channels", get(tenant_channels))
}

/// The binding kinds a job can bind on the customer side (crate::conversations::BINDING_KINDS without the
/// worker-only `helpdesk`), in catalogue order.
pub const CUSTOMER_KINDS: [&str; 2] = ["telegram", "web_chat"];

/// The verdict statuses the tenant `LicenceVerdict` knows.
const STATUSES: [&str; 5] = ["active", "inactive", "revoked", "expired", "none"];

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "telegram" => "Telegram bot",
        "web_chat" => "Customer web chat",
        "helpdesk" => "Helpdesk",
        _ => "Channel",
    }
}

/// `(identity_start, identity_ceiling, identity_how)` of a door (10 §3.1).
fn identity(kind: &str) -> (&'static str, &'static str, &'static str) {
    match kind {
        "telegram" => (
            "1",
            "2",
            "Only allow-listed Telegram users get in, so a chat starts identified.",
        ),
        "web_chat" => (
            "0",
            "2",
            "An anonymous visitor starts public; a signed assertion from the tenant's site identifies them.",
        ),
        "staff_chat" => (
            "1",
            "2",
            "Staff play the demo customer, so a turn starts identified.",
        ),
        _ => (
            "0",
            "3",
            "Caller id gives identified; a PIN or OTP on the call would reach step-up.",
        ),
    }
}

/// One job of the tenant with its current release.
struct Job {
    id: Uuid,
    key: String,
    label: String,
    release_id: Option<String>,
    release_hash: Option<String>,
}

/// `ChannelAnsweredBy` of `job` in `env`.
fn answered(job: &Job, env: &str) -> Value {
    json!({
        "job": job.key,
        "job_label": job.label,
        "release_id": job.release_id,
        "release_hash": job.release_hash,
        "env": env,
    })
}

/// The tenant `LicenceVerdict` of gate 2: `owned` = nonagenticai's own tenant (D70, always admitted), `v` the stored
/// verdict (`None` = never written), `plan` the deployment plan when the verdict names none.
pub fn gate_json(v: Option<&Verdict>, owned: bool, plan: &str) -> Value {
    let (status, reason) = if owned {
        ("active".to_string(), None)
    } else {
        match v {
            None => ("none".to_string(), Some("verdict_missing".to_string())),
            Some(x) if x.status == "active" && x.age_seconds > MAX_AGE_SECONDS => {
                ("inactive".to_string(), Some("verdict_stale".to_string()))
            }
            Some(x) if STATUSES.contains(&x.status.as_str()) => {
                (x.status.clone(), x.reason.clone())
            }
            Some(_) => ("inactive".to_string(), Some("unknown_status".to_string())),
        }
    };
    json!({
        "status": status,
        "app": "agent",
        "plan": v.and_then(|x| x.plan.clone()).unwrap_or_else(|| plan.to_string()),
        "kid": v.and_then(|x| x.kid.clone()),
        "licence_id": v.and_then(|x| x.licence_id.clone()),
        "holder_sub": Value::Null,
        "checked_at": iso(v.map_or_else(Utc::now, |x| x.checked_at)),
        "reason": reason,
        "platform_owned": owned,
    })
}

/// The edge's public host (`WEBCHAT_PUBLIC_BASE` without its scheme).
fn host_of(base: &str) -> String {
    base.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

const BINDINGS: &str = "SELECT b.id, b.job_id, b.kind, b.side, b.env, COALESCE(b.label, '') AS label, b.enabled, \
     b.verified_at, b.external_id, w.status AS webhook_status, w.last_update_at AS webhook_last_update_at, \
     w.last_error AS webhook_last_error, w.last_error_at AS webhook_last_error_at, \
     w.observed_at AS webhook_observed_at, w.pending_update_count AS webhook_pending_update_count, \
     w.webhook_down_reason \
     FROM connector_bindings b LEFT JOIN channel_webhook_status w ON w.binding_id = b.id \
     WHERE b.tenant_id = $1 ORDER BY b.created_at, b.id";

async fn tenant_channels(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let tenant_status: String = sqlx::query_scalar("SELECT status FROM tenants WHERE id = $1")
        .bind(t.tenant)
        .fetch_one(&mut *tx)
        .await?;
    let owned = crate::providers::llm::platform_owned(&mut tx, &st, t.tenant).await?;
    let v = verdict::read(&mut tx, t.tenant).await?;
    let jobs: Vec<Job> = sqlx::query(
        "SELECT j.id, j.key, j.label, r.release_id, r.hash FROM jobs j \
           LEFT JOIN job_release r ON r.tenant_id = j.tenant_id AND r.job_id = j.id \
                                  AND r.release_id = j.current_release_id \
          WHERE j.tenant_id = $1 ORDER BY j.key",
    )
    .bind(t.tenant)
    .fetch_all(&mut *tx)
    .await?
    .iter()
    .map(|r| Job {
        id: r.get("id"),
        key: r.get("key"),
        label: r.get("label"),
        release_id: r.get("release_id"),
        release_hash: r.get("hash"),
    })
    .collect();
    let counts: HashMap<(Uuid, String, String), i64> = sqlx::query(
        "SELECT job_id, channel, env, count(*) AS n FROM conversation \
          WHERE tenant_id = $1 AND started_at > now() - interval '7 days' GROUP BY job_id, channel, env",
    )
    .bind(t.tenant)
    .fetch_all(&mut *tx)
    .await?
    .iter()
    .map(|r| ((r.get("job_id"), r.get("channel"), r.get("env")), r.get("n")))
    .collect();
    let rows = sqlx::query(BINDINGS)
        .bind(t.tenant)
        .fetch_all(&mut *tx)
        .await?;
    // Bindings per (job, kind, env): the conversation count is per that key, shown once.
    let mut per_key: HashMap<(Uuid, String, String), usize> = HashMap::new();
    for r in rows
        .iter()
        .filter(|r| r.get::<String, _>("side") == "customer")
    {
        *per_key
            .entry((r.get("job_id"), r.get("kind"), r.get("env")))
            .or_default() += 1;
    }
    let refusals_24h: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_event \
          WHERE tenant_id = $1 AND action = 'channel_turn_refused' AND at > now() - interval '24 hours'",
    )
    .bind(t.tenant)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;

    let by_id: HashMap<Uuid, &Job> = jobs.iter().map(|j| (j.id, j)).collect();
    let web_host = host_of(&public_base(&st));
    let tenant = t.tenant.to_string();
    let mut channels = Vec::new();
    let mut bound = Vec::new();
    let mut counted: Vec<(Uuid, String, String)> = Vec::new();
    for r in &rows {
        let job_id: Uuid = r.get("job_id");
        let kind: String = r.get("kind");
        let side: String = r.get("side");
        let worker = side == "worker";
        let env: String = r.get("env");
        let label: String = r.get("label");
        let enabled: bool = r.get("enabled");
        let verified = r.get::<Option<DateTime<Utc>>, _>("verified_at").is_some();
        let live = enabled && verified;
        let job = by_id.get(&job_id).copied();
        let webhook = webhook_view(
            r.get("webhook_status"),
            r.get("webhook_last_update_at"),
            r.get("webhook_pending_update_count"),
            r.get("webhook_down_reason"),
            r.get("webhook_last_error"),
            r.get("webhook_last_error_at"),
            r.get("webhook_observed_at"),
        );
        let job_label = job.map_or("its job", |j| j.label.as_str());
        let mut note = if worker {
            if live {
                format!(
                    "Staff-side binding of {job_label} ({env}): handovers are routed to this external tool."
                )
            } else if !enabled {
                format!("Staff-side binding of {job_label}, disabled on its Channels page.")
            } else {
                format!("Staff-side binding of {job_label}, waiting for ownership proof.")
            }
        } else if live {
            format!(
                "Bound to {job_label} ({env}); every customer turn passes the licence gate first."
            )
        } else if !enabled {
            format!("Bound to {job_label} but disabled on its Channels page: carries no traffic.")
        } else {
            format!(
                "Bound to {job_label}, waiting for ownership proof (the verify code sent from the account)."
            )
        };
        let (start, ceiling, how) = identity(&kind);
        let shown = if label.trim().is_empty() {
            kind_label(&kind).to_string()
        } else {
            label
        };
        let key = (job_id, kind.clone(), env.clone());
        let shared = per_key.get(&key).copied().unwrap_or(1);
        let conversations = if worker {
            Value::Null
        } else if counted.contains(&key) {
            note.push_str(&format!(
                " Conversations are counted per job, kind and env: this {env} {kind} binding's are in the first one's count."
            ));
            Value::Null
        } else {
            if shared > 1 {
                note.push_str(&format!(
                    " Conversations are counted per job, kind and env: this count covers all {shared} {env} {kind} bindings of {job_label}."
                ));
            }
            json!(counts.get(&key).copied().unwrap_or(0))
        };
        if !worker {
            counted.push(key);
        }
        let edge_host = (kind == "web_chat").then(|| web_host.clone());
        // The public widget key, only for a live web chat door (an unverified or disabled one carries no traffic).
        let widget_key = r
            .get::<Option<String>, _>("external_id")
            .filter(|k| kind == "web_chat" && live && !k.trim().is_empty());
        let answered_by = match job {
            Some(j) if live => answered(j, &env),
            _ => Value::Null,
        };
        let bound_job = job.map(|j| j.key.clone());
        let id: Uuid = r.get("id");
        if !worker {
            bound.push(kind.clone());
        }
        channels.push(json!({
            "id": id,
            "kind": kind,
            "label": shown,
            "binding": "job_binding",
            "milestone": "M1",
            "state": if live { "live" } else { "not_ready" },
            "bot_username": Value::Null,
            "bound_tenant": &tenant,
            "bound_job": bound_job,
            "side": side,
            "job_key": bound_job,
            "env": env,
            "webhook": webhook,
            "conversations_7d": conversations,
            "note": note,
            "identity_start": start,
            "identity_ceiling": ceiling,
            "identity_how": how,
            "edge_host": edge_host,
            "widget_key": widget_key,
            "answered_by": answered_by,
        }));
    }

    let staff_n: i64 = counts
        .iter()
        .filter(|(k, _)| k.1 == "staff" && k.2 == "test")
        .map(|(_, n)| *n)
        .sum();
    let (start, ceiling, how) = identity("staff_chat");
    channels.push(json!({
        "id": "ch-staff",
        "kind": "staff_chat",
        "label": "Staff test chat",
        "binding": "staff_console",
        "milestone": "M1",
        "state": "live",
        "bot_username": Value::Null,
        "bound_tenant": Value::Null,
        "bound_job": Value::Null,
        "env": "test",
        "webhook": Value::Null,
        "conversations_7d": staff_n,
        "note": "Staff chat as a customer from /chat. Defaults to the test env.",
        "identity_start": start,
        "identity_ceiling": ceiling,
        "identity_how": how,
        "edge_host": Value::Null,
        "widget_key": Value::Null,
        "answered_by": jobs.first().map_or(Value::Null, |j| answered(j, "test")),
        "also_answers": jobs.iter().skip(1).map(|j| answered(j, "test")).collect::<Vec<_>>(),
    }));

    for kind in CUSTOMER_KINDS {
        if bound.iter().any(|b| b == kind) {
            continue;
        }
        let (start, ceiling, how) = identity(kind);
        channels.push(json!({
            "id": format!("ch-{kind}"),
            "kind": kind,
            "label": kind_label(kind),
            "binding": "job_binding",
            "milestone": "M1",
            "state": "available",
            "bot_username": Value::Null,
            "bound_tenant": Value::Null,
            "bound_job": Value::Null,
            "env": Value::Null,
            "webhook": Value::Null,
            "conversations_7d": Value::Null,
            "note": "Not set up: add a binding on a job's Channels page.",
            "identity_start": start,
            "identity_ceiling": ceiling,
            "identity_how": how,
            "edge_host": Value::Null,
            "widget_key": Value::Null,
            "answered_by": Value::Null,
        }));
    }
    let (start, ceiling, how) = identity("voice");
    channels.push(json!({
        "id": "ch-voice",
        "kind": "voice",
        "label": "Voice",
        "binding": "reserved",
        "milestone": "M4",
        "state": "later",
        "bot_username": Value::Null,
        "bound_tenant": Value::Null,
        "bound_job": Value::Null,
        "env": Value::Null,
        "webhook": Value::Null,
        "conversations_7d": Value::Null,
        "note": "No inbound phone door yet: spoken turns run through the voice connector of the interaction API.",
        "identity_start": start,
        "identity_ceiling": ceiling,
        "identity_how": how,
        "edge_host": Value::Null,
        "widget_key": Value::Null,
        "answered_by": Value::Null,
    }));

    Ok(Json(json!({
        "channels": channels,
        "tenant_status": tenant_status,
        "gate": gate_json(v.as_ref(), owned, plan(&st)),
        "refusal_text": crate::turn::REFUSAL_TEXT,
        "refusals_24h": refusals_24h,
        "operations_job": Value::Null,
        "job_operations": Vec::<Value>::new(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(status: &str, reason: Option<&str>, age: i64) -> Verdict {
        Verdict {
            tenant_id: Uuid::nil(),
            status: status.to_string(),
            reason: reason.map(str::to_string),
            licence_id: Some("lic".to_string()),
            plan: None,
            kid: None,
            platform_owned: false,
            checked_at: Utc::now(),
            age_seconds: age,
        }
    }

    #[test]
    fn the_gate_reads_the_verdict_like_gate_two() {
        let g = gate_json(Some(&v("active", None, 5)), false, "p");
        assert_eq!(
            (g["status"].clone(), g["reason"].clone()),
            (json!("active"), Value::Null)
        );
        assert_eq!(g["plan"], "p");
        let g = gate_json(Some(&v("active", None, MAX_AGE_SECONDS + 1)), false, "p");
        assert_eq!(
            (g["status"].clone(), g["reason"].clone()),
            (json!("inactive"), json!("verdict_stale"))
        );
        let g = gate_json(None, false, "p");
        assert_eq!(
            (g["status"].clone(), g["reason"].clone()),
            (json!("none"), json!("verdict_missing"))
        );
        let g = gate_json(Some(&v("revoked", Some("offboarded"), 5)), false, "p");
        assert_eq!(
            (g["status"].clone(), g["reason"].clone()),
            (json!("revoked"), json!("offboarded"))
        );
        let g = gate_json(Some(&v("weird", None, 5)), false, "p");
        assert_eq!(g["status"], "inactive");
        let g = gate_json(None, true, "p");
        assert_eq!(
            (g["status"].clone(), g["platform_owned"].clone()),
            (json!("active"), json!(true))
        );
    }

    #[test]
    fn the_web_chat_host_drops_the_scheme() {
        assert_eq!(host_of("https://chat.nonagentic.ai"), "chat.nonagentic.ai");
        assert_eq!(host_of("http://localhost:8099/x"), "localhost:8099");
    }
}
