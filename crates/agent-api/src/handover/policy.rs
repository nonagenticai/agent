//! The handover SLA policy (D64, B4 T16): B2 ledger kind `handover_policy`, one place per job (ref `{}`, key
//! `handover_policy`), working row `handover_policy` (0069). One line in `design::registry()` ([`register`]).
//!
//! | kind | key | working rows | places |
//! |---|---|---|---|
//! | `handover_policy` | `handover_policy` | `handover_policy` | `{}` when the job has a row |
//!
//! The value is `{targets, hours}`: `targets` = the web's `HandoverSlaTargets`, `hours` = the F-14 business hours
//! (T16a, crate::handover::hours; `null` = always staffed). No value = no SLA. `saveHandoverPolicy` writes the row
//! only inside the `mutate` closure of `design::track` through [`write_targets`] (tests/design_antidrift.rs: this
//! module is the kind's only other writer), so a save is a draft and never live (D54). The runtime reads it through [`read`]: the live policy
//! for a prod item, the draft for a test item (plan R8).
use std::sync::{Arc, OnceLock};

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::calendar::{self, Calendar};
use super::hours::{self, Hours};
use crate::design::{self, DesignOf, DesignTarget, DraftRef, JobRef, Place, Registry, Scope, Snap};
use crate::error::{ApiError, ApiResult};
use crate::release::targets::SET_BY;

/// The ledger kind.
pub const KIND: &str = "handover_policy";
/// The place key (fixture `TARGETS.handover_policy.key`).
pub const KEY: &str = "handover_policy";
/// The priorities a policy names, in the web's order (`J21Handover.ts` `PRIORITIES`).
pub const PRIORITIES: [&str; 3] = ["high", "normal", "low"];
/// The longest target, in minutes (30 days; `policyError`).
pub const MAX_MINUTES: i64 = 43_200;

/// Register the `handover_policy` target (one line in `design::registry()`).
pub fn register(r: &mut Registry) {
    r.register(Arc::new(HandoverPolicyTarget));
}

/// A registry holding only this kind and `tenant_calendar`, for the reads of [`read`] and [`hours_of`].
/// `design::read` needs the kinds' targets and a snapshot cache, nothing else;
/// [`crate::conversations::handover::create`] and the runner's `agent_cases` have no `AppState` to borrow
/// `st.design` from.
fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| {
        let mut r = Registry::default();
        register(&mut r);
        calendar::register(&mut r);
        r
    })
}

/// One priority's targets, in minutes from entering the queue (web `HandoverSlaTarget`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlaTarget {
    pub first_response_min: i64,
    pub resolve_min: i64,
}

/// The targets of every priority (web `HandoverSlaTargets`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Targets {
    pub high: SlaTarget,
    pub normal: SlaTarget,
    pub low: SlaTarget,
}

impl Targets {
    /// The targets of `priority` (`urgent` / `high` / `normal` / `low`); an unknown priority has none. `urgent` has
    /// no setting of its own: it is half of `high` (rounded up, at least a minute), so it is always the shortest.
    pub fn of(&self, priority: &str) -> Option<SlaTarget> {
        let half = |m: i64| ((m + 1) / 2).max(1);
        match priority {
            "urgent" => Some(SlaTarget {
                first_response_min: half(self.high.first_response_min),
                resolve_min: half(self.high.resolve_min),
            }),
            "high" => Some(self.high),
            "normal" => Some(self.normal),
            "low" => Some(self.low),
            _ => None,
        }
    }

    pub fn to_value(self) -> Value {
        json!({
            "high": target_value(self.high),
            "normal": target_value(self.normal),
            "low": target_value(self.low),
        })
    }
}

fn target_value(t: SlaTarget) -> Value {
    json!({"first_response_min": t.first_response_min, "resolve_min": t.resolve_min})
}

