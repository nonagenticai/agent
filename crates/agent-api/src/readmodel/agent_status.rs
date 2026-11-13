//! The one agent status (console redesign T6): `GET /api/jobs/{job}/status` (`AgentStatus`). Every surface that
//! says whether the agent serves customers (the job dashboard, the tenant home and job board, the top bar) reads it
//! here, so two pages cannot disagree.
//!
//! The state, in this order:
//! 1. the tenant is not active, or its licence verdict is not `active` → `paused`;
//! 2. no current release → `ready` when the draft has no blocking readiness gap, else `draft`;
//! 3. a current release and the bus does not answer → `live_with_issues` (the owner ruling: the agent is not
//!    answering customers, so it must not say "Live"; the job attention carries a blocking item for it);
//! 4. a current release and a blocking attention item (`urgent > 0`) → `live_with_issues`;
//! 5. otherwise `live`.
//!
//! `urgent` / `review` are the `blocking` / `action` counts of [`super::job_attention`], the same list the inbox
//! shows. `open_cases` counts the job's Production cases that are still open (`agent` = the agent holds it, `staff` =
//! handed to staff, `waiting` = waiting on the customer, `failed` = open with an alert) and the open test-chat cases
//! (`staff_tests`).
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;

use crate::conversations::iso;
use crate::design::{self, JobRef};
use crate::error::ApiResult;
use crate::licensing::verdict;
use crate::readmodel::{job_attention, tenant};
use crate::routes::analytics::{self, ratio};
use crate::routes::overview::STEP;
use crate::state::AppState;
use crate::tenancy::JobCtx;

/// The release that serves now. `label` is the human date (`23 Oct release`); `id` is the raw `cfg-…` id.
#[derive(Debug, Clone, Serialize)]
pub struct LiveRelease {
    pub id: String,
    pub label: String,
    pub since: String,
}

/// The job's open cases right now.
#[derive(Debug, Clone, Default, Serialize)]
pub struct OpenCases {
    pub agent: i64,
    pub staff: i64,
    pub waiting: i64,
    pub failed: i64,
    pub staff_tests: i64,
}

/// What the next release would carry.
#[derive(Debug, Clone, Serialize)]
pub struct NextRelease {
    pub draft_rev: i64,
    /// Blocking readiness gaps of the draft.
    pub blockers: i64,
    /// Unpublished changes in the draft.
    pub changes: i64,
}

/// The job's test suite (the validation questions) against its current release, else its newest one.
/// `state` is `no_questions | not_run | running | passing | failing`.
#[derive(Debug, Clone, Serialize)]
pub struct TestSuite {
    pub state: &'static str,
    pub release_id: Option<String>,
    pub questions: i64,
    pub passed: i64,
    pub failed: i64,
    pub pending: i64,
    pub last_run_at: Option<String>,
}

/// The suite's word from its facts: a running grading wins, then no questions, then failures (or a failed
/// grading), then never run, else passing.
pub fn tests_state_of(
    running: bool,
    questions: i64,
    failed: i64,
    grid_failed: bool,
    ran: bool,
) -> &'static str {
    if running {
        "running"
    } else if questions == 0 {
        "no_questions"
    } else if failed > 0 || grid_failed {
        "failing"
    } else if !ran {
        "not_run"
    } else {
        "passing"
    }
}

const LIVE_COUNTS: &str = "SELECT count(*), \
        count(*) FILTER (WHERE r.verdict = 'pass'), \
        count(*) FILTER (WHERE r.verdict = 'rejected') \
   FROM validation_question q \
   LEFT JOIN validation_result r ON r.question_id = q.id AND r.job_id = q.job_id AND r.release_id = $2 \
  WHERE q.job_id = $1 AND q.deleted_at IS NULL";

const STORED_COUNTS: &str = "SELECT count(*), \
        count(*) FILTER (WHERE r.verdict = 'pass'), \
        count(*) FILTER (WHERE r.verdict = 'rejected') \
   FROM validation_result r WHERE r.job_id = $1 AND r.release_id = $2";

