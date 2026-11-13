//! B8 T8: metering of a drafted step (D56, R7). Each drafting call is one B4 `usage_event` (source `generator`,
//! `use` = the call: the step key, or `journey_merge`) and one `generator_cost_line` (0171) carrying that event's
//! key; the run's cost is the sum of its lines. Attribution is by the connection: a tenant run meters on a
//! connection scoped to its tenant and job (usage attributed to the tenant, project = tenant id); a platform run on a
//! connection scoped to the platform-owned tenant (`AGENT_PLATFORM_TENANT_ID`) with no job (project null).
use serde::Serialize;
use sqlx::PgConnection;
use uuid::Uuid;

use super::llm::Draft;
use crate::error::ApiResult;
use crate::journey::model::GeneratorStepKey;
use crate::usage::{self, MeteringConfig, UsageEvent};

/// The `usage_event.source` of every generator call.
pub const SOURCE: &str = "generator";

/// The run a draft is metered to.
#[derive(Clone, Copy, Debug)]
pub struct RunRef {
    pub id: i64,
    /// The tenant run's job (D97); `None` for a platform run.
    pub job_id: Option<Uuid>,
}

/// Whose usage a run is (`GeneratorUsage.attributed_to` / `.project`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Attribution {
    pub attributed_to: &'static str,
    pub project: Option<String>,
}

/// A tenant run is the tenant's usage, project = the tenant id; a platform run is the platform's own, no project.
pub fn attribution(scope: &str, tenant: Uuid) -> Attribution {
    if scope == "tenant" {
        Attribution {
            attributed_to: "tenant",
            project: Some(tenant.to_string()),
        }
    } else {
        Attribution {
            attributed_to: "platform",
            project: None,
        }
    }
}

/// Meter every call of `draft` for `run`'s `step` on `conn` (see the module doc for its scope), in the caller's
/// transaction so the items, the usage rows and the cost lines commit together. Returns the step's tokens.
pub async fn meter(
    conn: &mut PgConnection,
    metering: &MeteringConfig,
    run: RunRef,
    step: GeneratorStepKey,
    actor: &str,
    draft: &Draft,
) -> ApiResult<i64> {
    let mut total = 0;
    for m in &draft.calls {
        let key = usage::emit(
            &mut *conn,
            metering,
            &UsageEvent {
                job_id: run.job_id,
                source: SOURCE,
                use_: m.call.use_label(),
                activity: None,
                conversation_id: None,
                case_id: None,
                actor,
                usage: &m.usage,
            },
        )
        .await?;
        let tokens = m.usage.tokens_in.max(0) + m.usage.tokens_out.max(0);
        sqlx::query(
            "INSERT INTO generator_cost_line (run_id, step, provider_id, model, tokens, funding, usage_event_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(run.id)
        .bind(step.as_str())
        .bind(&m.usage.provider_id)
        .bind(&m.usage.model)
        .bind(tokens)
        .bind(&m.usage.funding)
        .bind(key)
        .execute(&mut *conn)
        .await?;
        total += tokens;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribution_follows_the_scope() {
        let t = Uuid::nil();
        assert_eq!(attribution("tenant", t).attributed_to, "tenant");
        assert_eq!(attribution("tenant", t).project, Some(t.to_string()));
        assert_eq!(
            attribution("platform", t),
            Attribution {
                attributed_to: "platform",
                project: None
            }
        );
    }
}