/// `J21Handover.ts` `policyError`: why `targets` cannot be saved (`Err(message)`), else the targets with only the
/// three priorities and their two minute fields. Minutes are whole, 1 to 43200; first response ≤ resolve.
pub fn validate(targets: &Value) -> Result<Targets, String> {
    let mut out = [SlaTarget {
        first_response_min: 0,
        resolve_min: 0,
    }; 3];
    for (i, p) in PRIORITIES.into_iter().enumerate() {
        let Some(x) = targets.get(p).filter(|x| x.is_object()) else {
            return Err(format!("{p}: missing"));
        };
        let mut mins = [0_i64; 2];
        for (k, field) in ["first_response_min", "resolve_min"]
            .into_iter()
            .enumerate()
        {
            match x.get(field).and_then(Value::as_i64) {
                Some(v) if (1..=MAX_MINUTES).contains(&v) => mins[k] = v,
                _ => return Err(format!("{p}: minutes must be whole, 1 to {MAX_MINUTES}")),
            }
        }
        if mins[0] > mins[1] {
            return Err(format!("{p}: first response must come before resolve"));
        }
        out[i] = SlaTarget {
            first_response_min: mins[0],
            resolve_min: mins[1],
        };
    }
    Ok(Targets {
        high: out[0],
        normal: out[1],
        low: out[2],
    })
}

/// The shared human duration of `$lib/format` `duration` (whole minutes).
pub fn duration(minutes: i64) -> String {
    let m = minutes.abs();
    if m < 1 {
        return "<1 min".to_string();
    }
    if m < 60 {
        return format!("{m} min");
    }
    if m < 24 * 60 {
        let (h, r) = (m / 60, m % 60);
        return if r > 0 {
            format!("{h} h {r} min")
        } else {
            format!("{h} h")
        };
    }
    let (d, h) = (m / 1440, (m % 1440) / 60);
    if h > 0 {
        format!("{d} d {h} h")
    } else {
        format!("{d} d")
    }
}

