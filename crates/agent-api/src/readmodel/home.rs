//! The tenant overview and home read models (B1 T15, X7): the Integrations panel and the data-plane blocks
//! (spec 06 §4.5–4.6), the attention list and the agents' activity of `web/src/lib/api/types/tenant.ts`.
//!
//! [`attention`] and [`activity`] are the X7 extension points: B2 adds `release_waiting` / `readiness` items, B3
//! `drift`, `ingest_failed`, `llm_provider`, B4 `agent_alert` and the usage / case numbers. B1 lists `licence`,
//! `provision`, `template_newer` and `invite` items, and counts the `ai_agent` audit rows. Every item carries its
//! `owner_role` (a role key, or `automatic` when the platform retries it) and `due_at` (none of the tenant items has one).
use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::audit::read::{AuditRow, ROW_SELECT, event_json};
use crate::error::ApiResult;
use crate::provision::{OutboxRow, ROW_COLUMNS};
use crate::readmodel::jobs;
use crate::readmodel::tenant::TenantRow;
use crate::readmodel::wording;
use crate::state::AppState;

/// (target, panel label), in panel order.
pub const TARGETS: [(&str, &str); 3] = [
    ("licences", "Licences"),
    ("strata_rag", "Knowledge"),
    ("strata_ingest", "Ingest"),
];

fn is_open(status: &str) -> bool {
    matches!(status, "pending" | "in_flight" | "failed" | "dead")
}

/// The Integrations chip of the row that decides it.
fn chip(row: Option<&OutboxRow>) -> &'static str {
    match row.map(|r| r.status.as_str()) {
        Some("pending" | "in_flight") => "pending",
        Some("failed") => "failed",
        Some("dead") => "dead",
        Some("cancelled") => "cancelled",
        _ => "ready",
    }
}

/// `provision_state.drift` as `ProvisionDrift[]` (an array as B3 writes it; anything else is no drift).
fn drift_list(drift: Option<&Value>) -> Value {
    match drift {
        Some(Value::Array(a)) => Value::Array(a.clone()),
        _ => json!([]),
    }
}

/// `IntegrationRow[]` and `ProvisionBlock[]` of the scoped tenant. Per target: `current` = the OLDEST open row
/// (FIFO per target; open = pending, in flight, failed, dead), else the newest; `history` = the newest 20. A target
/// blocks data-plane calls until it last succeeded (`provision_state.last_success_at`): its latest live row pending or
/// in flight → 409 `provision_pending`, failed or dead → 503 `provision_failed` (crate::provision::require_provisioned).
pub async fn integrations(conn: &mut PgConnection) -> ApiResult<(Vec<Value>, Vec<Value>)> {
    let rows: Vec<OutboxRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLUMNS} FROM provision_outbox ORDER BY created_at DESC, id DESC LIMIT 500"
    )))
    .fetch_all(&mut *conn)
    .await?;
    type State = (String, Option<DateTime<Utc>>, Option<Value>);
    let state: Vec<State> =
        sqlx::query_as("SELECT target, last_success_at, drift FROM provision_state")
            .fetch_all(&mut *conn)
            .await?;
    let mut panel = Vec::new();
    let mut pending_targets = Vec::new();
    let mut failed_targets = Vec::new();
    for (target, label) in TARGETS {
        let mine: Vec<&OutboxRow> = rows.iter().filter(|r| r.target == target).collect();
        let current = mine
            .iter()
            .rev()
            .find(|r| is_open(&r.status))
            .or_else(|| mine.first())
            .copied();
        let s = state.iter().find(|(t, _, _)| t == target);
        let last_success = s.and_then(|(_, at, _)| *at);
        if last_success.is_none() {
            let latest = mine
                .iter()
                .find(|r| r.status != "superseded" && r.status != "cancelled");
            match latest.map(|r| r.status.as_str()) {
                Some("pending" | "in_flight") => pending_targets.push(target),
                Some("failed" | "dead") => failed_targets.push(target),
                _ => {}
            }
        }
        panel.push(json!({
            "target": target,
            "label": label,
            "chip": chip(current),
            "current": current.map(|r| r.tenant_json()),
            "history": mine.iter().take(20).map(|r| r.tenant_json()).collect::<Vec<_>>(),
            "last_success_at": last_success,
            "drift": drift_list(s.and_then(|(_, _, d)| d.as_ref())),
        }));
    }
    let mut blocks = Vec::new();
    if !pending_targets.is_empty() {
        blocks
            .push(json!({"code": "provision_pending", "status": 409, "targets": pending_targets}));
    }
    if !failed_targets.is_empty() {
        blocks.push(json!({"code": "provision_failed", "status": 503, "targets": failed_targets}));
    }
    Ok((panel, blocks))
}

