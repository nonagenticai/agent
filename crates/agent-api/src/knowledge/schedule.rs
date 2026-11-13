//! Scheduled source re-sync (B3 T8b, D82): built, tested and SWITCHED OFF.
//!
//! A source's `schedule` (five-field cron, UTC; a save refuses anything [`Cron::parse`] refuses, 422
//! `invalid_schedule`) names when it re-syncs by itself. A re-sync re-embeds, and the platform rule "no CronJob, timer or background loop may trigger GPU calls"
//! forbids that, so `SOURCE_SCHEDULE_ENABLED` defaults to `false` and boot refuses `true`
//! (`source_schedule_disabled_by_policy`, `crate::config`). [`spawn`] starts nothing while the flag is off; lifting the
//! refusal is the runbook `docs/runbooks/source-schedule-activation.md`.
//!
//! When it runs: an in-process ticker (no k8s CronJob), once a minute. Each tick takes a session advisory try-lock on
//! a dedicated connection that is closed when the tick ends (so a cancelled or panicking tick releases it), so one
//! replica runs a tick and the others skip it. No request path takes that key, so no request waits on it. A tick
//! lists the active tenants (as `agent_platform`), reads each tenant's enabled, scheduled, fetchable sources (file
//! sources have nothing to fetch) and starts every due one through the same code path as `resyncSource`
//! ([`runs::start`]), actor `System` (`service:agent-api-source-schedule`), audit `source.resync` with
//! `{trigger: "schedule"}`. A source with a run still running is skipped (409 `run_in_progress` from `start`).
//!
//! Due: some minute in `(since, now]` (whole UTC minutes, at most [`WINDOW_MINUTES`] back) matches the cron, where
//! `since` is the source's latest run start (any trigger), else its creation. A fire missed by more than the window is
//! dropped, never replayed.
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Datelike, TimeDelta, Timelike, Utc};
use uuid::Uuid;

use crate::audit::Actor;
use crate::auth::{Caller, RoleSet, TenantStatus};
use crate::error::ApiResult;
use crate::knowledge::runs::{self, StartReq};
use crate::knowledge::sources;
use crate::platform_db;
use crate::state::AppState;
use crate::tenancy::{self, JobCtx, TenantCtx};

/// The actor of a scheduled run (`source_run.started_by`, audit `actor_id`).
pub const SYSTEM: &str = "service:agent-api-source-schedule";
/// The advisory-lock key of the production tick (`hashtext`).
const LOCK: &str = "agent:source-schedule";
/// How often a tick runs.
const TICK: Duration = Duration::from_secs(60);
/// How far back a missed fire still counts (one day of minutes).
pub const WINDOW_MINUTES: i64 = 24 * 60;

/// Set once [`spawn`] has started the ticker: what `GET …/sources/{id}` answers as `schedule_active`.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether scheduled re-syncs run in this process (false unless [`spawn`] started the ticker).
pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------------------------------------------
// Cron

/// A parsed five-field cron (`minute hour day-of-month month day-of-week`, UTC). Each field is a bit set of the
/// values it allows; day-of-week 7 is Sunday (0). As in Vixie cron, when both day fields are restricted (neither
/// starts with `*`) a day matches either; otherwise both must match.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cron {
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64,
    dom_any: bool,
    dow_any: bool,
}

/// One field's allowed values in `lo..=hi`, or `None` when it is malformed or out of range.
fn field(s: &str, lo: u32, hi: u32) -> Option<u64> {
    let mut bits = 0u64;
    for item in s.split(',') {
        let (base, step) = match item.split_once('/') {
            Some((b, st)) => (b, Some(st.parse::<u32>().ok().filter(|n| *n > 0)?)),
            None => (item, None),
        };
        let (from, to) = if base == "*" {
            (lo, hi)
        } else if let Some((a, b)) = base.split_once('-') {
            (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?)
        } else {
            let a = base.parse::<u32>().ok()?;
            (a, if step.is_some() { hi } else { a })
        };
        if from < lo || to > hi || from > to {
            return None;
        }
        let mut v = from;
        while v <= to {
            bits |= 1 << v;
            v += step.unwrap_or(1);
        }
    }
    Some(bits)
}

impl Cron {
    /// `None` for anything [`sources::valid_cron`] refuses or a value out of its field's range.
    pub fn parse(s: &str) -> Option<Self> {
        if !sources::valid_cron(s) {
            return None;
        }
        let f: Vec<&str> = s.split_whitespace().collect();
        let mut dow = field(f[4], 0, 7)?;
        if dow & (1 << 7) != 0 {
            dow = (dow & !(1 << 7)) | 1;
        }
        Some(Self {
            minute: field(f[0], 0, 59)?,
            hour: field(f[1], 0, 23)?,
            dom: field(f[2], 1, 31)?,
            month: field(f[3], 1, 12)?,
            dow,
            dom_any: f[2].starts_with('*'),
            dow_any: f[4].starts_with('*'),
        })
    }

