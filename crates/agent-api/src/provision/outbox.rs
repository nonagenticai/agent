//! The fenced drainer (B1 X5; spec 06 §4.5, P4 T4a): claim due rows of the registered ops (`provision_claim`, one
//! short statement), send each OUTSIDE any transaction, classify the answer, and finish it with the lease token it
//! was claimed with (`provision_finish` inside `begin_scoped(row.tenant_id)`), together with the sender's
//! `on_success` writes and the audit row (`provision.succeeded|failed|dead`, actor `service:agent-api-drainer`). A
//! finish whose lease was lost changes nothing and is discarded. A retry whose dedupe key already has a newer pending
//! row is finished `superseded` (the newer row carries its desired state). Tick 5 s, batch 10, lease 120 s, per
//! replica. Every [`SWEEP_EVERY`] ticks (and on the first) the drainer also runs the model-change sweeper
//! (crate::knowledge::model_changes::sweep; no CronJob, no GPU call).
use std::collections::HashMap;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::audit::{self, Actor, Event, scrub};
use crate::error::ApiResult;
use crate::platform_db::begin_platform;
use crate::state::AppState;
use crate::tenancy::begin_scoped;

pub const TICK: Duration = Duration::from_secs(5);
pub const BATCH: i32 = 10;
pub const LEASE_SECS: i32 = 120;
/// The longest backoff (seconds).
pub const BACKOFF_CAP_SECS: u64 = 900;
/// Wait / 429-without-Retry-After delay.
pub const WAIT_SECS: u64 = 30;
/// `last_error` is cut to this many characters.
pub const MAX_ERROR: usize = 2000;
pub const DRAINER: &str = "service:agent-api-drainer";

/// The columns of a row as read (the enum status as text).
macro_rules! columns {
    () => {
        "id, tenant_id, target, op, dedupe_key, payload, status::text AS status, attempts, max_attempts, \
         next_attempt_at, lease_until, lease_token, last_http_status, last_error_code, last_error, result, \
         created_by, created_at, updated_at, completed_at"
    };
}

const CLAIM: &str = concat!("SELECT ", columns!(), " FROM provision_claim($1, $2, $3)");

/// One `provision_outbox` row.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct OutboxRow {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub target: String,
    pub op: String,
    pub dedupe_key: String,
    pub payload: Value,
    pub status: String,
    pub attempts: i32,
    pub max_attempts: i32,
    pub next_attempt_at: DateTime<Utc>,
    pub lease_until: Option<DateTime<Utc>>,
    pub lease_token: Option<Uuid>,
    pub last_http_status: Option<i32>,
    pub last_error_code: Option<String>,
    pub last_error: Option<String>,
    pub result: Option<Value>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// The columns of a row as read by the routes (`SELECT {ROW_COLUMNS} FROM provision_outbox …`).
pub const ROW_COLUMNS: &str = columns!();

impl OutboxRow {
    /// When the drainer tries again: pending rows only.
    fn next_attempt(&self) -> Option<DateTime<Utc>> {
        (self.status == "pending").then_some(self.next_attempt_at)
    }

    /// The tenant `ProvisionRow` (web `types/tenant.ts`): the outbox columns; never the payload.
    pub fn tenant_json(&self) -> Value {
        serde_json::json!({
            "id": self.id,
            "tenant_id": self.tenant_id,
            "target": self.target,
            "op": self.op,
            "dedupe_key": self.dedupe_key,
            "status": self.status,
            "attempts": self.attempts,
            "max_attempts": self.max_attempts,
            "next_attempt_at": self.next_attempt(),
            "last_http_status": self.last_http_status,
            "last_error_code": self.last_error_code,
            "last_error": self.last_error,
            "created_by": self.created_by,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "completed_at": self.completed_at,
        })
    }