/// `TenantOverview.members`: every member row, enabled admins, disabled members, open invites.
pub async fn member_counts(conn: &mut PgConnection) -> ApiResult<Value> {
    let (total, admins, disabled, invited): (i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*), \
                count(*) FILTER (WHERE 'tenant_admin' = ANY (roles) AND disabled_at IS NULL), \
                count(*) FILTER (WHERE disabled_at IS NOT NULL), \
                count(*) FILTER (WHERE disabled_at IS NULL AND invited_at IS NOT NULL) \
           FROM users",
    )
    .fetch_one(conn)
    .await?;
    Ok(json!({"total": total, "admins": admins, "disabled": disabled, "invited": invited}))
}

#[allow(clippy::too_many_arguments)]
fn item(
    id: String,
    kind: &str,
    severity: &str,
    title: &str,
    detail: String,
    href: &str,
    action: &str,
    job: Option<&str>,
    at: Option<DateTime<Utc>>,
    owner_role: &str,
    due_at: Option<DateTime<Utc>>,
) -> Value {
    json!({"code": id.clone(), "id": id, "kind": kind, "severity": severity, "title": title, "detail": detail,
           "href": href, "action": action, "job": job, "at": at,
           "owner_role": owner_role, "due_at": due_at})
}

/// X7 `home::attention` for the scoped tenant: what needs the tenant admin (`AttentionItem[]`), most urgent first.
/// `invite_lifespan_s` is `INVITE_EMAIL_LIFESPAN_SECONDS` (an invite older than it has expired).
pub async fn attention(
    conn: &mut PgConnection,
    t: &TenantRow,
    invite_lifespan_s: i64,
) -> ApiResult<Vec<Value>> {
    let mut out = Vec::new();
    if t.licence_id.is_none() && t.status == "active" {
        out.push(item(
            "licence:tenant".into(),
            "licence",
            "action",
            "Tenant licence not issued yet",
            "The licence service has not issued this tenant's licence; it is retried automatically.".into(),
            "/admin",
            "Check status",
            None,
            None,
            "automatic",
            None,
        ));
    }
    // B7 T3b (R1): a re-mint whose old (leaked) licence licences did not confirm revoked. The old id is parked in
    // `licence_revoke_pending` with the re-mint's `license.revoke_failed` row; the item clears once the tick drains it.
    let remint_parked: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT p.id, max(e.at) FROM tenants t \
           CROSS JOIN LATERAL unnest(t.licence_revoke_pending) AS p (id) \
           JOIN audit_event e ON e.tenant_id = t.id AND e.action = 'license.revoke_failed' \
                             AND e.details ->> 'via' = 'reissue' AND e.details ->> 'kind' = 'remint' \
                             AND e.details ->> 'license_id' = p.id \
          WHERE t.id = $1 GROUP BY p.id ORDER BY p.id",
    )
    .bind(t.id)
    .fetch_all(&mut *conn)
    .await?;
    for (id, at) in remint_parked {
        out.push(item(
            format!("licence:remint:{id}"),
            "licence",
            "blocking",
            "Old licence token still valid",
            format!(
                "The licence {id} was re-minted because its token leaked, but the licence service has not confirmed \
                 its revoke yet; it is retried automatically."
            ),
            "/admin",
            "Check status",
            None,
            Some(at),
            "automatic",
            None,
        ));
    }
    let failed: Vec<OutboxRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ROW_COLUMNS} FROM provision_outbox WHERE status IN ('failed', 'dead') \
          ORDER BY updated_at DESC, id DESC LIMIT 20"
    )))
    .fetch_all(&mut *conn)
    .await?;
    for r in &failed {
        let reason = r.last_error_code.as_deref().unwrap_or(r.status.as_str());
        let mut it = item(
            format!("provision:{}", r.id),
            "provision",
            if r.status == "dead" {
                "blocking"
            } else {
                "action"
            },
            "A set-up step failed",
            format!(
                "{} for {} stopped ({}).",
                wording::human_key(&r.op),
                wording::human_key(&r.target).to_lowercase(),
                wording::human_key(reason).to_lowercase()
            ),
            "/admin",
            "Retry",
            None,
            Some(r.updated_at),
            "tenant_admin",
            None,
        );
        it["code"] = json!(format!("{} for {} stopped ({reason}).", r.op, r.target));
        out.push(it);
    }
    let lifespan = Duration::seconds(invite_lifespan_s.clamp(0, 10 * 365 * 86_400));
    let invited: Vec<(String, Option<String>, DateTime<Utc>, bool)> = sqlx::query_as(
        "SELECT u.sub, u.display_name, u.invited_at, \
                EXISTS (SELECT 1 FROM audit_event e \
                         WHERE e.action = 'user.invite_email' AND e.status = 'failure' \
                           AND e.resource_id = u.sub AND e.at >= u.invited_at) \
           FROM users u WHERE u.invited_at IS NOT NULL AND u.disabled_at IS NULL ORDER BY u.invited_at",
    )
    .fetch_all(&mut *conn)
    .await?;
    let now = Utc::now();
    for (sub, name, at, email_failed) in invited {
        let expires = at + lifespan;
        let who = name.unwrap_or_else(|| sub.clone());
        if email_failed {
            out.push(item(
                format!("invite:{sub}"),
                "invite",
                "action",
                "Invite email not sent",
                format!("The invite email to {who} could not be sent; invite them again."),
                "/admin/users",
                "Invite again",
                None,
                Some(at),
                "tenant_admin",
                None,
            ));
        } else if expires < now {
            out.push(item(
                format!("invite:{sub}"),
                "invite",
                "action",
                "Invite expired",
                format!("{who} did not accept the invite before it expired."),
                "/admin/users",
                "Invite again",
                None,
                Some(expires),
                "tenant_admin",
                None,
            ));
        }
    }
    for j in jobs::list(&mut *conn).await? {
        let newer = jobs::newer(&mut *conn, &j).await?;
        if newer.is_empty() {
            continue;
        }
        let requested: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM template_upgrade_request WHERE job_id = $1 AND status = 'open')",
        )
        .bind(j.id)
        .fetch_one(&mut *conn)
        .await?;
        if requested {
            continue;
        }
        let tags: Vec<String> = newer
            .iter()
            .map(|p| format!("{}@v{}", p["layer"].as_str().unwrap_or(""), p["version"]))
            .collect();
        out.push(item(
            format!("template_newer:{}", j.key),
            "template_newer",
            "info",
            "A newer template is tagged",
            format!("{} can move to {}.", j.label, tags.join(", ")),
            "/admin/jobs",
            "Request upgrade",
            Some(j.key.as_str()),
            None,
            "tenant_admin",
            None,
        ));
    }
    let rank = |v: &Value| match v["severity"].as_str() {
        Some("blocking") => 0,
        Some("action") => 1,
        _ => 2,
    };
    out.sort_by_key(rank);
    Ok(out)
}