/// `J21Handover.ts` `policyText`: `high 15 min / 4 h · normal … · low …`, or `no SLA`.
pub fn policy_text(t: Option<&Targets>) -> String {
    let Some(t) = t else {
        return "no SLA".to_string();
    };
    PRIORITIES
        .into_iter()
        .filter_map(|p| t.of(p).map(|x| (p, x)))
        .map(|(p, x)| {
            format!(
                "{p} {} / {}",
                duration(x.first_response_min),
                duration(x.resolve_min)
            )
        })
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

/// The targets of a policy value (`{targets, hours}`); `None` = no value, or a value without valid targets.
pub fn targets_of(v: &Snap) -> Option<Targets> {
    let t = v.as_ref()?.get("targets")?;
    validate(t).ok()
}

/// The policy value of a working row.
fn value_of(targets: Value, hours: Option<Value>) -> Value {
    json!({"targets": targets, "hours": hours.unwrap_or(Value::Null)})
}

/// Write the job's working targets, and its hours when `hours` is `Some` (`Some(None)` = always staffed; `None` =
/// keep the stored hours). Only inside a `track` closure or `set`.
pub async fn write_targets(
    c: &mut PgConnection,
    tenant: Uuid,
    job: Uuid,
    targets: &Targets,
    hours: Option<Option<&Hours>>,
    by: &str,
) -> ApiResult<()> {
    if let Some(h) = hours {
        sqlx::query(
            "INSERT INTO handover_policy (tenant_id, job_id, targets, hours, updated_by) VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (tenant_id, job_id) DO UPDATE \
                SET targets = EXCLUDED.targets, hours = EXCLUDED.hours, updated_by = EXCLUDED.updated_by, \
                    updated_at = clock_timestamp()",
        )
        .bind(tenant)
        .bind(job)
        .bind(targets.to_value())
        .bind(h.map(Hours::to_value))
        .bind(by.to_owned())
        .execute(&mut *c)
        .await?;
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO handover_policy (tenant_id, job_id, targets, updated_by) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (tenant_id, job_id) DO UPDATE \
            SET targets = EXCLUDED.targets, updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
    )
    .bind(tenant)
    .bind(job)
    .bind(targets.to_value())
    .bind(by)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Replace the whole working row with `value` (`{targets, hours}`). Only inside `set`.
async fn write_value(
    c: &mut PgConnection,
    tenant: Uuid,
    job: Uuid,
    value: &Value,
) -> ApiResult<()> {
    let targets = match value.get("targets") {
        Some(t @ Value::Object(_)) => t.clone(),
        other => {
            return Err(internal(format!(
                "handover_policy targets is not an object: {other:?}"
            )));
        }
    };
    let hours = match value.get("hours") {
        None | Some(Value::Null) => None,
        Some(h @ Value::Object(_)) => Some(h.clone()),
        Some(other) => {
            return Err(internal(format!(
                "handover_policy hours is not an object: {other}"
            )));
        }
    };
    sqlx::query(
        "INSERT INTO handover_policy (tenant_id, job_id, targets, hours, updated_by) VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (tenant_id, job_id) DO UPDATE \
            SET targets = EXCLUDED.targets, hours = EXCLUDED.hours, updated_by = EXCLUDED.updated_by, \
                updated_at = clock_timestamp()",
    )
    .bind(tenant)
    .bind(job)
    .bind(targets)
    .bind(hours)
    .bind(SET_BY)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Remove the working row (no SLA). Only inside `set`.
async fn delete_row(c: &mut PgConnection, tenant: Uuid, job: Uuid) -> ApiResult<()> {
    sqlx::query("DELETE FROM handover_policy WHERE tenant_id = $1 AND job_id = $2")
        .bind(tenant)
        .bind(job)
        .execute(&mut *c)
        .await?;
    Ok(())
}

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

/// The job's policy value in the design `of`, through `design::read`.
async fn value(c: &mut PgConnection, job: &JobRef, of: &DesignOf) -> ApiResult<Snap> {
    design::read::value(c, registry(), job, of, KIND, &DraftRef::new()).await
}

/// The `hours` of a policy value (`None` = none, always staffed).
fn hours_in(v: &Snap) -> Option<Hours> {
    hours::of_value(v.as_ref().and_then(|x| x.get("hours")))
}

/// The job's policy in the design `of` (`Live` for prod, `Draft` for test), through `design::read`; `None` = no SLA.
pub async fn read(c: &mut PgConnection, job: &JobRef, of: &DesignOf) -> ApiResult<Option<Targets>> {
    Ok(targets_of(&value(c, job, of).await?))
}

/// The job's business hours in the design `of` (`None` = always staffed) and, when they use it, the tenant calendar
/// of the same design (T16a; plan R8: live for a prod item, the draft for a test item).
pub async fn hours_of(
    c: &mut PgConnection,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<(Option<Hours>, Option<Calendar>)> {
    let hours = hours_in(&value(&mut *c, job, of).await?);
    let cal = match &hours {
        Some(h) if h.use_tenant_calendar => calendar::read(&mut *c, registry(), job, of).await?,
        _ => None,
    };
    Ok((hours, cal))
}

/// `handoverPolicy` (web `HandoverPolicyView`): the live targets, the draft targets, whether the draft ledger has a
/// row at the place, and the live and draft business hours (T16a; `null` = always staffed).
pub async fn view(c: &mut PgConnection, job: &JobRef) -> ApiResult<Value> {
    let live = value(&mut *c, job, &DesignOf::Live).await?;
    let draft = value(&mut *c, job, &DesignOf::Draft).await?;
    let drafted = design::ledger::find(&mut *c, job, KEY).await?.is_some();
    Ok(json!({
        "job": job.key,
        "live": targets_of(&live).map(Targets::to_value),
        "draft": targets_of(&draft).map(Targets::to_value),
        "drafted": drafted,
        "hours": {
            "live": hours_in(&live).as_ref().map(Hours::to_value),
            "draft": hours_in(&draft).as_ref().map(Hours::to_value),
        },
    }))
}

/// `handover_policy` (D64): the job's SLA policy. Job-scoped: each job's Production gets a new policy only when that
/// job publishes it.
pub struct HandoverPolicyTarget;

impl DesignTarget for HandoverPolicyTarget {
    fn kind(&self) -> &'static str {
        KIND
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, _r: &DraftRef) -> String {
        KEY.to_string()
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        _r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let Some(job) = at.job else {
                return Ok(None);
            };
            let row: Option<(Value, Option<Value>)> = sqlx::query_as(
                "SELECT targets, hours FROM handover_policy WHERE tenant_id = $1 AND job_id = $2",
            )
            .bind(at.tenant)
            .bind(job.id)
            .fetch_optional(&mut *c)
            .await?;
            Ok(row.map(|(t, h)| value_of(t, h)))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        _r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = at
                .job
                .ok_or_else(|| internal("handover_policy is written per job".to_string()))?;
            match v {
                None => delete_row(c, at.tenant, job.id).await,
                Some(value) => write_value(c, at.tenant, job.id, value).await,
            }
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let Some(job) = at.job else {
                return Ok(Some(Vec::new()));
            };
            let n: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM handover_policy WHERE tenant_id = $1 AND job_id = $2",
            )
            .bind(at.tenant)
            .bind(job.id)
            .fetch_one(&mut *c)
            .await?;
            Ok(Some(if n > 0 {
                vec![DraftRef::new()]
            } else {
                Vec::new()
            }))
        })
    }
    /// Fixture `text`: `policyText(targets)`, or `no SLA`; with business hours, ` · hours <timezone>` (T16a).
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let t = policy_text(targets_of(v).as_ref());
        Some(match hours_in(v) {
            Some(h) => format!("{t} \u{b7} hours {}", h.timezone),
            None => t,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(fr: i64, res: i64) -> Value {
        json!({"first_response_min": fr, "resolve_min": res})
    }

    #[test]
    fn urgent_is_half_of_high() {
        let v = validate(&json!({"high": t(15, 241), "normal": t(60, 1440), "low": t(240, 4320)}))
            .unwrap();
        assert_eq!(
            v.of("urgent"),
            Some(SlaTarget {
                first_response_min: 8,
                resolve_min: 121
            })
        );
        let tiny = validate(&json!({"high": t(1, 1), "normal": t(60, 1440), "low": t(240, 4320)}))
            .unwrap();
        assert_eq!(tiny.of("urgent").unwrap().first_response_min, 1);
        assert_eq!(v.of("normal"), Some(v.normal));
        assert_eq!(v.of("bogus"), None);
    }

    #[test]
    fn validate_follows_policy_error() {
        let ok = json!({"high": t(15, 240), "normal": t(60, 1440), "low": t(240, 4320)});
        let v = validate(&ok).unwrap();
        assert_eq!(v.normal.first_response_min, 60);
        assert_eq!(v.to_value(), ok);
        assert_eq!(
            validate(&json!({"high": t(15, 240), "normal": t(60, 1440)})),
            Err("low: missing".into())
        );
        assert_eq!(
            validate(&json!({"high": t(0, 240), "normal": t(60, 1440), "low": t(1, 2)})),
            Err("high: minutes must be whole, 1 to 43200".into())
        );
        assert_eq!(
            validate(&json!({"high": t(1, 43_201), "normal": t(60, 1440), "low": t(1, 2)})),
            Err("high: minutes must be whole, 1 to 43200".into())
        );
        assert_eq!(
            validate(
                &json!({"high": {"first_response_min": 1.5, "resolve_min": 2}, "normal": t(1, 2), "low": t(1, 2)})
            ),
            Err("high: minutes must be whole, 1 to 43200".into())
        );
        assert_eq!(
            validate(&json!({"high": t(1, 2), "normal": t(90, 60), "low": t(1, 2)})),
            Err("normal: first response must come before resolve".into())
        );
    }

    #[test]
    fn text_is_policy_text() {
        let v = validate(&json!({"high": t(15, 240), "normal": t(60, 1500), "low": t(240, 4380)}))
            .unwrap();
        assert_eq!(
            policy_text(Some(&v)),
            "high 15 min / 4 h \u{b7} normal 1 h / 1 d 1 h \u{b7} low 4 h / 3 d 1 h"
        );
        assert_eq!(policy_text(None), "no SLA");
        assert_eq!(duration(90), "1 h 30 min");
        assert_eq!(duration(2880), "2 d");
    }
}
