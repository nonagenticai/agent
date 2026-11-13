//! A platform run's findings beyond its own drafts (B8 T10/T11): the run's hydration ([`run::hydrate`], the fixture's
//! rules 1 and 4 over the drafted items) plus, once the Validate step is reached, the machine rules over the
//! department's grid as the generator has written it (`journey::grid::load`: rules 1–6 over the untagged revisions)
//! and the uncovered evidence clusters (rule 7, information only, never red).
//!
//! Before Validate the grid is not consulted: the department may carry seeded content the run is not editing yet, and
//! a run's colour must follow its own drafts. At Package time the grid is always checked (`at_package`).
use std::collections::BTreeSet;

use sqlx::PgConnection;

use super::run::{self, RunDetail};
use crate::error::ApiResult;
use crate::journey::grid;
use crate::journey::model::{GeneratorStepKey, Severity, ValidationFinding};
use crate::journey::rules::{self, Uncovered};

/// The position of `step` in the platform pipeline.
pub fn step_index(step: &str) -> usize {
    GeneratorStepKey::parse(step)
        .and_then(|k| GeneratorStepKey::ALL.iter().position(|x| *x == k))
        .unwrap_or(0)
}

/// Is `step` the Validate step or later?
pub fn from_validate(step: &str) -> bool {
    step_index(step) >= step_index(GeneratorStepKey::Validate.as_str())
}

/// The department grid's findings for a platform run (empty when the grid cannot be read: no layer yet).
pub async fn grid_findings(
    c: &mut PgConnection,
    industry: &str,
    department: &str,
) -> ApiResult<Vec<ValidationFinding>> {
    match grid::load(c, industry, department).await {
        Ok(j) => Ok(j.findings),
        Err(e) if e.code() == "journey_not_found" => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Rule 7 for the run: one info finding per uncovered cluster of its classified evidence uploads.
pub async fn evidence_findings(
    c: &mut PgConnection,
    run: i64,
) -> ApiResult<Vec<ValidationFinding>> {
    let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT uncovered FROM generator_evidence WHERE run_id = $1 AND status = 'classified' ORDER BY seq",
    )
    .bind(run)
    .fetch_all(&mut *c)
    .await?;
    let mut labels: Vec<(String, u32, Option<String>)> = Vec::new();
    for (clusters,) in rows {
        for c in clusters.as_array().into_iter().flatten() {
            let label = c["label"].as_str().unwrap_or_default().to_owned();
            if label.is_empty() {
                continue;
            }
            let tickets = c["tickets"]
                .as_u64()
                .map_or(0, |t| u32::try_from(t).unwrap_or(u32::MAX));
            let stage = c["stage"].as_str().map(str::to_owned);
            labels.push((label, tickets, stage));
        }
    }
    let unc: Vec<Uncovered<'_>> = labels
        .iter()
        .map(|(label, tickets, stage)| Uncovered {
            label,
            tickets: *tickets,
            stage: stage.as_deref(),
        })
        .collect();
    Ok(rules::evidence_findings(&unc))
}

/// Every finding of the run: its hydration, then (open platform runs at Validate or later, or `at_package`) the
/// grid's findings the hydration does not already report, then rule 7.
pub async fn all(
    c: &mut PgConnection,
    d: &RunDetail,
    at_package: bool,
) -> ApiResult<Vec<ValidationFinding>> {
    let (_, mut out) = run::hydrate(d);
    if d.run.status != "running" || d.run.scope != "platform" {
        return Ok(out);
    }
    if !(at_package || from_validate(&d.run.current_step)) {
        return Ok(out);
    }
    let seen: BTreeSet<(u8, Option<String>, Option<String>)> = out
        .iter()
        .map(|f| (f.rule, f.stage.clone(), f.activity.clone()))
        .collect();
    // The run's own stage scope wins over the grid's: `sync_scopes` writes it at the Journey step, so a stage the run
    // marked out of scope is not judged by the grid's (not yet synced) in-scope rows.
    let skipped: BTreeSet<&str> = d
        .stages
        .iter()
        .filter(|s| s.out_of_scope)
        .map(|s| s.stage.as_str())
        .collect();
    for f in grid_findings(&mut *c, &d.run.industry, &d.run.department).await? {
        if f.stage.as_deref().is_some_and(|s| skipped.contains(s)) {
            continue;
        }
        if !seen.contains(&(f.rule, f.stage.clone(), f.activity.clone())) {
            out.push(f);
        }
    }
    out.extend(evidence_findings(&mut *c, d.run.id).await?);
    Ok(out)
}

/// Red findings only.
pub fn reds(f: &[ValidationFinding]) -> Vec<&ValidationFinding> {
    f.iter().filter(|x| x.severity == Severity::Red).collect()
}