/// The audit statuses, in display order.
pub const STATUSES: [&str; 5] = ["success", "failure", "denied", "error", "fallback"];

/// X7 `home::activity` for the scoped tenant over the last `days`: usage and cases are B4's (zeros, not attributed);
/// `outcomes` and `notable` come from the `ai_agent` rows of the audit log.
pub async fn activity(conn: &mut PgConnection, days: i32) -> ApiResult<Value> {
    let counts: Vec<(String, i64)> = sqlx::query_as(
        "SELECT status, count(*) FROM audit_event \
          WHERE actor_type = 'ai_agent' AND at > now() - make_interval(days => $1) GROUP BY status",
    )
    .bind(days)
    .fetch_all(&mut *conn)
    .await?;
    let mut outcomes: BTreeMap<&str, i64> = STATUSES.iter().map(|s| (*s, 0)).collect();
    for (status, n) in &counts {
        if let Some(c) = outcomes.get_mut(status.as_str()) {
            *c = *n;
        }
    }
    let notable: Vec<AuditRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{ROW_SELECT} WHERE e.actor_type = 'ai_agent' AND e.status <> 'success' \
            AND e.at > now() - make_interval(days => $1) ORDER BY e.id DESC LIMIT 5"
    )))
    .bind(days)
    .fetch_all(&mut *conn)
    .await?;
    let today = Utc::now().date_naive();
    let from = today - Duration::days(i64::from(days.max(1)) - 1);
    let daily: Vec<Value> = (0..days.max(1))
        .map(|i| json!({"day": (from + Duration::days(i64::from(i))).to_string(), "turns": 0}))
        .collect();
    Ok(json!({
        "from": from.to_string(),
        "to": today.to_string(),
        "turns": 0,
        "tokens": 0,
        "daily": daily,
        "cases": 0,
        "containment": null,
        "outcomes": outcomes,
        "notable": notable.iter().map(|r| event_json(r, None)).collect::<Vec<_>>(),
        "attributed": false,
    }))
}

/// `JobPulse[]` of the scoped tenant: each job's release lane (`current`, `waiting`), the template tags newer than its
/// pins, its readiness and the last 7 days of cases, all from [`jobs::board_extras`] / [`jobs::release_lane`].
/// `on_bus` is the bus fact the caller probed before opening its transaction.
pub async fn pulses(conn: &mut PgConnection, st: &AppState, on_bus: bool) -> ApiResult<Vec<Value>> {
    let mut out = Vec::new();
    for j in jobs::list(&mut *conn).await? {
        let extras = jobs::board_extras(&mut *conn, st, &j).await?;
        let (current, waiting) = jobs::release_lane(&mut *conn, j.id).await?;
        out.push(json!({
            "job": jobs::summary(&mut *conn, &j, on_bus).await?,
            "current": current,
            "waiting": waiting,
            "newer": jobs::newer(&mut *conn, &j).await?,
            "readiness": extras["readiness"],
            "cases_7d": extras["cases_7d"],
            "containment_7d": extras["containment_7d"],
        }));
    }
    Ok(out)
}
