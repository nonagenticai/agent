//! Case signals for B8 (B4 T12; B8 plan "B4" row): read-only aggregates over `agent_case`, `agent_handover_items`,
//! `operation_call` and `answer_grounding` of one job. Every query runs on the caller's job-scoped transaction (RLS
//! `tenant_isolation` + `job_scope`) and filters by `job.id` as well. Ids, codes and counts only (K50): no customer or
//! staff text leaves these functions.
//!
//! Definitions, shared by every function here:
//! - a case is **escalated** when it ever had a handover item (any reason, any state);
//! - a case is **contained** when it ended `resolved` and was never escalated;
//! - the live numbers are Production (`env = 'prod'`) only; Test-env cases try the draft and are not traffic.
use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgConnection;

use crate::design::JobRef;
use crate::error::ApiResult;

/// J4 `ActivityOutcome` (web `types/journey.ts`): Production cases of one activity over the window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct ActivityOutcome {
    /// `stage:activity`.
    pub activity: String,
    pub conversations: i64,
    pub contained: i64,
    pub escalated: i64,
}

/// Outcomes per activity of the job's Production cases started in the last `days` days, busiest first. Cases not
/// qualified to an activity are left out.
pub async fn activity_outcomes(
    c: &mut PgConnection,
    job: &JobRef,
    days: i32,
) -> ApiResult<Vec<ActivityOutcome>> {
    Ok(sqlx::query_as::<_, ActivityOutcome>(
        "SELECT c.activity, count(DISTINCT c.conversation_id) AS conversations, \
                count(*) FILTER (WHERE c.status = 'resolved' AND NOT h.escalated) AS contained, \
                count(*) FILTER (WHERE h.escalated) AS escalated \
           FROM agent_case c \
           CROSS JOIN LATERAL (SELECT EXISTS (SELECT 1 FROM agent_handover_items i \
                                               WHERE i.job_id = c.job_id AND i.case_id = c.case_id) AS escalated) h \
          WHERE c.job_id = $1 AND c.env = 'prod' AND COALESCE(c.activity, '') <> '' \
            AND c.started_at >= now() - make_interval(days => $2) \
          GROUP BY c.activity \
          ORDER BY conversations DESC, c.activity",
    )
    .bind(job.id)
    .bind(days)
    .fetch_all(&mut *c)
    .await?)
}

/// J5 `use_case.live` (web `types/journey.ts`): Production cases of one activity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct LiveCounts {
    pub cases: i64,
    pub contained: i64,
    pub escalated: i64,
    /// Median `duration_ms` of the cases that ended; `None` when none did.
    pub p50_ms: Option<i64>,
}

/// J5 `use_case.outcomes[]`: one outcome code and how often it ended a case.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct OutcomeCount {
    pub code: String,
    pub count: i64,
}

/// J5 `use_case.live` + `use_case.outcomes` of one activity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UseCaseLive {
    /// `None` when the activity had no Production case in the window.
    pub live: Option<LiveCounts>,
    /// Outcome codes of those cases, most frequent first (at most 20).
    pub outcomes: Vec<OutcomeCount>,
}

/// The live numbers and outcome codes of `activity` (`stage:activity`) over the last `days` days (Production).
pub async fn use_case_live(
    c: &mut PgConnection,
    job: &JobRef,
    activity: &str,
    days: i32,
) -> ApiResult<UseCaseLive> {
    let live = sqlx::query_as::<_, LiveCounts>(
        "SELECT count(*) AS cases, \
                count(*) FILTER (WHERE c.status = 'resolved' AND NOT h.escalated) AS contained, \
                count(*) FILTER (WHERE h.escalated) AS escalated, \
                (percentile_cont(0.5) WITHIN GROUP (ORDER BY c.duration_ms))::int8 AS p50_ms \
           FROM agent_case c \
           CROSS JOIN LATERAL (SELECT EXISTS (SELECT 1 FROM agent_handover_items i \
                                               WHERE i.job_id = c.job_id AND i.case_id = c.case_id) AS escalated) h \
          WHERE c.job_id = $1 AND c.env = 'prod' AND c.activity = $2 \
            AND c.started_at >= now() - make_interval(days => $3)",
    )
    .bind(job.id)
    .bind(activity.to_owned())
    .bind(days)
    .fetch_one(&mut *c)
    .await?;
    let outcomes = sqlx::query_as::<_, OutcomeCount>(
        "SELECT outcome AS code, count(*) AS count FROM agent_case \
          WHERE job_id = $1 AND env = 'prod' AND activity = $2 AND outcome IS NOT NULL \
            AND started_at >= now() - make_interval(days => $3) \
          GROUP BY outcome \
          ORDER BY count(*) DESC, outcome \
          LIMIT 20",
    )
    .bind(job.id)
    .bind(activity.to_owned())
    .bind(days)
    .fetch_all(&mut *c)
    .await?;
    Ok(UseCaseLive {
        live: (live.cases > 0).then_some(live),
        outcomes,
    })
}

