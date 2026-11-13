//! `discoveryState` (fixture `stateOf` + `journey-J18.ts`): `imports` (latest first; a classifying one with
//! `progress`, `coverage` null; a cluster `accepted` once its proposal is), `shadow`, the scorecard weight (F-51)
//! folded into every import's `volumes[]`, `runs` (this job's generator runs), `signals` (the traffic sources and
//! their volumes), `proposals` ([`super::proposals`]) and `usage` (discovery + generator `usage_event`s of the job,
//! read back, not recomputed).
//!
//! F-51: `setScorecardWeight` upserts an integer 0..1000 per activity (100 = default = no row); `valuable` =
//! the activity's weighted volume share × weight / 100. An activity is known to discovery when it is one of the job's
//! journey activities (`journey::job`, the effective activities; else 404 `activity_not_found`).
use agent_model::compile::parse_department;
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::cases::signals as case_signals;
use crate::design::{DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::generator::{run as gen_run, store as gen_store};
use crate::job_design::base;
use crate::journey::job as journey_job;
use crate::state::AppState;
use crate::tenancy::JobCtx;

use super::{proposals, shadow};

/// Default weight (no row).
pub const DEFAULT_WEIGHT: i64 = 100;
/// Largest weight.
pub const MAX_WEIGHT: i64 = 1000;

pub fn invalid_weight() -> ApiError {
    ApiError::Unprocessable(
        "invalid_weight",
        "The weight is an integer from 0 to 1000 (100 = default).".into(),
    )
}

/// `{weight}` → 0..=1000, else 422 `invalid_weight` (a fraction, a string, a negative or a bigger number).
pub fn weight_of(body: &Value) -> ApiResult<i32> {
    let w = body
        .get("weight")
        .and_then(Value::as_i64)
        .filter(|w| (0..=MAX_WEIGHT).contains(w))
        .ok_or_else(invalid_weight)?;
    i32::try_from(w).map_err(|_| invalid_weight())
}

/// `setScorecardWeight`: 404 `activity_not_found` for an activity the job's journey does not have; 100 deletes the row.
pub async fn set_weight(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobCtx,
    activity: &str,
    weight: i32,
) -> ApiResult<()> {
    let jr = JobRef {
        id: job.job_id,
        key: job.job_key.clone(),
    };
    // the activities discovery classifies against (the template's, `base::at`) or the journey's effective ones
    let bases = base::at(
        &mut *c,
        &st.design,
        st.compiler.as_ref(),
        &jr,
        &DesignOf::Draft,
    )
    .await?;
    let known = bases.activity(activity).is_some()
        || journey_job::job_journey(&mut *c, st, &jr, &DesignOf::Draft)
            .await
            .is_ok_and(|j| j.activities.iter().any(|a| a.activity.key == activity));
    if !known {
        return Err(ApiError::Missing(
            "activity_not_found",
            "This job's journey has no such activity.".into(),
        ));
    }
    if i64::from(weight) == DEFAULT_WEIGHT {
        sqlx::query(
            "DELETE FROM discovery_weight WHERE tenant_id = $1 AND job_id = $2 AND activity_key = $3",
        )
        .bind(job.t.tenant)
        .bind(job.job_id)
        .bind(activity)
        .execute(&mut *c)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO discovery_weight (tenant_id, job_id, activity_key, weight, updated_by) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant_id, job_id, activity_key) DO UPDATE \
                SET weight = EXCLUDED.weight, updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
        )
        .bind(job.t.tenant)
        .bind(job.job_id)
        .bind(activity)
        .bind(weight)
        .bind(&job.t.caller.sub)
        .execute(&mut *c)
        .await?;
    }
    Ok(())
}

