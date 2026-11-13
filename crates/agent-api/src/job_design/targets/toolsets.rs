//! J8 target `narrowing` (`narrow:<role>:<activity>:<op>`): an operation removed from a role's toolset for one
//! activity (`toolset_narrowing`). The value is `true`; absent = in the toolset. `proj` = `!!v` (fixture). And J8
//! target `role_setting` (`role-setting:<role>`, T11a): a job's switch and tool budget of one agent role.
use futures_util::future::BoxFuture;
use serde_json::Value;
use sqlx::PgConnection;

use super::{SET_BY, decode, field, job_of, place_ref, present};
use crate::design::{DesignTarget, DraftRef, Place, Scope, Snap};
use crate::error::ApiResult;

/// `(enabled, max_tool_calls, turn_budget_s, system_prompt, case_per_thread)` of a `job_agent_role_setting` row.
type RoleSettingRow = (bool, Option<i16>, Option<i32>, Option<String>, bool);

/// One toolset narrowing.
pub struct Narrowing;

impl DesignTarget for Narrowing {
    fn kind(&self) -> &'static str {
        "narrowing"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!(
            "narrow:{}:{}:{}",
            field(r, "role"),
            field(r, "activity"),
            field(r, "op")
        )
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let found: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM toolset_narrowing \
                                 WHERE job_id = $1 AND role = $2 AND activity = $3 AND op = $4)",
            )
            .bind(job_of(at)?)
            .bind(field(r, "role"))
            .bind(field(r, "activity"))
            .bind(field(r, "op"))
            .fetch_one(&mut *c)
            .await?;
            Ok(found.then_some(Value::Bool(true)))
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
            let (role, activity, op) = (field(r, "role"), field(r, "activity"), field(r, "op"));
            if present(v).is_some() {
                sqlx::query(
                    "INSERT INTO toolset_narrowing (job_id, role, activity, op, updated_by) \
                     VALUES ($1, $2, $3, $4, $5) \
                     ON CONFLICT (tenant_id, job_id, role, activity, op) DO NOTHING",
                )
                .bind(job)
                .bind(role)
                .bind(activity)
                .bind(op)
                .bind(SET_BY)
                .execute(&mut *c)
                .await?;
            } else {
                sqlx::query(
                    "DELETE FROM toolset_narrowing \
                      WHERE job_id = $1 AND role = $2 AND activity = $3 AND op = $4",
                )
                .bind(job)
                .bind(role)
                .bind(activity)
                .bind(op)
                .execute(&mut *c)
                .await?;
            }
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let rows: Vec<(String, String, String)> = sqlx::query_as(
                "SELECT role, activity, op FROM toolset_narrowing WHERE job_id = $1 \
                  ORDER BY role, activity, op",
            )
            .bind(job_of(at)?)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                rows.iter()
                    .map(|(role, activity, op)| {
                        place_ref(&[
                            ("role", role.as_str()),
                            ("activity", activity.as_str()),
                            ("op", op.as_str()),
                        ])
                    })
                    .collect(),
            ))
        })
    }
    /// Fixture `!!s`: a narrowing is there or not.
    fn proj(&self, v: &Snap) -> Value {
        Value::Bool(v.is_some())
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        Some(
            if v.is_some() {
                "removed from the toolset"
            } else {
                "in the toolset"
            }
            .to_string(),
        )
    }
}

/// J8 target `role_setting` (`role-setting:<role>`, T11a, F-40): a job's setting of one agent role
/// (`job_agent_role_setting`). The value is `{enabled, max_tool_calls, turn_budget_s?, system_prompt?,
/// case_per_thread?}` (the A5 fields only when set; no row = absent = the platform default:
/// off); `proj` = `[enabled, max_tool_calls]` (fixture), plus `[turn_budget_s, system_prompt, case_per_thread]` when one is set.
pub struct RoleSettingTarget;