    /// The platform `ProvisionRow` (web `types/platform.ts`): the row joined with its target's `provision_state`
    /// (`last_success_at`, `drift` as a one-line summary).
    pub fn platform_json(
        &self,
        last_success_at: Option<DateTime<Utc>>,
        drift: Option<&Value>,
    ) -> Value {
        let last_error = match (&self.last_error_code, &self.last_error) {
            (None, None) => Value::Null,
            (code, msg) => serde_json::json!({
                "code": code.clone().unwrap_or_else(|| "error".into()),
                "message": msg.clone().unwrap_or_default(),
            }),
        };
        serde_json::json!({
            "id": self.id,
            "tenant_id": self.tenant_id,
            "target": self.target,
            "op": self.op,
            "status": self.status,
            "attempts": self.attempts,
            "next_attempt_at": self.next_attempt(),
            "last_error": last_error,
            "last_success_at": last_success_at,
            "drift": drift_summary(drift),
            "updated_at": self.updated_at,
        })
    }
}

/// `provision_state.drift` as one line (`null` when nothing drifted).
pub fn drift_summary(drift: Option<&Value>) -> Option<String> {
    let n = match drift? {
        Value::Array(a) => a.len(),
        Value::Object(o) => o.len(),
        Value::Null => 0,
        _ => 1,
    };
    (n > 0).then(|| format!("{n} remote slot version(s) moved outside this app"))
}

/// What a sender's attempt produced.
#[derive(Clone, Debug)]
pub enum SendResult {
    /// The downstream answered: status, its error code (`error.code` / `detail.error`), `Retry-After` seconds, body.
    Http {
        status: u16,
        code: Option<String>,
        retry_after: Option<u64>,
        body: Value,
    },
    /// No answer (connect, timeout, …).
    Transport(String),
    /// Not yet sendable (e.g. `licence_pending`): retried in 30 s, not counted.
    Wait(&'static str),
    /// Done locally without a call (e.g. nothing to revoke): succeeded with this result.
    Done(Value),
    /// Will fail identically again: failed fast with this code.
    Fail(&'static str),
}

/// What happens to the row.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    Succeeded(Value),
    /// `pending` again after `after`; `counted` = the attempt counts toward `max_attempts`.
    Retry {
        after: Duration,
        counted: bool,
        code: String,
    },
    FailFast(String),
    Dead(String),
}

/// Full jitter in `[0, min(900, 5·2ⁿ)]` seconds for attempt `n`, from `entropy`.
pub fn backoff(attempts: i32, entropy: u64) -> Duration {
    let n = u32::try_from(attempts.clamp(0, 20)).unwrap_or(0);
    let cap_ms = 5u64
        .saturating_mul(1u64 << n)
        .min(BACKOFF_CAP_SECS)
        .saturating_mul(1000);
    Duration::from_millis(entropy % (cap_ms + 1))
}

/// The 06 §4.5 response table.
pub fn classify(r: &SendResult, attempts: i32, max_attempts: i32, entropy: u64) -> Verdict {
    let counted_retry = |code: &str| {
        if attempts >= max_attempts {
            Verdict::Dead(code.to_string())
        } else {
            Verdict::Retry {
                after: backoff(attempts, entropy),
                counted: true,
                code: code.to_string(),
            }
        }
    };
    match r {
        SendResult::Done(v) => Verdict::Succeeded(v.clone()),
        SendResult::Fail(code) => Verdict::FailFast((*code).to_string()),
        SendResult::Wait(code) => Verdict::Retry {
            after: Duration::from_secs(WAIT_SECS),
            counted: false,
            code: (*code).to_string(),
        },
        SendResult::Transport(_) => counted_retry("transport"),
        SendResult::Http {
            status,
            code,
            retry_after,
            body,
        } => {
            let code = code.as_deref();
            match (*status, code) {
                (200..=299, _) => Verdict::Succeeded(body.clone()),
                (404, Some("CONNECTION_NOT_FOUND")) => Verdict::Succeeded(body.clone()),
                (409, Some("VERSION_CONFLICT")) => Verdict::FailFast("version_conflict".into()),
                (409, Some("ensure_in_progress")) => counted_retry("ensure_in_progress"),
                (429, _) => Verdict::Retry {
                    after: Duration::from_secs(retry_after.unwrap_or(WAIT_SECS)),
                    counted: false,
                    code: "rate_limited".into(),
                },
                (408, _) | (500..=599, _) => counted_retry("upstream_5xx"),
                (s, c) => Verdict::FailFast(c.map_or_else(|| format!("http_{s}"), str::to_string)),
            }
        }
    }
}

/// What one [`drain_once`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DrainReport {
    pub claimed: usize,
    pub succeeded: usize,
    pub retried: usize,
    pub failed: usize,
    pub dead: usize,
    /// Finishes refused because the lease was lost (another drainer re-claimed the row).
    pub discarded: usize,
    /// Rows whose finish hit a database error (they are re-claimed after the lease).
    pub errors: usize,
}