/// The imports of the job, latest first, each with its volumes (weight + valuable) and clusters.
const IMPORTS: &str = "
SELECT coalesce(jsonb_agg(v ORDER BY seq DESC), '[]'::jsonb) FROM (
  SELECT i.seq, jsonb_build_object(
           'id', i.id, 'file', i.file, 'rows', i.rows, 'distinct_rows', i.distinct_rows,
           'uploaded_at', i.uploaded_at, 'uploaded_by', i.uploaded_by, 'status', i.status,
           'progress', CASE WHEN i.status = 'classifying' AND i.distinct_rows > 0
                            THEN round((i.done::numeric / i.distinct_rows), 4)::double precision
                            WHEN i.status = 'classifying' THEN 0 ELSE 1 END,
           'coverage', CASE WHEN i.status = 'classified' THEN i.coverage END,
           'long_tail', i.long_tail,
           'format', i.format, 'text_column', i.text_column, 'count_column', i.count_column,
           'error', i.error,
           'volumes', coalesce((
               SELECT jsonb_agg(vol.e || jsonb_build_object(
                        'weight', coalesce(w.weight, 100),
                        'valuable', CASE WHEN tot.w = 0 THEN 0
                                         ELSE round(((vol.e->>'tickets')::numeric / tot.w)
                                                    * coalesce(w.weight, 100) / 100, 4)::double precision END)
                      ORDER BY vol.ord)
                 FROM jsonb_array_elements(i.volumes) WITH ORDINALITY AS vol(e, ord)
                 CROSS JOIN (SELECT coalesce(sum(r.weight), 0)::numeric AS w FROM discovery_row r
                              WHERE r.tenant_id = i.tenant_id AND r.job_id = i.job_id AND r.import_id = i.id) tot
                 LEFT JOIN discovery_weight w
                        ON w.tenant_id = i.tenant_id AND w.job_id = i.job_id AND w.activity_key = vol.e->>'activity'),
             '[]'::jsonb),
           'clusters', coalesce((
               SELECT jsonb_agg(jsonb_build_object(
                        'id', k.id, 'label', k.label, 'tickets', k.tickets, 'examples', k.examples,
                        'suggestion', k.suggestion,
                        'state', CASE WHEN d.state = 'accepted' THEN 'accepted' ELSE 'open' END,
                        'accepted_at', CASE WHEN d.state = 'accepted' THEN d.at END,
                        'accepted_by', CASE WHEN d.state = 'accepted' THEN d.\"by\" END)
                      ORDER BY k.tickets DESC, k.id)
                 FROM discovery_cluster k
                LEFT JOIN discovery_decision d
                       ON d.tenant_id = k.tenant_id AND d.job_id = k.job_id
                      AND d.proposal_id = 'p-cl-' || regexp_replace(lower(k.import_id), '[^a-z0-9_]', '-', 'g')
                                          || '-' || regexp_replace(lower(k.id), '[^a-z0-9_]', '-', 'g')
                WHERE k.tenant_id = i.tenant_id AND k.job_id = i.job_id AND k.import_id = i.id),
             '[]'::jsonb)) AS v
    FROM discovery_import i
   WHERE i.tenant_id = $1 AND i.job_id = $2
) x";

const SIGNAL_DAYS: i32 = proposals::WINDOW_DAYS;

fn plural(n: i64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The traffic sources discovery reads and their volumes.
async fn signals(c: &mut PgConnection, job: &JobCtx, shadow: &Value) -> ApiResult<Value> {
    let jr = JobRef {
        id: job.job_id,
        key: job.job_key.clone(),
    };
    let rows: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(rows), 0)::bigint FROM discovery_import \
          WHERE tenant_id = $1 AND job_id = $2 AND status = 'classified'",
    )
    .bind(job.t.tenant)
    .bind(job.job_id)
    .fetch_one(&mut *c)
    .await?;
    let conversations: i64 = case_signals::activity_outcomes(&mut *c, &jr, SIGNAL_DAYS)
        .await?
        .iter()
        .map(|o| o.conversations)
        .sum();
    let replies: i64 = case_signals::knowledge_misses(&mut *c, &jr, SIGNAL_DAYS)
        .await?
        .iter()
        .map(|m| m.replies)
        .sum();
    let shadowed = shadow["conversations"].as_i64().unwrap_or(0);
    let window = format!("last {SIGNAL_DAYS} days \u{b7} prod");
    Ok(json!([
        {"source": "import", "label": format!("Uploaded exports ({})", plural(rows, "ticket", "tickets")),
         "count": rows, "unit": "tickets", "window": "all uploads", "active": rows > 0},
        {"source": "live", "label": "Production conversations", "count": conversations,
         "unit": "conversations", "window": window, "active": conversations > 0},
        {"source": "knowledge_search", "label": "Knowledge searches that found nothing", "count": replies,
         "unit": "searches", "window": window, "active": replies > 0},
        {"source": "shadow", "label": "Shadow traffic", "count": shadowed, "unit": "conversations",
         "window": "since shadow mode started", "active": shadow["active"].as_bool().unwrap_or(false)},
    ]))
}

