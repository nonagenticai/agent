//! A generator run as the P18 pages read it (B8 T9a): the step lists per scope (`lib/journey/P18GenPipeline.ts`
//! `stepsFor` / `llmStepsFor`), the read-time hydration of a run (fixture `hydrateRun`: stages' activities derived
//! from accepted journey items, rule 1 per live new object not approved, rule 4 per in-scope stage without an
//! activity, one amber finding for undecided drafts at the current step), the summary/detail JSON and the usage
//! estimate (D56: tokens on a funding lane, never a price). Pure: [`super::store`] loads the rows.
use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::journey::model::{GeneratorStepKey, Severity, ValidationFinding};

/// Platform or tenant run (D97).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Platform,
    Tenant,
}

impl Scope {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "platform" => Some(Self::Platform),
            "tenant" => Some(Self::Tenant),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Platform => "platform",
            Self::Tenant => "tenant",
        }
    }
}

/// Plan P8 T3 `Step::order(RunClass::Tenant)` = web `P18_TENANT_STEPS`: a tenant run refines an existing job, so it
/// skips the platform-only steps (business model, operations, harmonise; B-P8-4).
pub const TENANT_STEPS: [GeneratorStepKey; 6] = [
    GeneratorStepKey::Input,
    GeneratorStepKey::Journey,
    GeneratorStepKey::Objects,
    GeneratorStepKey::Validate,
    GeneratorStepKey::UseCases,
    GeneratorStepKey::Package,
];

/// The steps a run of `scope` walks, in order (web `stepsFor`).
pub fn steps_for(scope: Scope) -> Vec<GeneratorStepKey> {
    match scope {
        Scope::Platform => GeneratorStepKey::ALL.to_vec(),
        Scope::Tenant => TENANT_STEPS.to_vec(),
    }
}

/// The subset of [`steps_for`] that calls the LLM (web `llmStepsFor`).
pub fn llm_steps_for(scope: Scope) -> Vec<GeneratorStepKey> {
    steps_for(scope)
        .into_iter()
        .filter(|k| k.drafts())
        .collect()
}

/// Fallback token range when no packaged run of the scope has usage yet (fixture `estimateGeneratorRun`).
pub fn fallback_tokens(scope: Scope) -> i64 {
    match scope {
        Scope::Platform => 1_200_000,
        Scope::Tenant => 100_000,
    }
}

/// `[min × 0.8, max × 1.25]` of the past runs' tokens, else of the fallback (rounded like `Math.round`).
pub fn estimate_range(scope: Scope, past_tokens: &[i64]) -> (i64, i64) {
    let lo = past_tokens
        .iter()
        .copied()
        .min()
        .unwrap_or_else(|| fallback_tokens(scope));
    let hi = past_tokens
        .iter()
        .copied()
        .max()
        .unwrap_or_else(|| fallback_tokens(scope));
    (round(lo as f64 * 0.8), round(hi as f64 * 1.25))
}

fn round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