fn entropy() -> u64 {
    Uuid::new_v4().as_u64_pair().0
}

/// The text kept in `last_error`: scrubbed, at most 2000 characters.
fn error_text(r: &SendResult) -> Option<String> {
    let raw = match r {
        SendResult::Http { status, body, .. } if !(200..=299).contains(status) => {
            let msg = body
                .pointer("/error/message")
                .or_else(|| body.get("message"))
                .or_else(|| body.get("detail"))
                .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
                .unwrap_or_default();
            format!("HTTP {status} {msg}")
        }
        SendResult::Transport(m) => m.clone(),
        SendResult::Wait(c) => format!("waiting: {c}"),
        SendResult::Fail(c) => c.to_string(),
        _ => return None,
    };
    Some(scrub::scrub_text(&raw, MAX_ERROR))
}

fn http_status(r: &SendResult) -> Option<i32> {
    match r {
        SendResult::Http { status, .. } => Some(i32::from(*status)),
        _ => None,
    }
}

/// Finish one sent row: `provision_finish`, then (on success) the sender's writes, and the audit row, in one
/// transaction scoped to the row's tenant. `Ok(false)` = the lease was lost.
async fn finish(
    st: &AppState,
    row: &OutboxRow,
    sender: &dyn super::Sender,
    r: &SendResult,
    v: &Verdict,
) -> ApiResult<bool> {
    let Some(token) = row.lease_token else {
        return Ok(false);
    };
    let (status, next, counted, code, result): (
        &str,
        Option<DateTime<Utc>>,
        bool,
        Option<&str>,
        Option<&Value>,
    ) = match v {
        Verdict::Succeeded(res) => ("succeeded", None, true, None, Some(res)),
        Verdict::Retry {
            after,
            counted,
            code,
        } => {
            let at = Utc::now()
                + chrono::Duration::from_std(*after).unwrap_or(chrono::Duration::seconds(30));
            ("pending", Some(at), *counted, Some(code.as_str()), None)
        }
        Verdict::FailFast(code) => ("failed", None, true, Some(code.as_str()), None),
        Verdict::Dead(code) => ("dead", None, true, Some(code.as_str()), None),
    };
    let mut tx = begin_scoped(&st.db, row.tenant_id).await?;
    // a retry goes back to `pending`, but a newer row of the same dedupe key may already be pending (queued while this
    // one was in flight; it carries this row's desired state): this one is superseded instead, so the one-pending
    // index (`provision_outbox_one_pending`) is never violated and the newer row is not blocked (review B3 I2)
    let (status, next) = if status == "pending" {
        let newer: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM provision_outbox \
                             WHERE tenant_id = $1 AND dedupe_key = $2 AND status = 'pending' AND id <> $3)",
        )
        .bind(row.tenant_id)
        .bind(&row.dedupe_key)
        .bind(row.id)
        .fetch_one(&mut *tx)
        .await?;
        if newer {
            ("superseded", None)
        } else {
            (status, next)
        }
    } else {
        (status, next)
    };
    let ok: bool =
        sqlx::query_scalar("SELECT provision_finish($1, $2, $3, $4, $5, $6, $7, $8, $9)")
            .bind(row.id)
            .bind(token)
            .bind(status)
            .bind(http_status(r))
            .bind(code)
            .bind(error_text(r))
            .bind(result)
            .bind(next)
            .bind(counted)
            .fetch_one(&mut *tx)
            .await?;
    if !ok {
        tx.rollback().await?;
        return Ok(false);
    }
    let actor = Actor::system(DRAINER);
    let event = |action: &'static str| {
        Event::new(action, "provision_outbox", row.id.to_string())
            .detail("op", row.op.clone())
            .detail("target", row.target.clone())
            .detail("attempts", row.attempts)
    };
    match v {
        Verdict::Succeeded(res) => {
            sender.on_success(&mut tx, row, res).await?;
            audit::record(&mut tx, &actor, event("provision.succeeded")).await?;
        }
        Verdict::FailFast(code) => {
            let e = event("provision.failed")
                .status(audit::Status::Failure)
                .detail("code", code.clone());
            audit::record(&mut tx, &actor, e).await?;
        }
        Verdict::Dead(code) => {
            let e = event("provision.dead")
                .status(audit::Status::Failure)
                .detail("code", code.clone());
            audit::record(&mut tx, &actor, e).await?;
        }
        Verdict::Retry { .. } => {}
    }
    tx.commit().await?;
    if let Verdict::FailFast(code) | Verdict::Dead(code) = v {
        metrics::counter!(
            "agent_provision_outbox_failed_total",
            "target" => row.target.clone(),
            "op" => row.op.clone(),
            "code" => code.clone()
        )
        .increment(1);
    }
    Ok(true)
}

