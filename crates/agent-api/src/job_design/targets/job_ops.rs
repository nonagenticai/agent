//! Design kind `job_operation` (`job-op:<op key>`, working rows `job_operation`, 0246): an operation the job adds from
//! one of its tenant's connector templates (owner 2026-10-31: the platform is general purpose; a tenant brings its own
//! tools to its own jobs, never through the platform catalogue). The value is a [`JobOp`]: the identity level the
//! tenant chose and a copy of the op's definition taken when it was added, so a later template edit never changes a
//! released job.
//!
//! A job op is part of the job like a catalogue op: it is expected (routable, listed, tool policy, readiness needs its
//! prod route: `routes_store::expected`), the assistant role may run it up to its max side effect
//! (`toolset::Toolsets`), and the runtime catalogue holds it (`runtime::design`). A catalogue op of the job with the
//! same key wins: a job op never shadows the job's template.
use std::collections::BTreeMap;

use agent_model::catalog::{OperationRow, STAR};
use agent_model::kinds::catalog::IdentityLevel;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{SET_BY, decode, field, job_of, place_ref, present};
use crate::connectors::model::{InputType, SideEffect};
use crate::connectors::resolve::ExpectedOperation;
use crate::design::{
    DesignOf, DesignTarget, DraftRef, JobRef, Place, Registry, Scope, Snap, values_of_kind,
};
use crate::error::ApiResult;

pub const KIND: &str = "job_operation";
/// The key prefix of a [`KIND`] place.
pub const KEY_PREFIX: &str = "job-op:";

/// One operation a job owns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobOp {
    /// The level a caller needs before the agent runs it (`0` public … `S` staff), chosen by the tenant.
    pub identity_level: IdentityLevel,
    pub side_effect: SideEffect,
    #[serde(default)]
    pub input_fields: Vec<String>,
    #[serde(default)]
    pub output_fields: Vec<String>,
    #[serde(default)]
    pub description: String,
    /// The template the definition was copied from (`key@version`).
    pub template: String,
    /// The inputs' JSON-schema type and description, as the template declares them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_types: BTreeMap<String, InputType>,
}

impl JobOp {
    fn modality(&self) -> &'static str {
        if self.side_effect == SideEffect::R {
            "observe"
        } else {
            "do"
        }
    }

    fn level(&self) -> Value {
        serde_json::to_value(self.identity_level).unwrap_or(Value::Null)
    }

    /// The op as a compiled catalogue row (what the runtime catalogue, the toolsets and the J6 view read).
    pub fn row(&self, op: &str) -> OperationRow {
        let r = crate::connectors::opkey::parse_op_key(op);
        let mut attrs = Map::new();
        attrs.insert("modality".into(), json!(self.modality()));
        attrs.insert("is_derive".into(), json!(false));
        attrs.insert("identity_level".into(), self.level());
        attrs.insert("side_effect".into(), json!(self.side_effect));
        attrs.insert("input_fields".into(), json!(self.input_fields));
        attrs.insert("output_fields".into(), json!(self.output_fields));
        attrs.insert("confirm_back".into(), json!(false));
        attrs.insert("builtin".into(), json!(false));
        OperationRow {
            attrs,
            key: op.to_owned(),
            object: r.object,
            verb: r.verb,
            variant: if r.variant.is_empty() {
                STAR.to_owned()
            } else {
                r.variant
            },
            discriminator: None,
            overridden: Vec::new(),
            inherited: Vec::new(),
            description: self.description.clone(),
            retail: None,
            stages: Vec::new(),
            error_states: Vec::new(),
        }
    }

    /// The op as an expected operation: required (its prod route gates the release).
    pub fn expected(&self, op: &str) -> ExpectedOperation {
        let r = crate::connectors::opkey::parse_op_key(op);
        ExpectedOperation {
            object: r.object,
            verb: r.verb,
            variant: r.variant,
            modality: self.modality().to_owned(),
            is_derive: false,
            side_effect: format!("{:?}", self.side_effect),
            identity_level: crate::runtime::catalog::level_wire(self.identity_level).to_owned(),
            stages: Vec::new(),
            required: true,
            note: format!("Added to the job from {}", self.template),
        }
    }
}

/// Ledger ref of a job op place.
pub fn op_ref(op: &str) -> DraftRef {
    place_ref(&[("op", op)])
}