    /// The cron fires in the UTC minute of `t`.
    pub fn matches(&self, t: DateTime<Utc>) -> bool {
        let has = |bits: u64, v: u32| bits & (1 << v) != 0;
        let dom = has(self.dom, t.day());
        let dow = has(self.dow, t.weekday().num_days_from_sunday());
        let day = if self.dom_any || self.dow_any {
            dom && dow
        } else {
            dom || dow
        };
        has(self.minute, t.minute())
            && has(self.hour, t.hour())
            && has(self.month, t.month())
            && day
    }
}

/// `t` cut to its whole minute.
fn minute_of(t: DateTime<Utc>) -> DateTime<Utc> {
    t.with_second(0)
        .and_then(|m| m.with_nanosecond(0))
        .unwrap_or(t)
}

/// The cron fires in some whole UTC minute after `since`'s minute, up to and including `now`'s, at most
/// [`WINDOW_MINUTES`] back from `now`.
pub fn due(cron: &Cron, since: DateTime<Utc>, now: DateTime<Utc>) -> bool {
    let last = minute_of(now);
    let first = (minute_of(since) + TimeDelta::minutes(1))
        .max(last - TimeDelta::minutes(WINDOW_MINUTES - 1));
    let mut t = first;
    while t <= last {
        if cron.matches(t) {
            return true;
        }
        t += TimeDelta::minutes(1);
    }
    false
}

// ---------------------------------------------------------------------------------------------------------------
// Ticks

/// What one tick did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tick {
    /// Runs started.
    pub started: usize,
    /// Due sources not started: a run still running, or an unreadable stored schedule.
    pub skipped: usize,
    /// Due sources whose start failed (logged).
    pub failed: usize,
}

/// A scheduled, enabled, fetchable source and when it last started (or was created).
#[derive(sqlx::FromRow)]
struct Candidate {
    id: Uuid,
    job_id: Uuid,
    job_key: String,
    schedule: String,
    since: DateTime<Utc>,
}

/// The scheduler: [`spawn`] builds the production one; [`Scheduler::unguarded`] is the tests' (one tenant, its own
/// lock key, no boot guard).
pub struct Scheduler {
    st: AppState,
    lock_key: String,
    only: Option<Uuid>,
}

impl Scheduler {
    /// Test-only: a scheduler over tenant `tenant` alone, with its own lock key, bypassing the boot guard (D82). Never
    /// spawned.
    #[doc(hidden)]
    pub fn unguarded(st: AppState, tenant: Uuid) -> Self {
        Self {
            st,
            lock_key: format!("{LOCK}:{tenant}"),
            only: Some(tenant),
        }
    }

    /// One tick at `now`: `None` when another replica holds the tick lock.
    pub async fn tick_at(&self, now: DateTime<Utc>) -> ApiResult<Option<Tick>> {
        let mut conn = self.st.db.acquire().await?;
        // the session lock dies with the connection, whatever ends this tick
        conn.close_on_drop();
        let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
            .bind(&self.lock_key)
            .fetch_one(&mut *conn)
            .await?;
        if !got {
            return Ok(None);
        }
        let tick = self.run(now).await;
        sqlx::query("SELECT pg_advisory_unlock(hashtext($1))")
            .bind(&self.lock_key)
            .execute(&mut *conn)
            .await?;
        tick.map(Some)
    }

    async fn tenants(&self) -> ApiResult<Vec<Uuid>> {
        if let Some(t) = self.only {
            return Ok(vec![t]);
        }
        let mut tx = platform_db::begin_platform(&self.st.db).await?;
        let ids: Vec<Uuid> =
            sqlx::query_scalar("SELECT id FROM agent_list_tenants() WHERE status = 'active'")
                .fetch_all(&mut *tx)
                .await?;
        tx.commit().await?;
        Ok(ids)
    }

    async fn candidates(&self, tenant: Uuid) -> ApiResult<Vec<Candidate>> {
        let mut tx = tenancy::begin_scoped(&self.st.db, tenant).await?;
        let rows: Vec<Candidate> = sqlx::query_as(
            "SELECT s.id, s.job_id, j.key AS job_key, s.schedule, \
                    GREATEST(s.created_at, COALESCE((SELECT max(r.started_at) FROM source_run r \
                                                      WHERE r.source_id = s.id), s.created_at)) AS since \
               FROM source s JOIN jobs j ON j.tenant_id = s.tenant_id AND j.id = s.job_id \
              WHERE s.schedule IS NOT NULL AND s.enabled AND s.type <> 'file' \
              ORDER BY s.id",
        )
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(rows)
    }