/// One run row (`generator_run` + the sum of its cost lines).
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct RunRow {
    pub id: i64,
    pub tenant_id: Uuid,
    pub scope: String,
    pub industry: String,
    pub department: String,
    pub job_key: String,
    pub job_id: Option<Uuid>,
    pub status: String,
    pub current_step: String,
    pub description: String,
    pub objects: Vec<String>,
    pub output: Vec<String>,
    pub tagged: Option<String>,
    pub funding: String,
    pub started_by: String,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub tokens: i64,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct StepRow {
    pub step: String,
    pub status: String,
    pub summary: String,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct StageRow {
    pub stage: String,
    pub label: String,
    pub out_of_scope: bool,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ItemRow {
    pub item_id: String,
    pub step: String,
    pub kind: String,
    pub title: String,
    pub detail: String,
    pub example: Option<Value>,
    pub decision: String,
    pub decision_note: Option<String>,
    /// `payload.key`, the activity key the draft names (`title` is a human name).
    pub payload_key: Option<String>,
    pub new_object: Option<String>,
    /// F-23: a proposed `operation` row (object, verb, variant and the F-22 fields).
    pub new_operation: Option<Value>,
    pub proposal: String,
    pub llm_confidence: f32,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct CostLineRow {
    pub step: String,
    pub model: String,
    pub tokens: i64,
    pub funding: String,
    pub at: DateTime<Utc>,
}

/// A run with its children.
#[derive(Clone, Debug)]
pub struct RunDetail {
    pub run: RunRow,
    pub steps: Vec<StepRow>,
    pub stages: Vec<StageRow>,
    pub items: Vec<ItemRow>,
    pub cost_lines: Vec<CostLineRow>,
}

/// What a person decided keeps the item in the run (fixture `LIVE`).
pub fn is_live(decision: &str) -> bool {
    matches!(decision, "accepted" | "renamed" | "merged")
}

/// A stage with the activities the run's accepted journey drafts put in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydratedStage {
    pub key: String,
    pub label: String,
    pub activities: Vec<String>,
    pub out_of_scope: bool,
}

/// Fixture `hydrateRun`: the stages' activities and the findings (none once packaged).
pub fn hydrate(d: &RunDetail) -> (Vec<HydratedStage>, Vec<ValidationFinding>) {
    let acts: Vec<&ItemRow> = d
        .items
        .iter()
        .filter(|i| i.step == "journey" && i.kind == "activity" && is_live(&i.decision))
        .collect();
    let keys: Vec<(&ItemRow, String)> = acts
        .iter()
        .filter_map(|i| {
            super::accept::activity_key(&i.decision, &i.title, i.payload_key.as_deref())
                .map(|k| (*i, k))
        })
        .collect();
    let stages: Vec<HydratedStage> = d
        .stages
        .iter()
        .map(|s| {
            let prefix = format!("{}:", s.stage);
            HydratedStage {
                key: s.stage.clone(),
                label: s.label.clone(),
                activities: keys
                    .iter()
                    .filter(|(_, k)| k.starts_with(&prefix))
                    .map(|(_, k)| k.clone())
                    .collect(),
                out_of_scope: s.out_of_scope,
            }
        })
        .collect();
    let mut findings = Vec::new();
    if d.run.status != "packaged" {
        for s in &stages {
            if !s.out_of_scope && s.activities.is_empty() {
                findings.push(ValidationFinding {
                    rule: 4,
                    severity: Severity::Red,
                    message: format!("stage {} has no activity", s.label),
                    stage: Some(s.key.clone()),
                    activity: None,
                });
            }
        }
        for i in &d.items {
            if let Some(obj) = &i.new_object
                && is_live(&i.decision)
                && i.proposal != "approved"
            {
                findings.push(ValidationFinding {
                    rule: 1,
                    severity: Severity::Red,
                    message: format!(
                        "{} names object {obj}, not canonical (proposal {})",
                        i.title, i.proposal
                    ),
                    stage: None,
                    activity: i.title.split(" × ").next().map(str::to_string),
                });
            }
        }
        let pending = d
            .items
            .iter()
            .filter(|i| i.step == d.run.current_step && i.decision == "pending")
            .count();
        if pending > 0 {
            findings.push(ValidationFinding {
                rule: 0,
                severity: Severity::Amber,
                message: format!("{pending} draft(s) at this step not decided yet"),
                stage: None,
                activity: None,
            });
        }
    }
    (stages, findings)
}

/// Red findings (they block Package).
pub fn red_of(findings: &[ValidationFinding]) -> usize {
    findings
        .iter()
        .filter(|f| f.severity == Severity::Red)
        .count()
}

/// The summary status: `running` with a red finding reads `blocked`.
pub fn status_of(status: &str, red: usize) -> &'static str {
    match status {
        "running" if red > 0 => "blocked",
        "running" => "running",
        "packaged" => "packaged",
        _ => "abandoned",
    }
}

pub fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// `GeneratorUsage` (D56): tenant runs are the tenant's usage (project = tenant id), platform runs the platform's.
pub fn usage_json(r: &RunRow) -> Value {
    let tenant = r.scope == "tenant";
    json!({
        "tokens": r.tokens,
        "funding": r.funding,
        "attributed_to": if tenant { "tenant" } else { "platform" },
        "project": if tenant { Some(r.tenant_id.to_string()) } else { None },
        "activity": "generator",
    })
}

/// `GeneratorRunSummary`. A platform run shows no tenant (it carries the platform-owned tenant only for RLS, R15).
pub fn summary_json(r: &RunRow, tenant_name: Option<&str>, red: usize) -> Value {
    let tenant = r.scope == "tenant";
    json!({
        "id": r.id,
        "scope": r.scope,
        "industry": r.industry,
        "department": r.department,
        "job": r.job_key,
        "tenant_id": if tenant { Some(r.tenant_id.to_string()) } else { None },
        "tenant_name": if tenant { tenant_name } else { None },
        "status": status_of(&r.status, red),
        "current_step": r.current_step,
        "started_by": r.started_by,
        "started_at": iso(r.started_at),
        "updated_at": iso(r.updated_at),
        "output": r.output,
        "cost": usage_json(r),
        "red": red,
    })
}

/// `GeneratorRun`: the summary plus steps, items, hydrated stages, findings and the cost lines.
pub fn detail_json(d: &RunDetail, tenant_name: Option<&str>) -> Value {
    detail_json_with(d, tenant_name, None)
}

/// [`detail_json`] with the run's findings given (the hydration plus the grid's and the evidence's, see
/// `generator::findings`); `None` = the hydration alone.
pub fn detail_json_with(
    d: &RunDetail,
    tenant_name: Option<&str>,
    findings: Option<Vec<ValidationFinding>>,
) -> Value {
    let (stages, hydrated) = hydrate(d);
    let findings = findings.unwrap_or(hydrated);
    let red = red_of(&findings);
    let mut v = summary_json(&d.run, tenant_name, red);
    let o = v.as_object_mut().expect("summary is an object");
    o.insert("description".into(), json!(d.run.description));
    o.insert(
        "steps".into(),
        Value::Array(
            d.steps
                .iter()
                .map(|s| json!({"key": s.step, "status": s.status, "summary": s.summary}))
                .collect(),
        ),
    );
    o.insert(
        "items".into(),
        Value::Array(
            d.items
                .iter()
                .map(|i| {
                    json!({
                        "id": i.item_id,
                        "step": i.step,
                        "kind": i.kind,
                        "title": i.title,
                        "detail": i.detail,
                        "example": i.example,
                        "decision": i.decision,
                        "decision_note": i.decision_note,
                        "new_object": i.new_object,
                        "new_operation": i.new_operation,
                        "proposal": i.proposal,
                        "llm_confidence": i.llm_confidence,
                    })
                })
                .collect(),
        ),
    );
    o.insert(
        "stages".into(),
        Value::Array(
            stages
                .iter()
                .map(|s| {
                    json!({"key": s.key, "label": s.label, "activities": s.activities,
                           "out_of_scope": s.out_of_scope})
                })
                .collect(),
        ),
    );
    o.insert("objects".into(), json!(d.run.objects));
    o.insert("findings".into(), json!(findings));
    o.insert("tagged".into(), json!(d.run.tagged));
    o.insert(
        "cost_lines".into(),
        Value::Array(
            d.cost_lines
                .iter()
                .map(|l| {
                    json!({"step": l.step, "model": l.model, "tokens": l.tokens, "funding": l.funding,
                           "at": iso(l.at)})
                })
                .collect(),
        ),
    );
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(v: &[GeneratorStepKey]) -> Vec<&'static str> {
        v.iter().map(|k| k.as_str()).collect()
    }

    /// `steps_match_web_pipeline`: the Rust lists equal the web's `P18_STEPS` / `P18_TENANT_STEPS`, read from the
    /// TypeScript source (`key: '…'`, `drafts: null` = no LLM call).
    #[test]
    fn steps_match_web_pipeline() {
        let src = include_str!("../../../../web/src/lib/journey/P18GenPipeline.ts");
        let steps_block = &src[src.find("export const P18_STEPS").expect("P18_STEPS")..];
        let steps_block = &steps_block[..steps_block.find("];").expect("end of P18_STEPS")];
        let mut order = Vec::new();
        let mut llm = Vec::new();
        for entry in steps_block.split("key: '").skip(1) {
            let key = &entry[..entry.find('\'').expect("key quote")];
            order.push(key.to_string());
            if !entry.contains("drafts: null") {
                llm.push(key.to_string());
            }
        }
        assert_eq!(keys(&steps_for(Scope::Platform)), order);
        assert_eq!(keys(&llm_steps_for(Scope::Platform)), llm);
        let tenant_block = &src[src
            .find("export const P18_TENANT_STEPS")
            .expect("P18_TENANT_STEPS")..];
        let tenant_block = &tenant_block[..tenant_block.find("];").expect("end")];
        let tenant: Vec<String> = tenant_block
            .split('\'')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect();
        assert_eq!(keys(&steps_for(Scope::Tenant)), tenant);
        let tenant_llm: Vec<String> = tenant
            .iter()
            .filter(|k| llm.contains(*k))
            .cloned()
            .collect();
        assert_eq!(keys(&llm_steps_for(Scope::Tenant)), tenant_llm);
    }

    #[test]
    fn estimate_uses_past_runs_else_fallback() {
        assert_eq!(
            estimate_range(Scope::Platform, &[1_000_000, 2_000_000]),
            (800_000, 2_500_000)
        );
        assert_eq!(estimate_range(Scope::Platform, &[]), (960_000, 1_500_000));
        assert_eq!(estimate_range(Scope::Tenant, &[]), (80_000, 125_000));
    }

    fn run(status: &str, current: &str) -> RunRow {
        RunRow {
            id: 7,
            tenant_id: Uuid::nil(),
            scope: "platform".into(),
            industry: "retail".into(),
            department: "customer_service".into(),
            job_key: "retail_agent".into(),
            job_id: None,
            status: status.into(),
            current_step: current.into(),
            description: String::new(),
            objects: vec![],
            output: vec![],
            tagged: None,
            funding: "platform_external".into(),
            started_by: "u".into(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
            tokens: 0,
        }
    }

    fn item(id: &str, step: &str, kind: &str, title: &str, decision: &str) -> ItemRow {
        ItemRow {
            item_id: id.into(),
            step: step.into(),
            kind: kind.into(),
            title: title.into(),
            detail: String::new(),
            example: None,
            decision: decision.into(),
            decision_note: None,
            payload_key: None,
            new_object: None,
            new_operation: None,
            proposal: "none".into(),
            llm_confidence: 0.9,
        }
    }

    fn stage(k: &str, out: bool) -> StageRow {
        StageRow {
            stage: k.into(),
            label: k.to_uppercase(),
            out_of_scope: out,
        }
    }

    #[test]
    fn hydrate_derives_activities_and_rules_1_4_and_amber() {
        let mut new_obj = item(
            "7-objects-1",
            "objects",
            "activity_object",
            "purchase:buy × voucher",
            "accepted",
        );
        new_obj.new_object = Some("voucher".into());
        let d = RunDetail {
            run: run("running", "objects"),
            steps: vec![],
            stages: vec![
                stage("purchase", false),
                stage("usage", false),
                stage("exit", true),
            ],
            items: vec![
                item(
                    "7-journey-1",
                    "journey",
                    "activity",
                    "purchase:buy",
                    "accepted",
                ),
                item(
                    "7-journey-2",
                    "journey",
                    "activity",
                    "usage:help",
                    "dropped",
                ),
                new_obj,
                item("7-objects-2", "objects", "activity_object", "x", "pending"),
            ],
            cost_lines: vec![],
        };
        let (stages, f) = hydrate(&d);
        assert_eq!(stages[0].activities, vec!["purchase:buy".to_string()]);
        assert!(stages[1].activities.is_empty());
        let rules: Vec<u8> = f.iter().map(|x| x.rule).collect();
        assert_eq!(rules, vec![4, 1, 0], "{f:?}");
        assert_eq!(f[0].stage.as_deref(), Some("usage"));
        assert_eq!(f[1].activity.as_deref(), Some("purchase:buy"));
        assert_eq!(red_of(&f), 2);
        let s = summary_json(&d.run, None, red_of(&f));
        assert_eq!(s["status"], "blocked");
        assert_eq!(s["tenant_id"], Value::Null);
        assert_eq!(s["cost"]["attributed_to"], "platform");
    }

    #[test]
    fn packaged_run_has_no_findings() {
        let d = RunDetail {
            run: run("packaged", "package"),
            steps: vec![],
            stages: vec![stage("purchase", false)],
            items: vec![],
            cost_lines: vec![],
        };
        let (_, f) = hydrate(&d);
        assert!(f.is_empty());
        assert_eq!(status_of("packaged", 0), "packaged");
    }
}
