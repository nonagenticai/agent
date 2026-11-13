//! J7 target `gate_setting` (B5 T8a, F-16 / F-17; owner 2026-10-20): one job's setting of one gate question — on/off
//! and the text the customer reads when it hands over or blocks.
//!
//! `gate_setting` at `{gate}` = the `job_gate_question_setting` row `{enabled, customer_reply}` (null = the base: on,
//! the question's own text). `proj` = `[enabled, customer_reply]` (fixture `ledger.ts` `gate_setting`). Absent = the
//! library's. A value with both fields null is no setting: `set` removes the row (the table's CHECK
//! `num_nonnulls(enabled, customer_reply) >= 1`).
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::{SET_BY, decode, field, job_of, place_ref, present};
use crate::design::{DesignTarget, DraftRef, Place, Scope, Snap};
use crate::error::ApiResult;

/// The reserved gate key of the tenant's "I reviewed the gate questions" marker: a `gate_setting` row
/// `{enabled: true}` whose `updated_at` / `updated_by` are the review's time and actor. It is not a question key
/// (`is_gate_key` admits only `[A-Z][A-Z0-9]*`), and every reader of the settings looks one up BY a question's key,
/// so the marker is invisible to the gate runtime and to the release bundle (`release::compile` never reads
/// `gate_setting`); it only makes the job's gate rows non-default, which is what the `gates_unreviewed` readiness
/// gap tests.
pub const REVIEWED_KEY: &str = "_reviewed";

/// A setting as its place stores it.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
pub struct GateSetting {
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub customer_reply: Option<String>,
}

impl GateSetting {
    /// Both fields back to the base.
    pub fn is_base(&self) -> bool {
        self.enabled.is_none() && self.customer_reply.is_none()
    }

    pub fn value(&self) -> Value {
        json!({"enabled": self.enabled, "customer_reply": self.customer_reply})
    }
}

/// One job's setting of one gate question (absent = the library's).
pub struct GateSettingTarget;

impl DesignTarget for GateSettingTarget {
    fn kind(&self) -> &'static str {
        "gate_setting"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    /// Fixture `gate-setting:${gate}`.
    fn key(&self, r: &DraftRef) -> String {
        format!("gate-setting:{}", field(r, "gate"))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let row = sqlx::query_as::<_, GateSetting>(
                "SELECT enabled, customer_reply FROM job_gate_question_setting \
                  WHERE job_id = $1 AND gate = $2",
            )
            .bind(job_of(at)?)
            .bind(field(r, "gate"))
            .fetch_optional(&mut *c)
            .await?;
            Ok(row.map(|s| s.value()))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let gate = field(r, "gate");
            let s: GateSetting = match present(v) {
                Some(x) => decode("gate_setting", x)?,
                None => GateSetting::default(),
            };
            write(&mut *c, job, gate, &s, SET_BY).await
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let gates: Vec<String> = sqlx::query_scalar(
                "SELECT gate FROM job_gate_question_setting WHERE job_id = $1 ORDER BY gate",
            )
            .bind(job_of(at)?)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                gates
                    .iter()
                    .map(|g| place_ref(&[("gate", g.as_str())]))
                    .collect(),
            ))
        })
    }
    /// Fixture: `[enabled ?? null, customer_reply ?? null]`.
    fn proj(&self, v: &Snap) -> Value {
        let Some(x) = present(v) else {
            return Value::Null;
        };
        let pick = |f: &str| x.get(f).cloned().unwrap_or(Value::Null);
        json!([pick("enabled"), pick("customer_reply")])
    }
    /// Fixture: `off` · `reply “…”`, else `as the library has it`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let base = "as the library has it".to_string();
        let Some(x) = present(v) else {
            return Some(base);
        };
        let mut parts = Vec::new();
        if x.get("enabled") == Some(&Value::Bool(false)) {
            parts.push("off".to_string());
        }
        if let Some(t) = x
            .get("customer_reply")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            parts.push(format!("reply “{t}”"));
        }
        Some(if parts.is_empty() {
            base
        } else {
            parts.join(" · ")
        })
    }
}

/// Write setting `s` of `gate` (the route's mutation and the restore path): both fields at the base removes the row.
pub async fn write(
    c: &mut PgConnection,
    job: uuid::Uuid,
    gate: &str,
    s: &GateSetting,
    by: &str,
) -> ApiResult<()> {
    if s.is_base() {
        sqlx::query("DELETE FROM job_gate_question_setting WHERE job_id = $1 AND gate = $2")
            .bind(job)
            .bind(gate)
            .execute(&mut *c)
            .await?;
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO job_gate_question_setting (job_id, gate, enabled, customer_reply, updated_by) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (tenant_id, job_id, gate) DO UPDATE \
            SET enabled = EXCLUDED.enabled, customer_reply = EXCLUDED.customer_reply, \
                updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
    )
    .bind(job)
    .bind(gate)
    .bind(s.enabled)
    .bind(&s.customer_reply)
    .bind(by)
    .execute(&mut *c)
    .await?;
    Ok(())
}
