//! J12 target `procedure` (`procedure:<name>`): the job's TENANT versions of one procedure name
//! (`procedure_version`); base versions are the compiled `procedure` rows and never stored here (plan R1). The value
//! is the versions, oldest first: `[{version, status, context_space, description, reviewers, steps, released_in,
//! success_criteria, tool_requirements, pddl_domain}]` (`version` an integer, R6; the last three are T7a's, 0147).
//! A value written before T7a lacks them and reads as no criteria / no requirements / no domain.
//! `proj` = `[version, status, reviewers, steps, success_criteria, tool_requirements, pddl_domain]` per version, by
//! version (fixture).
//! `published(release)` stamps `released_in` on the name's PUBLISHED version (fixture `published`).
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::{SET_BY, decode, field, job_of, place_ref, present};
use crate::design::{DesignTarget, DraftRef, Place, Scope, Snap};
use crate::error::ApiResult;

/// One tenant version as the place stores it.
#[derive(Debug, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
struct Version {
    version: i32,
    status: String,
    context_space: String,
    description: String,
    #[serde(default)]
    reviewers: Vec<String>,
    steps: Value,
    #[serde(default)]
    released_in: Option<String>,
    /// T7a: `Condition[]`.
    #[serde(default = "no_criteria")]
    success_criteria: Value,
    #[serde(default)]
    tool_requirements: Vec<String>,
    #[serde(default)]
    pddl_domain: Option<String>,
}

fn no_criteria() -> Value {
    Value::Array(Vec::new())
}

impl Version {
    fn value(self) -> Value {
        json!({
            "version": self.version,
            "status": self.status,
            "context_space": self.context_space,
            "description": self.description,
            "reviewers": self.reviewers,
            "steps": self.steps,
            "released_in": self.released_in,
            "success_criteria": self.success_criteria,
            "tool_requirements": self.tool_requirements,
            "pddl_domain": self.pddl_domain,
        })
    }
}

/// The tenant versions of one procedure name.
pub struct Procedure;

impl DesignTarget for Procedure {
    fn kind(&self) -> &'static str {
        "procedure"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("procedure:{}", field(r, "name"))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let rows = sqlx::query_as::<_, Version>(
                "SELECT version, status, context_space, description, reviewers, steps, released_in, \
                        success_criteria, tool_requirements, pddl_domain \
                   FROM procedure_version WHERE job_id = $1 AND name = $2 ORDER BY version",
            )
            .bind(job_of(at)?)
            .bind(field(r, "name"))
            .fetch_all(&mut *c)
            .await?;
            Ok((!rows.is_empty())
                .then(|| Value::Array(rows.into_iter().map(Version::value).collect())))
        })
    }
    /// Restore: the name's tenant versions become exactly the value's (all removed for `None`). Deleted before
    /// inserted, so the one-PUBLISHED-per-name index never sees two.
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let name = field(r, "name");
            let versions: Vec<Version> = match present(v) {
                Some(x) => decode("procedure", x)?,
                None => Vec::new(),
            };
            sqlx::query("DELETE FROM procedure_version WHERE job_id = $1 AND name = $2")
                .bind(job)
                .bind(name)
                .execute(&mut *c)
                .await?;
            for p in versions {
                sqlx::query(
                    "INSERT INTO procedure_version \
                            (job_id, name, version, status, context_space, description, reviewers, steps, \
                             released_in, created_by, success_criteria, tool_requirements, pddl_domain) \
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
                )
                .bind(job)
                .bind(name)
                .bind(p.version)
                .bind(&p.status)
                .bind(&p.context_space)
                .bind(&p.description)
                .bind(&p.reviewers)
                .bind(&p.steps)
                .bind(&p.released_in)
                .bind(SET_BY)
                .bind(&p.success_criteria)
                .bind(&p.tool_requirements)
                .bind(&p.pddl_domain)
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
            let names: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT name FROM procedure_version WHERE job_id = $1 ORDER BY name",
            )
            .bind(job_of(at)?)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                names
                    .iter()
                    .map(|n| place_ref(&[("name", n.as_str())]))
                    .collect(),
            ))
        })
    }
    /// Fixture: `[version, status, reviewers, steps, success_criteria, tool_requirements, pddl_domain]` per version
    /// (the T7a fields as their defaults when a value predates them), sorted by version.
    fn proj(&self, v: &Snap) -> Value {
        let mut rows: Vec<(i64, Value)> = present(v)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|p| {
                let pick = |f: &str| p.get(f).cloned().unwrap_or(Value::Null);
                let list = |f: &str| match p.get(f) {
                    Some(v) if !v.is_null() => v.clone(),
                    _ => Value::Array(Vec::new()),
                };
                let n = p.get("version").and_then(Value::as_i64).unwrap_or(0);
                (
                    n,
                    json!([
                        pick("version"),
                        pick("status"),
                        pick("reviewers"),
                        pick("steps"),
                        list("success_criteria"),
                        list("tool_requirements"),
                        pick("pddl_domain")
                    ]),
                )
            })
            .collect();
        rows.sort_by_key(|(n, _)| *n);
        Value::Array(rows.into_iter().map(|(_, x)| x).collect())
    }
    /// Fixture: every non-discarded version as `<version> <status>`, joined with ` · `, else `none`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        let parts: Vec<String> = present(v)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|p| p.get("status").and_then(Value::as_str) != Some("DISCARDED"))
            .map(|p| {
                let version = p.get("version").map(super::js_string).unwrap_or_default();
                let status = p
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_lowercase();
                format!("{version} {status}")
            })
            .collect();
        Some(if parts.is_empty() {
            "none".to_string()
        } else {
            parts.join(" · ")
        })
    }
    fn published<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        release: &'a str,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            sqlx::query(
                "UPDATE procedure_version SET released_in = $3, updated_at = clock_timestamp() \
                  WHERE job_id = $1 AND name = $2 AND status = 'PUBLISHED'",
            )
            .bind(job_of(at)?)
            .bind(field(r, "name"))
            .bind(release)
            .execute(&mut *c)
            .await?;
            Ok(())
        })
    }
}
