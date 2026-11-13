//! A2/A3 design targets: `tool_policy` (`tool-policy:<op>`, working rows `job_tool_policy`) and `iam_attachment`
//! (`iam-attachment:<procedure id>`, working rows `job_iam_attachment`). Both are job-scoped and released with the
//! design: the snapshot stores every place, `runtime::declared` reads the values back.
//!
//! | kind | key | value | places |
//! |---|---|---|---|
//! | `tool_policy` | `tool-policy:<op key>` | a [`ToolPolicy`] | every row of the job |
//! | `iam_attachment` | `iam-attachment:<id>` | `{}` | every attached procedure |
//!
//! `set` is the restore path (discard, follow-live, rollback) and decodes the value into its type, so a stored
//! policy is always well-formed; the op/procedure checks against the catalogue and the tenant's procedures belong to
//! the routes (`iam::routes`), which see the live catalogue. An attachment whose procedure is gone (deleted since a
//! snapshot) cannot be restored: the FK refuses it and `set` answers 409 `procedure_not_found`.
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{SET_BY, decode, field, job_of, place_ref, present};
use crate::design::{DesignTarget, DraftRef, Place, Scope, Snap};
use crate::error::{ApiError, ApiResult};
use crate::runtime::declared::{Confirmation, OnFailure, ToolPolicy};

/// Ledger ref of a policy place.
pub fn policy_ref(op: &str) -> DraftRef {
    place_ref(&[("op", op)])
}

/// Ledger ref of an attachment place.
pub fn attachment_ref(procedure: Uuid) -> DraftRef {
    place_ref(&[("procedure", procedure.to_string().as_str())])
}

/// Write (insert or replace) the working row of one policy.
pub async fn write_policy(
    c: &mut PgConnection,
    job: Uuid,
    op: &str,
    p: &ToolPolicy,
    by: &str,
) -> ApiResult<()> {
    let stored =
        serde_json::to_value(p).map_err(|e| super::internal(format!("tool_policy encode: {e}")))?;
    sqlx::query(
        "INSERT INTO job_tool_policy (job_id, op, policy, updated_by) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (tenant_id, job_id, op) DO UPDATE \
            SET policy = EXCLUDED.policy, updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
    )
    .bind(job)
    .bind(op)
    .bind(stored)
    .bind(by)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Delete the working row of one policy; whether there was one.
pub async fn delete_policy(c: &mut PgConnection, job: Uuid, op: &str) -> ApiResult<bool> {
    let r = sqlx::query("DELETE FROM job_tool_policy WHERE job_id = $1 AND op = $2")
        .bind(job)
        .bind(op)
        .execute(&mut *c)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// Attach a procedure to a job (working row); idempotent.
pub async fn write_attachment(
    c: &mut PgConnection,
    job: Uuid,
    procedure: Uuid,
    by: &str,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO job_iam_attachment (job_id, procedure_id, updated_by) VALUES ($1, $2, $3) \
         ON CONFLICT (tenant_id, job_id, procedure_id) DO UPDATE \
            SET updated_by = EXCLUDED.updated_by, updated_at = clock_timestamp()",
    )
    .bind(job)
    .bind(procedure)
    .bind(by)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Detach a procedure from a job; whether it was attached.
pub async fn delete_attachment(
    c: &mut PgConnection,
    job: Uuid,
    procedure: Uuid,
) -> ApiResult<bool> {
    let r = sqlx::query("DELETE FROM job_iam_attachment WHERE job_id = $1 AND procedure_id = $2")
        .bind(job)
        .bind(procedure)
        .execute(&mut *c)
        .await?;
    Ok(r.rows_affected() > 0)
}

/// The per-op declaration of a job.
pub struct ToolPolicyTarget;

impl DesignTarget for ToolPolicyTarget {
    fn kind(&self) -> &'static str {
        "tool_policy"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("tool-policy:{}", field(r, "op"))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let row: Option<Value> = sqlx::query_scalar(
                "SELECT policy FROM job_tool_policy WHERE job_id = $1 AND op = $2",
            )
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
                    let p: ToolPolicy = decode("tool_policy", v)?;
                    write_policy(&mut *c, job, op, &p, SET_BY).await?;
                }
                None => {
                    delete_policy(&mut *c, job, op).await?;
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
                sqlx::query_scalar("SELECT op FROM job_tool_policy WHERE job_id = $1 ORDER BY op")
                    .bind(job_of(at)?)
                    .fetch_all(&mut *c)
                    .await?;
            Ok(Some(ops.iter().map(|o| policy_ref(o)).collect()))
        })
    }
    /// The whole policy is the compared value.
    fn proj(&self, v: &Snap) -> Value {
        present(v).cloned().unwrap_or(Value::Null)
    }
    /// `no declaration` | `N state inputs · M outputs · requires a, b · auto`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let Some(p) = present(v).and_then(|v| serde_json::from_value::<ToolPolicy>(v.clone()).ok())
        else {
            return Some("no declaration".to_string());
        };
        let state = p
            .inputs
            .values()
            .filter(|i| !matches!(i, crate::runtime::declared::InputDecl::Model))
            .count();
        let mut s = format!("{state} state inputs · {} outputs", p.outputs.len());
        if !p.requires.is_empty() {
            s.push_str(&format!(" · requires {}", p.requires.join(", ")));
        }
        if p.confirmation == Confirmation::Auto {
            s.push_str(" · auto-confirmed");
        }
        if p.on_failure == OnFailure::Refuse {
            s.push_str(" · refuses on failure");
        }
        Some(s)
    }
}

/// The IAM procedures a job has attached.
pub struct IamAttachmentTarget;

fn procedure_of(r: &DraftRef) -> ApiResult<Uuid> {
    Uuid::parse_str(field(r, "procedure"))
        .map_err(|_| super::internal("iam_attachment ref names no procedure id".into()))
}

impl DesignTarget for IamAttachmentTarget {
    fn kind(&self) -> &'static str {
        "iam_attachment"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("iam-attachment:{}", field(r, "procedure"))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let found: Option<i32> = sqlx::query_scalar(
                "SELECT 1 FROM job_iam_attachment WHERE job_id = $1 AND procedure_id = $2",
            )
            .bind(job_of(at)?)
            .bind(procedure_of(r)?)
            .fetch_optional(&mut *c)
            .await?;
            Ok(found.map(|_| json!({})))
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
            let procedure = procedure_of(r)?;
            if present(v).is_some() {
                let exists: Option<i32> =
                    sqlx::query_scalar("SELECT 1 FROM iam_procedure WHERE id = $1")
                        .bind(procedure)
                        .fetch_optional(&mut *c)
                        .await?;
                if exists.is_none() {
                    return Err(ApiError::Conflict(
                        "procedure_not_found",
                        "That identity procedure no longer exists; it cannot be attached again."
                            .into(),
                    ));
                }
                write_attachment(&mut *c, job, procedure, SET_BY).await?;
            } else {
                delete_attachment(&mut *c, job, procedure).await?;
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
            let ids: Vec<Uuid> = sqlx::query_scalar(
                "SELECT procedure_id FROM job_iam_attachment WHERE job_id = $1 ORDER BY procedure_id",
            )
            .bind(job_of(at)?)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(ids.into_iter().map(attachment_ref).collect()))
        })
    }
    fn proj(&self, v: &Snap) -> Value {
        Value::Bool(present(v).is_some())
    }
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        Some(
            if present(v).is_some() {
                "identity procedure attached"
            } else {
                "identity procedure not attached"
            }
            .to_string(),
        )
    }
}