/// Discovery's and the generator's LLM usage of the job in the window.
async fn usage(c: &mut PgConnection, job: &JobCtx) -> ApiResult<Value> {
    let rows: Vec<(String, String, String, i64, i64)> = sqlx::query_as(
        "SELECT source, use, funding, coalesce(sum(calls), 0)::bigint, \
                coalesce(sum(tokens_in + tokens_out), 0)::bigint \
           FROM usage_event \
          WHERE tenant_id = $1 AND job_id = $2 AND source IN ('discovery', 'generator') \
            AND occurred_at >= now() - make_interval(days => $3) \
          GROUP BY source, use, funding ORDER BY source, use, funding",
    )
    .bind(job.t.tenant)
    .bind(job.job_id)
    .bind(SIGNAL_DAYS)
    .fetch_all(&mut *c)
    .await?;
    let tenant: String = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(job.t.tenant)
        .fetch_optional(&mut *c)
        .await?
        .unwrap_or_else(|| job.t.tenant.to_string());
    let total: i64 = rows.iter().map(|r| r.4).sum();
    let lines: Vec<Value> = rows
        .into_iter()
        .map(|(source, used, funding, calls, tokens)| {
            let what = match (source.as_str(), used.as_str()) {
                ("discovery", "classify") => "Ticket classification".to_owned(),
                ("discovery", "cluster") => "Cluster drafting".to_owned(),
                (s, u) => format!("{s} {u}"),
            };
            json!({"what": what, "detail": plural(calls, "call", "calls"), "tokens": tokens,
                   "activity": source, "funding": funding})
        })
        .collect();
    Ok(
        json!({"tenant": tenant, "window": format!("last {SIGNAL_DAYS} days"), "lines": lines, "tokens": total}),
    )
}

/// The generator runs of this job (tenant runs; the job's tenant sees only its own).
async fn runs(c: &mut PgConnection, job: &JobCtx) -> ApiResult<Vec<Value>> {
    let mut out = Vec::new();
    for r in gen_store::list(&mut *c).await? {
        if r.job_id != Some(job.job_id) {
            continue;
        }
        let d = gen_store::detail(&mut *c, r.id).await?;
        let (_, findings) = gen_run::hydrate(&d);
        let mut v = gen_run::summary_json(&d.run, None, gen_run::red_of(&findings));
        if let Some(o) = v.as_object_mut() {
            o.insert("started_by_name".into(), json!(d.run.started_by));
            o.insert("tagged".into(), json!(d.run.tagged));
        }
        out.push(v);
    }
    Ok(out)
}

/// The job's industry and primary department (D115: `selection.departments[0]`), each null when the draft selects
/// none (a base-only preview) or its bases cannot be read.
async fn hierarchy(
    c: &mut PgConnection,
    st: &AppState,
    job: &JobCtx,
) -> (Option<String>, Option<String>) {
    let jr = JobRef {
        id: job.job_id,
        key: job.job_key.clone(),
    };
    let Ok(bases) = base::at(
        &mut *c,
        &st.design,
        st.compiler.as_ref(),
        &jr,
        &DesignOf::Draft,
    )
    .await
    else {
        return (None, None);
    };
    let sel = &bases.selection;
    let department = sel
        .departments
        .first()
        .and_then(|k| parse_department(k))
        .filter(|(industry, _)| *industry == sel.industry)
        .map(|(_, department)| department);
    (
        Some(sel.industry.clone()).filter(|i| !i.is_empty()),
        department,
    )
}

/// `discoveryState` (module docs).
pub async fn view(c: &mut PgConnection, st: &AppState, job: &JobCtx) -> ApiResult<Value> {
    let imports: Value = sqlx::query_scalar(IMPORTS)
        .bind(job.t.tenant)
        .bind(job.job_id)
        .fetch_one(&mut *c)
        .await?;
    let shadow = shadow::view(&mut *c, job).await?;
    let signals = signals(&mut *c, job, &shadow).await?;
    let proposals = proposals::view(&mut *c, st, job).await?;
    let usage = usage(&mut *c, job).await?;
    let runs = runs(&mut *c, job).await?;
    let (industry, department) = hierarchy(&mut *c, st, job).await;
    Ok(json!({
        "job": job.job_key,
        "industry": industry,
        "department": department,
        "imports": imports,
        "shadow": shadow,
        "runs": runs,
        "signals": signals,
        "proposals": proposals,
        "usage": usage,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_are_integers_in_range() {
        assert_eq!(weight_of(&json!({"weight": 0})).unwrap(), 0);
        assert_eq!(weight_of(&json!({"weight": 1000})).unwrap(), 1000);
        for bad in [
            json!({"weight": 1001}),
            json!({"weight": -1}),
            json!({"weight": 1.5}),
            json!({"weight": "5"}),
            json!({}),
        ] {
            assert!(weight_of(&bad).is_err(), "{bad}");
        }
    }
}
