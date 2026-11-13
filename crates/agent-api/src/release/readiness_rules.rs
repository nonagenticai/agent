//! B2's readiness contributor (spec B2 §2.6, T10): business rules. A rule slot with an unset value is a
//! placeholder → a `rule_placeholder` go-live gap (never blocking), plus the checklist steps `rules` and `dor_rules`
//! (fixture `buildBoard` "rules" + `readinessOf`).
//!
//! The slots are those of what is checked: the release bundle's `rules` for `DesignOf::Release(id)`, else the
//! draft-compiled bundle's (`cx.rules`, compiled once by crate::readiness::checked; else [`rules::draft_slots`]).
//! The values are read in `cx.design` ([`rules::statuses`]).
use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde_json::Value;
use sqlx::PgConnection;

use super::bundle::RuleSlot;
use super::rules;
use crate::design::{DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::readiness::board::{area_shell, job_path, step};
use crate::readiness::{
    AreaKey, AreaStatus, Assessment, Blocks, Enforced, Fixer, Readiness, ReadinessContributor,
    ReadinessCx, ReadinessGap, ReadinessStatus,
};

/// Register B2's contributors (crate::readiness::registry).
pub fn register(r: &mut Readiness) {
    r.register(Arc::new(RulesReadiness));
}

/// The `rules` area.
pub struct RulesReadiness;

impl ReadinessContributor for RulesReadiness {
    fn area(&self) -> AreaKey {
        AreaKey::Rules
    }

    fn assess<'a>(
        &'a self,
        c: &'a mut PgConnection,
        cx: &'a ReadinessCx<'a>,
    ) -> BoxFuture<'a, ApiResult<Assessment>> {
        Box::pin(assess(c, cx))
    }
}

/// The rule slots of release `id`'s stored bundle (first of each id).
pub(crate) async fn release_slots(
    c: &mut PgConnection,
    job: &JobRef,
    id: &str,
) -> ApiResult<Vec<RuleSlot>> {
    let stored: Option<Option<Value>> = sqlx::query_scalar(
        "SELECT bundle -> 'rules' FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(job.id)
    .bind(id)
    .fetch_optional(&mut *c)
    .await?;
    let Some(v) = stored.flatten() else {
        return Ok(Vec::new());
    };
    let all: Vec<RuleSlot> = serde_json::from_value(v).map_err(|e| {
        ApiError::Internal(anyhow::anyhow!(
            "job_release.bundle.rules of {id} is unreadable: {e}"
        ))
    })?;
    let mut slots: Vec<RuleSlot> = Vec::new();
    for r in all {
        if !slots.iter().any(|s| s.id == r.id) {
            slots.push(r);
        }
    }
    Ok(slots)
}

async fn assess(c: &mut PgConnection, cx: &ReadinessCx<'_>) -> ApiResult<Assessment> {
    let mut slots = match &cx.design {
        DesignOf::Release(id) => release_slots(&mut *c, cx.job, id).await?,
        DesignOf::Draft | DesignOf::Live => match cx.rules {
            Some(slots) => slots.to_vec(),
            None => rules::draft_slots(&mut *c, cx.compiler, cx.job).await?,
        },
    };
    super::rule_authoring::merge(
        &mut slots,
        super::rule_authoring::in_design(&mut *c, cx.reg, cx.job, &cx.design).await?,
    )?;
    let statuses = rules::statuses(&mut *c, cx.reg, cx.job, &cx.design, &slots).await?;
    let job = cx.job.key.as_str();
    let href = job_path(job, "/rules");
    let placeholders: Vec<&str> = statuses
        .iter()
        .filter(|s| s.placeholder)
        .map(|s| s.id.as_str())
        .collect();
    let all: Vec<&str> = statuses.iter().map(|s| s.id.as_str()).collect();
    let n = statuses.len();

    let go_live: Vec<ReadinessGap> = statuses
        .iter()
        .zip(&slots)
        .filter(|(s, _)| s.placeholder)
        .map(|(s, slot)| ReadinessGap {
            key: format!("rule:{}", s.id),
            area: AreaKey::Rules,
            subject: s.id.clone(),
            title: format!("{} ({}) is still a placeholder", s.label, s.id),
            why: format!(
                "The rule decides {}. Without your value the agent cannot evaluate it, so every such request goes to a person.",
                slot.applies_to.join(", ")
            ),
            cause: None,
            blocks: Blocks::GoLive,
            fixer: Fixer::TenantAdmin,
            fix_label: format!("Set {} values", s.id),
            href: href.clone(),
            enforced_from: Enforced::M4,
        })
        .collect();

    let ready = n - placeholders.len();
    let mut area = area_shell(AreaKey::Rules, job);
    area.template_part = format!("{n} rule slots");
    area.ready = u32::try_from(ready).unwrap_or(u32::MAX);
    area.total = u32::try_from(n).unwrap_or(u32::MAX);
    area.status = if n == 0 {
        AreaStatus::Todo
    } else if ready < n {
        AreaStatus::Warn
    } else {
        AreaStatus::Ok
    };
    area.summary = if ready < n {
        format!("Placeholder: {}", placeholders.join(", "))
    } else if n > 0 {
        "Every rule has a value".to_string()
    } else {
        "The template has no rule slots yet".to_string()
    };

    let rules_step = if !placeholders.is_empty() {
        step(
            job,
            "rules",
            ReadinessStatus::Placeholder,
            format!("Placeholder: {} has no value yet", placeholders.join(", ")),
        )
    } else if n > 0 {
        step(
            job,
            "rules",
            ReadinessStatus::Done,
            format!("{n} rules with values ({})", all.join(", ")),
        )
    } else {
        step(
            job,
            "rules",
            ReadinessStatus::Todo,
            "The job template has no rule slots yet",
        )
    };
    let dor_step = if !placeholders.is_empty() {
        step(
            job,
            "dor_rules",
            ReadinessStatus::Placeholder,
            format!("{} still a placeholder", placeholders.join(", ")),
        )
    } else {
        step(
            job,
            "dor_rules",
            ReadinessStatus::Done,
            "All rules have values",
        )
    };

    Ok(Assessment {
        area,
        blocking: Vec::new(),
        testing: Vec::new(),
        go_live,
        steps: vec![rules_step, dor_step],
    })
}