/// J5 `UseCaseCase` (web `types/journey.ts`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct RecentCase {
    pub case_id: String,
    pub title: String,
    pub status: String,
    pub outcome: Option<String>,
    pub identity_level: i32,
    pub env: String,
    pub started_at: DateTime<Utc>,
}

/// The newest `limit` cases of `activity` (both environments; the row says which), newest first.
pub async fn recent_for_activity(
    c: &mut PgConnection,
    job: &JobRef,
    activity: &str,
    limit: i64,
) -> ApiResult<Vec<RecentCase>> {
    Ok(sqlx::query_as::<_, RecentCase>(sqlx::AssertSqlSafe(format!(
        "SELECT case_id, title, status, outcome, {level} AS identity_level, env, started_at \
           FROM agent_case \
          WHERE job_id = $1 AND activity = $2 \
          ORDER BY started_at DESC, case_id DESC \
          LIMIT $3",
        level = crate::conversations::state::level_sql("agent_case"),
    )))
    .bind(job.id)
    .bind(activity.to_owned())
    .bind(limit.clamp(0, 200))
    .fetch_all(&mut *c)
    .await?)
}

/// Agent replies of one request type that said "I don't know" (`answer_grounding.dont_know`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct KnowledgeMiss {
    /// The case's request type (`''` when the reply belongs to no case).
    pub request_type: String,
    /// Agent replies with a grounding row in the window.
    pub replies: i64,
    /// Of those, the "I don't know" replies.
    pub misses: i64,
}

/// Knowledge misses per request type of the job's Production replies in the last `days` days, most misses first;
/// request types without a miss are left out.
pub async fn knowledge_misses(
    c: &mut PgConnection,
    job: &JobRef,
    days: i32,
) -> ApiResult<Vec<KnowledgeMiss>> {
    Ok(sqlx::query_as::<_, KnowledgeMiss>(
        "SELECT COALESCE(k.request_type, '') AS request_type, count(*) AS replies, \
                count(*) FILTER (WHERE g.dont_know) AS misses \
           FROM answer_grounding g \
           JOIN conversation_message m ON m.message_id = g.message_id \
           JOIN conversation v ON v.tenant_id = m.tenant_id AND v.job_id = m.job_id \
                              AND v.conversation_id = m.conversation_id \
           LEFT JOIN agent_case k ON k.tenant_id = m.tenant_id AND k.job_id = m.job_id AND k.case_id = m.case_id \
          WHERE g.job_id = $1 AND v.env = 'prod' \
            AND m.at_ms >= (extract(epoch FROM now() - make_interval(days => $2)) * 1000)::int8 \
          GROUP BY COALESCE(k.request_type, '') \
         HAVING count(*) FILTER (WHERE g.dont_know) > 0 \
          ORDER BY misses DESC, request_type",
    )
    .bind(job.id)
    .bind(days)
    .fetch_all(&mut *c)
    .await?)
}

/// An operation the runtime asked for that no route offers (`operation_call.route = 'not_offered'`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, sqlx::FromRow)]
pub struct UnroutedCall {
    /// `object.verb{variant}`.
    pub operation: String,
    /// `prod` or `test`.
    pub env: String,
    pub calls: i64,
    pub last_at: DateTime<Utc>,
}

/// Unrouted operation calls of the job in the last `days` days per operation and env, most calls first.
pub async fn unrouted_calls(
    c: &mut PgConnection,
    job: &JobRef,
    days: i32,
) -> ApiResult<Vec<UnroutedCall>> {
    Ok(sqlx::query_as::<_, UnroutedCall>(
        "SELECT operation, env, count(*) AS calls, max(started_at) AS last_at \
           FROM operation_call \
          WHERE job_id = $1 AND route = 'not_offered' \
            AND started_at >= now() - make_interval(days => $2) \
          GROUP BY operation, env \
          ORDER BY calls DESC, operation, env",
    )
    .bind(job.id)
    .bind(days)
    .fetch_all(&mut *c)
    .await?)
}