/// One pass: claim up to [`BATCH`] due rows of the registered ops and deliver them.
pub async fn drain_once(st: &AppState) -> DrainReport {
    let mut report = DrainReport::default();
    let ops = st.senders.ops();
    if ops.is_empty() {
        return report;
    }
    let rows: Vec<OutboxRow> = match sqlx::query_as(CLAIM)
        .bind(BATCH)
        .bind(LEASE_SECS)
        .bind(&ops)
        .fetch_all(&st.db)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "provision_claim_failed");
            report.errors += 1;
            return report;
        }
    };
    report.claimed = rows.len();
    for row in &rows {
        let Some(sender) = st.senders.get(&row.op) else {
            continue;
        };
        let r = sender.send(st, row).await;
        let v = classify(&r, row.attempts, row.max_attempts, entropy());
        match finish(st, row, sender.as_ref(), &r, &v).await {
            Ok(false) => report.discarded += 1,
            Ok(true) => match v {
                Verdict::Succeeded(_) => report.succeeded += 1,
                Verdict::Retry { .. } => report.retried += 1,
                Verdict::FailFast(_) => report.failed += 1,
                Verdict::Dead(_) => report.dead += 1,
            },
            Err(e) => {
                report.errors += 1;
                tracing::warn!(row = %row.id, error = %e, "provision_finish_failed");
            }
        }
    }
    report
}

/// The drainer runs the model-change sweeper every this many ticks (10 min), and on its first tick (boot).
pub const SWEEP_EVERY: u64 = 120;

/// [`drain_once`] every [`TICK`] (immediately again while a full batch was claimed); never stops. Piggybacks the
/// model-change sweeper (module doc).
pub fn spawn_drainer(st: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut t = tokio::time::interval(TICK);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut ticks: u64 = 0;
        loop {
            t.tick().await;
            if ticks.is_multiple_of(SWEEP_EVERY) {
                match crate::knowledge::model_changes::sweep(&st).await {
                    Ok(r) if r.changed() => tracing::info!(report = ?r, "model_change_sweep"),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "model_change_sweep_failed"),
                }
            }
            ticks = ticks.wrapping_add(1);
            loop {
                let r = drain_once(&st).await;
                if r.claimed > 0 {
                    tracing::info!(report = ?r, "provision_drain");
                }
                if r.claimed < usize::try_from(BATCH).unwrap_or(10) {
                    break;
                }
            }
        }
    })
}