    async fn run(&self, now: DateTime<Utc>) -> ApiResult<Tick> {
        let mut out = Tick::default();
        for tenant in self.tenants().await? {
            let rows = match self.candidates(tenant).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(%tenant, error = %e, "source_schedule_tenant_unreadable");
                    continue;
                }
            };
            for c in rows {
                let Some(cron) = Cron::parse(&c.schedule) else {
                    tracing::warn!(%tenant, source = %c.id, "source_schedule_unreadable");
                    out.skipped += 1;
                    continue;
                };
                if !due(&cron, c.since, now) {
                    continue;
                }
                let ctx = system_ctx(tenant, c.job_id, c.job_key);
                let req = StartReq {
                    trigger: "schedule",
                    actor: Some(Actor::system(SYSTEM)),
                    ..StartReq::default()
                };
                match runs::start(&self.st, &ctx, c.id, req).await {
                    Ok(_) => out.started += 1,
                    Err(e) if e.code() == "run_in_progress" => out.skipped += 1,
                    Err(e) => {
                        tracing::warn!(%tenant, source = %c.id, code = e.code(), error = %e, "source_schedule_start_failed");
                        out.failed += 1;
                    }
                }
            }
        }
        Ok(out)
    }
}

/// The job context a scheduled run starts in: the system caller, no roles (the route guards are not on this path).
fn system_ctx(tenant: Uuid, job_id: Uuid, job_key: String) -> JobCtx {
    let caller = Caller {
        sub: SYSTEM.to_string(),
        actor: SYSTEM.to_string(),
        home: None,
        super_admin: false,
        session: "source-schedule".to_string(),
        verified_email: None,
    };
    JobCtx {
        t: TenantCtx::new(
            tenant,
            TenantStatus::Active,
            RoleSet::NONE,
            None,
            caller,
            false,
        ),
        job_id,
        job_key,
    }
}

/// Starts the ticker when `SOURCE_SCHEDULE_ENABLED` is on; `None` (nothing spawned) while it is off, which boot
/// enforces (D82).
pub fn spawn(st: AppState) -> Option<tokio::task::JoinHandle<()>> {
    if !st.cfg.source_schedule_enabled {
        return None;
    }
    ACTIVE.store(true, Ordering::Relaxed);
    let s = Scheduler {
        st,
        lock_key: LOCK.to_string(),
        only: None,
    };
    Some(tokio::spawn(async move {
        let mut t = tokio::time::interval(TICK);
        t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            t.tick().await;
            match s.tick_at(Utc::now()).await {
                Ok(Some(r)) if r != Tick::default() => {
                    tracing::info!(report = ?r, "source_schedule_tick")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "source_schedule_tick_failed"),
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn cron_fields_parse_and_refuse_out_of_range() {
        assert!(Cron::parse("*/15 6-18 * * 1,2").is_some());
        assert!(Cron::parse("0 3 * * 7").is_some());
        assert!(Cron::parse("60 * * * *").is_none());
        assert!(Cron::parse("0 24 * * *").is_none());
        assert!(Cron::parse("0 0 0 * *").is_none());
        assert!(Cron::parse("0 0 * 13 *").is_none());
        assert!(Cron::parse("*/0 * * * *").is_none());
        assert!(Cron::parse("5-1 * * * *").is_none());
        assert!(Cron::parse("@daily").is_none());
    }

    #[test]
    fn day_fields_or_when_both_restricted() {
        // the 1st of the month OR a Monday; 2026-10-19 is a Monday, 2026-10-20 a Tuesday
        let c = Cron::parse("0 3 1 * 1").unwrap();
        assert!(c.matches(at("2026-10-19T03:00:00Z")));
        assert!(c.matches(at("2026-11-01T03:00:00Z")));
        assert!(!c.matches(at("2026-10-20T03:00:00Z")));
        // Sunday as 7 and as 0; 2026-10-18 is a Sunday
        assert!(
            Cron::parse("0 3 * * 7")
                .unwrap()
                .matches(at("2026-10-18T03:00:00Z"))
        );
        assert!(
            Cron::parse("0 3 * * 0")
                .unwrap()
                .matches(at("2026-10-18T03:00:00Z"))
        );
    }

    #[test]
    fn due_counts_whole_minutes_after_since() {
        let c = Cron::parse("* * * * *").unwrap();
        let t = at("2026-10-21T10:00:30Z");
        assert!(!due(&c, t, at("2026-10-21T10:00:59Z")), "same minute");
        assert!(due(&c, t, at("2026-10-21T10:01:00Z")));
        // Mondays 03:00; 2026-10-19 is a Monday: a day and an hour later it is out of the window
        let weekly = Cron::parse("0 3 * * 1").unwrap();
        let since = at("2026-10-01T00:00:00Z");
        assert!(due(&weekly, since, at("2026-10-20T02:59:00Z")));
        assert!(
            !due(&weekly, since, at("2026-10-20T03:01:00Z")),
            "a fire older than the window is dropped"
        );
    }
}
