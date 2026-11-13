//! J7 gate statistics (B5 T8): the last 7 days of a job's gate log (`gate_evaluation` + `gate_answer`, 0143), per
//! question. The log never holds customer or reply text; these are counts only.
//!
//! | `GateStats` field | From |
//! |---|---|
//! | `verdicts_7d` | `gate_answer` rows of the question |
//! | `fail_open` / `fail_closed` | of those, the evaluation's `outcome` `failed_open` / `failed_closed` |
//! | `below_threshold` | of those, `unsure` (under the rule's `min_confidence`) |
//! | `latency_p50_ms` / `latency_p95_ms` | `percentile_disc` of the evaluations' `latency_ms` |
//! | `timeouts` | of those, the evaluation's `error = 'timeout'` |
//! | `confidence` | choice questions: the answers' confidence in 20 buckets of 0.05 (absent without traffic) |
//!
//! Per option: `EffGateOption.verdicts` = answers whose `value` is the option's key.
use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::job_design::types::GateStats;

/// The stats window (days).
pub const WINDOW_DAYS: i64 = 7;
/// Confidence buckets of a choice question (0–0.05 … 0.95–1).
pub const BUCKETS: usize = 20;
/// `gate_evaluation.error` of a Laya call that ran out of time (T9 writes it).
pub const TIMEOUT_ERROR: &str = "timeout";

/// One question's last 7 days.
#[derive(Clone, Debug, PartialEq)]
pub struct QuestionStats {
    pub stats: GateStats,
    /// Option key → answers with that value.
    pub verdicts: BTreeMap<String, u64>,
    /// Confidence buckets (all answers that carry a confidence).
    pub buckets: Vec<u64>,
}

/// A job's last 7 days, by question key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub by_gate: BTreeMap<String, QuestionStats>,
    /// Since when the stats count; `None` without traffic.
    pub window_from: Option<DateTime<Utc>>,
}

/// No traffic.
pub fn zero() -> GateStats {
    GateStats {
        verdicts_7d: 0,
        fail_open: 0,
        fail_closed: 0,
        below_threshold: 0,
        latency_p50_ms: None,
        latency_p95_ms: None,
        timeouts: 0,
        confidence: None,
    }
}

impl Stats {
    /// The stats of `gate` (zero without traffic). `choice` = a choice question (the only kind with buckets).
    pub fn of(&self, gate: &str, choice: bool) -> GateStats {
        match self.by_gate.get(gate) {
            Some(q) => {
                let mut s = q.stats.clone();
                s.confidence = (choice && s.verdicts_7d > 0).then(|| q.buckets.clone());
                s
            }
            None => zero(),
        }
    }

    /// Answers of `gate` with value `option`.
    pub fn verdicts(&self, gate: &str, option: &str) -> u64 {
        self.by_gate
            .get(gate)
            .and_then(|q| q.verdicts.get(option))
            .copied()
            .unwrap_or(0)
    }
}

fn count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn ms(n: Option<i32>) -> Option<u64> {
    n.and_then(|v| u64::try_from(v).ok())
}

#[derive(sqlx::FromRow)]
struct Row {
    gate: String,
    verdicts: i64,
    fail_open: i64,
    fail_closed: i64,
    below: i64,
    p50: Option<i32>,
    p95: Option<i32>,
    timeouts: i64,
}

/// The last [`WINDOW_DAYS`] days of `job`'s gate log, as of `now` (the caller's transaction is job-scoped).
pub async fn read(c: &mut PgConnection, job: Uuid, now: DateTime<Utc>) -> ApiResult<Stats> {
    let since = now - Duration::days(WINDOW_DAYS);
    let rows = sqlx::query_as::<_, Row>(
        "SELECT a.gate, \
                count(*) AS verdicts, \
                count(*) FILTER (WHERE e.outcome = 'failed_open') AS fail_open, \
                count(*) FILTER (WHERE e.outcome = 'failed_closed') AS fail_closed, \
                count(*) FILTER (WHERE a.unsure) AS below, \
                percentile_disc(0.5) WITHIN GROUP (ORDER BY e.latency_ms) AS p50, \
                percentile_disc(0.95) WITHIN GROUP (ORDER BY e.latency_ms) AS p95, \
                count(*) FILTER (WHERE e.error = $3) AS timeouts \
           FROM gate_answer a \
           JOIN gate_evaluation e ON e.id = a.evaluation_id \
          WHERE a.job_id = $1 AND a.at >= $2 \
          GROUP BY a.gate",
    )
    .bind(job)
    .bind(since)
    .bind(TIMEOUT_ERROR)
    .fetch_all(&mut *c)
    .await?;
    let mut out = Stats::default();
    for r in rows {
        out.by_gate.insert(
            r.gate,
            QuestionStats {
                stats: GateStats {
                    verdicts_7d: count(r.verdicts),
                    fail_open: count(r.fail_open),
                    fail_closed: count(r.fail_closed),
                    below_threshold: count(r.below),
                    latency_p50_ms: ms(r.p50),
                    latency_p95_ms: ms(r.p95),
                    timeouts: count(r.timeouts),
                    confidence: None,
                },
                verdicts: BTreeMap::new(),
                buckets: vec![0; BUCKETS],
            },
        );
    }
    if out.by_gate.is_empty() {
        return Ok(out);
    }
    out.window_from = Some(since);
    let values: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT gate, value, count(*) FROM gate_answer \
          WHERE job_id = $1 AND at >= $2 AND value IS NOT NULL \
          GROUP BY gate, value",
    )
    .bind(job)
    .bind(since)
    .fetch_all(&mut *c)
    .await?;
    for (gate, value, n) in values {
        if let Some(q) = out.by_gate.get_mut(&gate) {
            q.verdicts.insert(value, count(n));
        }
    }
    let buckets: Vec<(String, i32, i64)> = sqlx::query_as(
        "SELECT gate, LEAST(GREATEST(floor(confidence / 0.05)::int, 0), 19) AS bucket, count(*) \
           FROM gate_answer \
          WHERE job_id = $1 AND at >= $2 AND confidence IS NOT NULL \
          GROUP BY 1, 2",
    )
    .bind(job)
    .bind(since)
    .fetch_all(&mut *c)
    .await?;
    for (gate, bucket, n) in buckets {
        if let (Some(q), Ok(i)) = (out.by_gate.get_mut(&gate), usize::try_from(bucket))
            && let Some(slot) = q.buckets.get_mut(i)
        {
            *slot += count(n);
        }
    }
    Ok(out)
}