/// Write (insert or replace) the working row of one job op.
pub async fn write(
    c: &mut PgConnection,
    job: Uuid,
    op: &str,
    v: &JobOp,
    by: &str,
) -> ApiResult<()> {
    let stored = serde_json::to_value(v)
        .map_err(|e| super::internal(format!("job_operation encode: {e}")))?;
    sqlx::query(
        "INSERT INTO job_operation (job_id, op, value, updated_by) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (tenant_id, job_id, op) DO UPDATE \
            SET value = EXCLUDED.value, updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
    )
    .bind(job)
    .bind(op)
    .bind(stored)
    .bind(by)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Delete the working row of one job op; whether there was one.
pub async fn delete(c: &mut PgConnection, job: Uuid, op: &str) -> ApiResult<bool> {
    let r = sqlx::query("DELETE FROM job_operation WHERE job_id = $1 AND op = $2")
        .bind(job)
        .bind(op)
        .execute(&mut *c)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// The job's own ops of design `of` (op key → value), every `job_operation` place; a value that does not decode is
/// left out (logged by key).
pub async fn of_design(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<BTreeMap<String, JobOp>> {
    let mut out = BTreeMap::new();
    for r in values_of_kind(&mut *c, reg, job, of, KIND).await? {
        let Some(op) = r.key.strip_prefix(KEY_PREFIX) else {
            continue;
        };
        let Some(v) = r.snap.filter(|v| !v.is_null()) else {
            continue;
        };
        match serde_json::from_value::<JobOp>(v) {
            Ok(j) => {
                out.insert(op.to_owned(), j);
            }
            Err(_) => {
                tracing::warn!(job = %job.key, key = %r.key, "a job operation does not decode")
            }
        }
    }
    Ok(out)
}

/// The job op design target.
pub struct JobOperationTarget;

impl DesignTarget for JobOperationTarget {
    fn kind(&self) -> &'static str {
        KIND
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("{KEY_PREFIX}{}", field(r, "op"))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let row: Option<Value> =
                sqlx::query_scalar("SELECT value FROM job_operation WHERE job_id = $1 AND op = $2")
                    .bind(job_of(at)?)
                    .bind(field(r, "op"))
                    .fetch_optional(&mut *c)
                    .await?;
            Ok(row)
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
            let op = field(r, "op");
            match present(v) {
                Some(v) => {
                    let j: JobOp = decode(KIND, v)?;
                    write(&mut *c, job, op, &j, SET_BY).await?;
                }
                None => {
                    delete(&mut *c, job, op).await?;
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
            let ops: Vec<String> =
                sqlx::query_scalar("SELECT op FROM job_operation WHERE job_id = $1 ORDER BY op")
                    .bind(job_of(at)?)
                    .fetch_all(&mut *c)
                    .await?;
            Ok(Some(ops.iter().map(|o| op_ref(o)).collect()))
        })
    }
    /// The whole value is the compared value.
    fn proj(&self, v: &Snap) -> Value {
        present(v).cloned().unwrap_or(Value::Null)
    }
    /// `not on the job` | `W · level 0 · from fr_mcp@1`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let Some(j) = present(v).and_then(|v| serde_json::from_value::<JobOp>(v.clone()).ok())
        else {
            return Some("not on the job".to_string());
        };
        Some(format!(
            "{:?} · level {} · from {}",
            j.side_effect,
            crate::runtime::catalog::level_wire(j.identity_level),
            j.template
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fr() -> JobOp {
        JobOp {
            identity_level: IdentityLevel::Public,
            side_effect: SideEffect::W,
            input_fields: vec!["title".into(), "description".into()],
            output_fields: vec!["id".into()],
            description: "Starts an fr project.".into(),
            template: "fr_mcp@1".into(),
            input_types: BTreeMap::new(),
        }
    }

    /// A job op is a runtime catalogue op at the tenant's level, with its own inputs and outputs.
    #[test]
    fn a_job_op_is_a_catalogue_row_the_runtime_reads() {
        let row = fr().row("fr.start_project");
        let d = crate::runtime::catalog::OpDef::from_row(&row).expect("a runtime op");
        assert_eq!(d.key, "fr.start_project");
        assert_eq!(d.identity_level, IdentityLevel::Public);
        assert_eq!(d.inputs, ["title", "description"]);
        assert_eq!(d.outputs, ["id"]);
        assert!(d.writes());
        let e = fr().expected("fr.start_project");
        assert!(e.required);
        assert_eq!(
            (e.side_effect.as_str(), e.identity_level.as_str()),
            ("W", "0")
        );
    }
}