async fn test_suite(
    c: &mut PgConnection,
    job: &JobRef,
    current: Option<&str>,
) -> ApiResult<TestSuite> {
    let release: Option<(String, String)> =
        match current {
            Some(id) => sqlx::query_as(
                "SELECT release_id, state FROM job_release WHERE job_id = $1 AND release_id = $2",
            )
            .bind(job.id)
            .bind(id)
            .fetch_optional(&mut *c)
            .await?,
            None => {
                sqlx::query_as(
                    "SELECT release_id, state FROM job_release WHERE job_id = $1 \
                  ORDER BY approved_at DESC, release_id DESC LIMIT 1",
                )
                .bind(job.id)
                .fetch_optional(&mut *c)
                .await?
            }
        };
    let Some((release_id, rel_state)) = release else {
        let questions: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM validation_question WHERE job_id = $1 AND deleted_at IS NULL",
        )
        .bind(job.id)
        .fetch_one(&mut *c)
        .await?;
        return Ok(TestSuite {
            state: tests_state_of(false, questions, 0, false, false),
            release_id: None,
            questions,
            passed: 0,
            failed: 0,
            pending: questions,
            last_run_at: None,
        });
    };
    let sql = if rel_state == "applied_earlier" {
        STORED_COUNTS
    } else {
        LIVE_COUNTS
    };
    let (questions, passed, failed): (i64, i64, i64) = sqlx::query_as(sql)
        .bind(job.id)
        .bind(&release_id)
        .fetch_one(&mut *c)
        .await?;
    let cut: Option<DateTime<Utc>> = sqlx::query_scalar(
        "SELECT ran_at FROM validation_cut_run WHERE job_id = $1 AND release_id = $2",
    )
    .bind(job.id)
    .bind(&release_id)
    .fetch_optional(&mut *c)
    .await?;
    let grid: Option<(String, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT status, finished_at FROM validation_grid_run WHERE job_id = $1 AND release_id = $2 \
          ORDER BY queued_at DESC, id DESC LIMIT 1",
    )
    .bind(job.id)
    .bind(&release_id)
    .fetch_optional(&mut *c)
    .await?;
    let status = grid.as_ref().map(|g| g.0.as_str());
    let running = matches!(status, Some("queued" | "running"));
    let grid_failed = status == Some("failed");
    let ran = cut.is_some() || matches!(status, Some("done" | "failed"));
    let last = [cut, grid.as_ref().and_then(|g| g.1)]
        .into_iter()
        .flatten()
        .max();
    Ok(TestSuite {
        state: tests_state_of(running, questions, failed, grid_failed, ran),
        release_id: Some(release_id),
        questions,
        passed,
        failed,
        pending: questions - passed - failed,
        last_run_at: last.map(iso),
    })
}

/// `AgentStatus`: `state` is `draft | ready | live | live_with_issues | paused`.
#[derive(Debug, Clone, Serialize)]
pub struct AgentStatus {
    pub state: &'static str,
    pub live_release: Option<LiveRelease>,
    pub on_bus: bool,
    pub urgent: i64,
    pub review: i64,
    pub open_cases: OpenCases,
    pub resolved_by_agent_30d: Option<f64>,
    pub next_release: Option<NextRelease>,
    pub tests: TestSuite,
}

/// The status word from its facts (see the module doc for the order).
pub fn state_of(
    paused: bool,
    has_release: bool,
    blockers: i64,
    on_bus: bool,
    urgent: i64,
) -> &'static str {
    if paused {
        "paused"
    } else if !has_release {
        if blockers == 0 { "ready" } else { "draft" }
    } else if !on_bus || urgent > 0 {
        "live_with_issues"
    } else {
        "live"
    }
}

/// Does the message bus answer? A PING bounded by [`STEP`]; `false` when `REDIS_URL` is not set. The one PING in
/// the crate's read models: every request probes once and hands the answer on (`job_attention::collect`).
pub async fn bus_up(st: &AppState) -> bool {
    let Some(root) = st.redis.as_ref() else {
        return false;
    };
    matches!(
        tokio::time::timeout(
            STEP,
            redis::cmd("PING").query_async::<String>(&mut root.conn()),
        )
        .await,
        Ok(Ok(_))
    )
}

/// The human label of a release applied at `since`: `23 Oct release`.
pub fn release_label(since: DateTime<Utc>) -> String {
    format!("{} release", since.format("%-d %b"))
}

/// The job's open cases (see the module doc).
async fn open_cases(c: &mut PgConnection, job: &JobRef) -> ApiResult<OpenCases> {
    let (agent, staff, waiting, failed, staff_tests): (i64, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT count(*) FILTER (WHERE env = 'prod' AND status = 'open'), \
                count(*) FILTER (WHERE env = 'prod' AND status = 'handover'), \
                count(*) FILTER (WHERE env = 'prod' AND status IN ('identity_required', 'confirmation_required')), \
                count(*) FILTER (WHERE env = 'prod' AND alert IS NOT NULL), \
                count(*) FILTER (WHERE env = 'test') \
           FROM agent_case \
          WHERE job_id = $1 \
            AND status IN ('open', 'identity_required', 'confirmation_required', 'handover')",
    )
    .bind(job.id)
    .fetch_one(&mut *c)
    .await?;
    Ok(OpenCases {
        agent,
        staff,
        waiting,
        failed,
        staff_tests,
    })
}