impl DesignTarget for RoleSettingTarget {
    fn kind(&self) -> &'static str {
        "role_setting"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("role-setting:{}", field(r, "role"))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let row: Option<RoleSettingRow> = sqlx::query_as(
                "SELECT enabled, max_tool_calls, turn_budget_s, system_prompt, case_per_thread \
                   FROM job_agent_role_setting WHERE job_id = $1 AND role = $2",
            )
            .bind(job_of(at)?)
            .bind(field(r, "role"))
            .fetch_optional(&mut *c)
            .await?;
            Ok(row.map(|(enabled, max, turn, prompt, per_thread)| {
                let mut v = serde_json::json!({"enabled": enabled, "max_tool_calls": max});
                // the A5 fields only when set: a setting without them keeps its pre-A5 shape
                if let Some(n) = turn {
                    v["turn_budget_s"] = n.into();
                }
                if let Some(p) = prompt {
                    v["system_prompt"] = p.into();
                }
                if per_thread {
                    v["case_per_thread"] = true.into();
                }
                v
            }))
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
            let role = field(r, "role");
            match present(v) {
                Some(v) => {
                    let s: crate::job_design::toolset::RoleSetting = decode("role_setting", v)?;
                    let max = s.max_tool_calls.and_then(|n| i16::try_from(n).ok());
                    let turn = s.turn_budget_s.and_then(|n| i32::try_from(n).ok());
                    sqlx::query(
                        "INSERT INTO job_agent_role_setting \
                                (job_id, role, enabled, max_tool_calls, turn_budget_s, system_prompt, \
                                 case_per_thread, updated_by) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                         ON CONFLICT (tenant_id, job_id, role) DO UPDATE \
                            SET enabled = EXCLUDED.enabled, max_tool_calls = EXCLUDED.max_tool_calls, \
                                turn_budget_s = EXCLUDED.turn_budget_s, system_prompt = EXCLUDED.system_prompt, \
                                case_per_thread = EXCLUDED.case_per_thread, \
                                updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
                    )
                    .bind(job)
                    .bind(role)
                    .bind(s.enabled)
                    .bind(max)
                    .bind(turn)
                    .bind(s.system_prompt.as_deref())
                    .bind(s.case_per_thread)
                    .bind(SET_BY)
                    .execute(&mut *c)
                    .await?;
                }
                None => {
                    sqlx::query(
                        "DELETE FROM job_agent_role_setting WHERE job_id = $1 AND role = $2",
                    )
                    .bind(job)
                    .bind(role)
                    .execute(&mut *c)
                    .await?;
                }
            }
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let roles: Vec<String> = sqlx::query_scalar(
                "SELECT role FROM job_agent_role_setting WHERE job_id = $1 ORDER BY role",
            )
            .bind(job_of(at)?)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                roles
                    .iter()
                    .map(|role| place_ref(&[("role", role.as_str())]))
                    .collect(),
            ))
        })
    }
    /// Fixture: `r && [r.enabled, r.max_tool_calls ?? null]`.
    fn proj(&self, v: &Snap) -> Value {
        match present(v) {
            Some(r) => {
                let mut a = vec![
                    r.get("enabled").cloned().unwrap_or(Value::Null),
                    r.get("max_tool_calls").cloned().unwrap_or(Value::Null),
                ];
                // A5: three more places, only when one is set, so a pre-A5 setting projects as before
                let k = ["turn_budget_s", "system_prompt", "case_per_thread"];
                if k.iter().any(|k| r.get(*k).is_some_and(|v| !v.is_null())) {
                    a.extend(k.iter().map(|k| r.get(*k).cloned().unwrap_or(Value::Null)));
                }
                Value::Array(a)
            }
            None => Value::Null,
        }
    }
    /// Fixture: `platform default` | `on` / `off` (+ ` · up to N tool calls`).
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let Some(r) = present(v) else {
            return Some("platform default".to_string());
        };
        let on = r.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        let calls = match r.get("max_tool_calls").and_then(Value::as_u64) {
            Some(n) if n > 0 => format!(" · up to {n} tool calls"),
            _ => String::new(),
        };
        let turn = match r.get("turn_budget_s").and_then(Value::as_u64) {
            Some(n) => format!(" · {n}s per turn"),
            None => String::new(),
        };
        let prompt = if r.get("system_prompt").is_some_and(|v| v.is_string()) {
            " · own prompt"
        } else {
            ""
        };
        let per_thread = if r.get("case_per_thread").and_then(Value::as_bool) == Some(true) {
            " · one case per thread"
        } else {
            ""
        };
        Some(format!(
            "{}{calls}{turn}{prompt}{per_thread}",
            if on { "on" } else { "off" }
        ))
    }
}