/// The outbox, signup-journal and licence-reconciler gauges (the 30 s metrics tick): one aggregate read as `agent_platform`, no tenant
/// ids in any label.
pub async fn refresh_gauges(db: &PgPool) -> ApiResult<()> {
    let mut tx = begin_platform(db).await?;
    let rows: Vec<(String, String, i64, i64, f64)> = sqlx::query_as(
        "SELECT target, op, pending, dead, oldest_pending_s FROM agent_provision_stats()",
    )
    .fetch_all(&mut *tx)
    .await?;
    let steps: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT step, status, count(*) FROM signup_provisioning_steps GROUP BY step, status",
    )
    .fetch_all(&mut *tx)
    .await?;
    let backoff: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT kind, status, count(*) FROM licence_work_backoff GROUP BY kind, status",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    // The licence reconciler's failing items (review #2): `retrying` (backing off) and `error` (gave up auditing).
    let mut items: HashMap<(String, String), i64> = HashMap::new();
    for kind in ["mint", "revoke", "parked", "tenant"] {
        for status in ["retrying", "error"] {
            items.insert((kind.to_string(), status.to_string()), 0);
        }
    }
    for (kind, status, n) in backoff {
        items.insert((kind, status), n);
    }
    for ((kind, status), n) in items {
        metrics::gauge!("agent_licence_work_failing", "kind" => kind, "status" => status)
            .set(n as f64);
    }
    let mut oldest: HashMap<String, f64> = HashMap::new();
    let mut dead: HashMap<String, i64> = HashMap::new();
    for (target, op, pending, d, age) in rows {
        metrics::gauge!("agent_provision_outbox_pending", "target" => target.clone(), "op" => op)
            .set(pending as f64);
        let o = oldest.entry(target.clone()).or_insert(0.0);
        *o = o.max(age);
        *dead.entry(target).or_insert(0) += d;
    }
    for (target, age) in oldest {
        metrics::gauge!("agent_provision_outbox_oldest_pending_age_seconds", "target" => target)
            .set(age);
    }
    for (target, d) in dead {
        metrics::gauge!("agent_provision_outbox_dead", "target" => target).set(d as f64);
    }
    let mut counts: HashMap<(String, String), i64> = HashMap::new();
    for step in crate::signup::provisioning::STEPS {
        for status in ["pending", "ok", "failed", "skipped"] {
            counts.insert((step.to_string(), status.to_string()), 0);
        }
    }
    for (step, status, n) in steps {
        counts.insert((step, status), n);
    }
    for ((step, status), n) in counts {
        metrics::gauge!("agent_signup_steps", "step" => step, "status" => status).set(n as f64);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn http(status: u16, code: Option<&str>, retry_after: Option<u64>) -> SendResult {
        SendResult::Http {
            status,
            code: code.map(str::to_string),
            retry_after,
            body: json!({}),
        }
    }

    #[test]
    fn backoff_is_bounded_by_cap() {
        for a in 0..=14 {
            let cap = Duration::from_secs(BACKOFF_CAP_SECS.min(5 << a));
            assert!(backoff(a, u64::MAX) <= cap, "attempt {a}");
            assert_eq!(backoff(a, 0), Duration::ZERO);
        }
    }

    #[test]
    fn classify_table() {
        assert!(matches!(
            classify(&http(201, None, None), 1, 12, 7),
            Verdict::Succeeded(_)
        ));
        assert!(matches!(
            classify(&http(404, Some("CONNECTION_NOT_FOUND"), None), 1, 12, 7),
            Verdict::Succeeded(_)
        ));
        assert_eq!(
            classify(&http(409, Some("VERSION_CONFLICT"), None), 1, 12, 7),
            Verdict::FailFast("version_conflict".into())
        );
        assert!(matches!(
            classify(&http(409, Some("ensure_in_progress"), None), 1, 12, 7),
            Verdict::Retry { counted: true, .. }
        ));
        assert_eq!(
            classify(&http(429, None, Some(120)), 1, 12, 7),
            Verdict::Retry {
                after: Duration::from_secs(120),
                counted: false,
                code: "rate_limited".into()
            }
        );
        assert!(matches!(
            classify(&http(503, None, None), 3, 12, 7),
            Verdict::Retry { counted: true, .. }
        ));
        assert_eq!(
            classify(&http(503, None, None), 12, 12, 7),
            Verdict::Dead("upstream_5xx".into())
        );
        assert_eq!(
            classify(&SendResult::Transport("x".into()), 12, 12, 7),
            Verdict::Dead("transport".into())
        );
        assert_eq!(
            classify(
                &http(403, Some("bundle_not_ensurable_by_caller"), None),
                1,
                12,
                7
            ),
            Verdict::FailFast("bundle_not_ensurable_by_caller".into())
        );
        assert_eq!(
            classify(&http(422, None, None), 1, 12, 7),
            Verdict::FailFast("http_422".into())
        );
        assert!(matches!(
            classify(&SendResult::Wait("licence_pending"), 12, 12, 7),
            Verdict::Retry { counted: false, .. }
        ));
        assert_eq!(
            classify(&SendResult::Fail("licence_failed"), 1, 12, 7),
            Verdict::FailFast("licence_failed".into())
        );
        assert!(matches!(
            classify(&SendResult::Done(json!({"noop": true})), 1, 12, 7),
            Verdict::Succeeded(_)
        ));
    }
}