/// The one status of `job`. Opens its own transactions (the bus is probed with none held).
pub async fn agent_status(st: &AppState, j: &JobCtx, job: &JobRef) -> ApiResult<AgentStatus> {
    let bus = bus_up(st).await;
    let attention = job_attention::collect(st, j, job, bus, true).await?;
    let count = |severity: &str| {
        attention
            .items
            .iter()
            .filter(|i| i["severity"] == severity)
            .count() as i64
    };
    let (urgent, review) = (count("blocking"), count("action"));
    let on_bus = attention.bus_up;
    // the draft compile `collect` already made rides along: one compile per request
    let draft = attention.draft;

    let mut tx = j.begin(&st.db).await?;
    design::ensure_job(&mut tx, &st.design, job).await?;
    let tenant_row = tenant::read(&mut tx, j.t.tenant).await?;
    let licence = verdict::read(&mut tx, j.t.tenant).await?;
    let paused = tenant_row.as_ref().is_some_and(|t| t.status != "active")
        || licence.as_ref().is_some_and(|v| v.status != "active");
    let current: Option<(String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT r.release_id, COALESCE(r.released_at, r.approved_at) FROM jobs j \
           JOIN job_release r ON r.tenant_id = j.tenant_id AND r.job_id = j.id \
                             AND r.release_id = j.current_release_id \
          WHERE j.id = $1",
    )
    .bind(job.id)
    .fetch_optional(&mut *tx)
    .await?;
    let draft_rev: i64 = sqlx::query_scalar("SELECT draft_rev FROM jobs WHERE id = $1")
        .bind(job.id)
        .fetch_one(&mut *tx)
        .await?;
    let changes: i64 = sqlx::query_scalar("SELECT count(*) FROM draft_ledger WHERE job_id = $1")
        .bind(job.id)
        .fetch_one(&mut *tx)
        .await?;
    // the draft is compiled only when the answer needs it: nothing is live yet, or the draft moved past the release
    let blockers = if current.is_none() || changes > 0 {
        st.readiness
            .draft_blocking_with(&mut tx, &st.design, st.compiler.as_ref(), job, Some(draft))
            .await?
            .len() as i64
    } else {
        0
    };
    let open = open_cases(&mut tx, job).await?;
    let tests = test_suite(&mut tx, job, current.as_ref().map(|c| c.0.as_str())).await?;
    let w = analytics::window(&mut tx, 30).await?;
    let o = analytics::outcomes(&mut tx, job.id, w.from, w.to).await?;
    tx.commit().await?;

    let state = state_of(paused, current.is_some(), blockers, on_bus, urgent);
    Ok(AgentStatus {
        state,
        live_release: current.map(|(id, since)| LiveRelease {
            id,
            label: release_label(since),
            since: iso(since),
        }),
        on_bus,
        urgent,
        review,
        open_cases: open,
        resolved_by_agent_30d: ratio(o.contained, o.cases),
        next_release: (changes > 0 || blockers > 0).then_some(NextRelease {
            draft_rev,
            blockers,
            changes,
        }),
        tests,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_follows_the_rule_in_order() {
        // paused beats everything
        assert_eq!(state_of(true, true, 0, true, 0), "paused");
        // nothing live: ready without a blocking gap, draft with one
        assert_eq!(state_of(false, false, 0, false, 0), "ready");
        assert_eq!(state_of(false, false, 3, true, 0), "draft");
        // live: the bus down or an urgent item is live_with_issues (never "live")
        assert_eq!(state_of(false, true, 0, false, 0), "live_with_issues");
        assert_eq!(state_of(false, true, 0, true, 2), "live_with_issues");
        assert_eq!(state_of(false, true, 5, true, 0), "live");
    }

    #[test]
    fn the_release_label_is_the_day_and_month() {
        let t: DateTime<Utc> = "2026-10-23T09:30:00Z".parse().unwrap();
        assert_eq!(release_label(t), "23 Oct release");
        let t: DateTime<Utc> = "2026-10-05T09:30:00Z".parse().unwrap();
        assert_eq!(release_label(t), "5 Oct release");
    }
}
