//! `GET /api/platform/gates/usage` (gateLibraryUsage, T12; the B5 bucket of Revision 2026-10-22) → `GateUsage[]`:
//! per live library question key (by key), the tenant jobs that ask it (by tenant name, then job key), with the
//! tenant's refinements of it in that job and its last-7-days verdicts.
//!
//! - A job asks a library question when one of its `jobs.pins` is a tag holding a live `gate_question` row of that
//!   key (as [`super::pins`]; the platform library reaches a job only through its pins, B5 R1).
//! - `refinements` = the job's `job_gate_option_refinement` + `job_gate_rule_refinement` rows of the question
//!   (relabel / hide / threshold, B5 0143).
//! - `verdicts_7d` = the job's `gate_answer` rows (joined to their `gate_evaluation`) of the question in the last
//!   `stats::WINDOW_DAYS` days, as `job_design::gates::stats::read` counts them (B5 T8) but for all of a tenant's
//!   asked jobs in one query; `null` without traffic.
//!
//! Read the B7 way (R3): the tenant list as `agent_platform` (`agent_list_tenants()`), then each tenant's rows as
//! `agent_app` under RLS (`app.current_tenant`; `app.current_job` unset, so the RESTRICTIVE `job_scope` admits every
//! job of the tenant). One `READ ONLY` transaction, rolled back.
use std::collections::{BTreeMap, BTreeSet};

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::job_design::gates::stats;
use crate::platform::routes::read_tx;
use crate::platform_db;
use crate::state::AppState;
use crate::tenancy::{SuperAdmin, set_tenant};

/// UI `GateJobUse`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GateJobUse {
    pub tenant_id: String,
    pub tenant_name: String,
    pub job: String,
    pub job_label: String,
    pub refinements: i64,
    pub verdicts_7d: Option<u64>,
}

/// UI `GateUsage`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GateUsage {
    pub gate: String,
    pub jobs: Vec<GateJobUse>,
}

/// (job id, job key, job label, gate key).
type Asked = (Uuid, String, String, String);

/// Every job of the tenant set on `conn` that asks a library question, with the question.
async fn asked(conn: &mut PgConnection) -> ApiResult<Vec<Asked>> {
    Ok(sqlx::query_as(
        "SELECT DISTINCT j.id, j.key, j.label, t.key \
           FROM jobs j \
           CROSS JOIN LATERAL jsonb_array_elements(CASE jsonb_typeof(j.pins) WHEN 'array' THEN j.pins \
                                                       ELSE '[]'::jsonb END) p \
           JOIN layer_tag_row m ON m.family = p->>'layer' AND m.version = (p->>'version')::int \
           JOIN layer_row t ON t.id = m.row_id \
          WHERE t.kind = 'gate_question' AND t.op <> 'tombstone' \
          ORDER BY j.key, t.key",
    )
    .fetch_all(conn)
    .await?)
}

/// (job id, gate) → refinement rows, for the tenant set on `conn`.
async fn refinements(conn: &mut PgConnection) -> ApiResult<BTreeMap<(Uuid, String), i64>> {
    let rows: Vec<(Uuid, String, i64)> = sqlx::query_as(
        "SELECT job_id, gate, count(*) FROM ( \
                SELECT job_id, gate FROM job_gate_option_refinement \
                UNION ALL SELECT job_id, gate FROM job_gate_rule_refinement) r \
          GROUP BY job_id, gate",
    )
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(|(j, g, n)| ((j, g), n)).collect())
}

/// (job id, gate) → `verdicts_7d`, for `jobs` on `conn`: the same rows and window as [`stats::read`]'s `verdicts`
/// (`gate_answer` joined to `gate_evaluation`, `at >= now - WINDOW_DAYS`). A pair without traffic has no entry.
async fn verdicts_7d(
    conn: &mut PgConnection,
    jobs: &[Uuid],
    now: DateTime<Utc>,
) -> ApiResult<BTreeMap<(Uuid, String), u64>> {
    let since = now - Duration::days(stats::WINDOW_DAYS);
    let rows: Vec<(Uuid, String, i64)> = sqlx::query_as(
        "SELECT a.job_id, a.gate, count(*) \
           FROM gate_answer a \
           JOIN gate_evaluation e ON e.id = a.evaluation_id \
          WHERE a.job_id = ANY($1) AND a.at >= $2 \
          GROUP BY a.job_id, a.gate",
    )
    .bind(jobs)
    .bind(since)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(j, g, n)| ((j, g), u64::try_from(n).unwrap_or(0)))
        .collect())
}

/// The usage of every live library question, in the caller's read-only transaction (left as `agent_platform`).
pub async fn usage(conn: &mut PgConnection) -> ApiResult<Vec<GateUsage>> {
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT key FROM layer_head WHERE kind = 'gate_question' AND op <> 'tombstone'",
    )
    .fetch_all(&mut *conn)
    .await?;
    let mut by_gate: BTreeMap<String, Vec<GateJobUse>> =
        keys.into_iter().map(|k| (k, Vec::new())).collect();
    let mut tenants: Vec<(Uuid, String)> =
        sqlx::query_as("SELECT id, name FROM agent_list_tenants()")
            .fetch_all(&mut *conn)
            .await?;
    tenants.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)));
    platform_db::leave(&mut *conn).await?;
    let now = Utc::now();
    for (tenant, name) in tenants {
        set_tenant(&mut *conn, tenant).await?;
        let asked = asked(&mut *conn).await?;
        if asked.is_empty() {
            continue;
        }
        let refined = refinements(&mut *conn).await?;
        let jobs: Vec<Uuid> = asked
            .iter()
            .map(|a| a.0)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let verdicts = verdicts_7d(&mut *conn, &jobs, now).await?;
        for (job_id, job, job_label, gate) in asked {
            let Some(list) = by_gate.get_mut(&gate) else {
                continue; // a pinned tag holds a question the head no longer has live: not in the library
            };
            let verdicts_7d = verdicts.get(&(job_id, gate.clone())).copied();
            list.push(GateJobUse {
                tenant_id: tenant.to_string(),
                tenant_name: name.clone(),
                job,
                job_label,
                refinements: refined.get(&(job_id, gate.clone())).copied().unwrap_or(0),
                verdicts_7d,
            });
        }
    }
    platform_db::enter(&mut *conn).await?;
    Ok(by_gate
        .into_iter()
        .map(|(gate, jobs)| GateUsage { gate, jobs })
        .collect())
}

/// `GET /api/platform/gates/usage`.
pub async fn gate_library_usage(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Vec<GateUsage>>> {
    let mut tx = read_tx(&st).await?;
    let out = usage(&mut tx).await?;
    tx.rollback().await?;
    Ok(Json(out))
}
