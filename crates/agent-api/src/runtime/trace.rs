//! The test-env reasoning trail of one agent reply (`ReplyTrace`, web `api/types/live.ts`; B4 T8). Ids, keys and
//! outcome codes only (K50): never customer text, never a step's data.
//!
//! The orchestrator reports what the turn did ([`TraceInput`] on `request.resolved`); the interaction daemon adds
//! the design side in its own read-only transaction, before the reply's: the draft fingerprint of the change keys
//! (B2 `design::fingerprint`, as `test_case.draft_fp`), `exercised` (the unpublished changes at places the turn
//! read) and `differs` (B2's draft-vs-live diff on those places, mapped to [`TraceDiff`] kinds). Gates and
//! post-checks stay empty until B5. A prod reply has no trace.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::PgConnection;

use crate::design::{self, CaseDesign, JobRef, Registry, ledger};
use crate::error::ApiResult;

/// What the orchestrator reports about the turn (all optional; the daemon fills the design side).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceInput {
    #[serde(default)]
    pub activity: Option<TraceActivity>,
    /// `name@version` of the procedure that ran.
    #[serde(default)]
    pub procedure: Option<String>,
    #[serde(default)]
    pub steps: Vec<TraceStep>,
    #[serde(default)]
    pub ops: Vec<TraceOp>,
    /// Knowledge passages cited (`policy.returns`).
    #[serde(default)]
    pub cites: Vec<String>,
    /// The design place keys the turn read (`rule_values:RET-01`, `route:order.read`, `setting:stance`, …).
    #[serde(default)]
    pub read: Vec<String>,
    /// D108 (KH T5): the layer markers of the turn's system message (`[layer <key>@<version>]`, in prompt order), from
    /// the qualified request.
    #[serde(default)]
    pub layers: Vec<String>,
    /// KH T5: the layer markers of the assistant rounds' system message, when an assistant ran (it re-assembles from
    /// the case's plan-time design).
    #[serde(default)]
    pub assistant_layers: Vec<String>,
    /// KH T5: why the qualifier's message had no template layer section (`template_errors` | `no_layers`).
    #[serde(default)]
    pub prompt_warning: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceActivity {
    pub key: String,
    #[serde(default)]
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceStep {
    pub label: String,
    /// `done` | `failed` | `asked` | `stopped`.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceOp {
    pub operation: String,
    pub state: String,
    pub code: String,
    #[serde(default)]
    pub instance_id: String,
    #[serde(default)]
    pub latency_ms: u64,
}

/// One rule evaluation of the turn (`TraceCheck`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceCheck {
    /// Rule id (`RET-01`).
    pub id: String,
    pub label: String,
    /// `pass` | `fail`.
    pub verdict: String,
    /// The evaluation's code (`eligible`, `ineligible:<id>`, `escalate`, `placeholder`).
    pub detail: String,
    pub draft_key: Option<String>,
}

/// One way the other design would act differently (`TraceDiff`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceDiff {
    /// `outcome` | `label` | `rule` | `route`.
    pub kind: String,
    /// Ledger key of the unpublished change behind it.
    pub key: Option<String>,
    pub summary: String,
    pub other_text: Option<String>,
}

/// The trail of one test-env reply (web `ReplyTrace`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReplyTrace {
    /// `draft` (test) | `live` (prod; never traced today).
    pub design: String,
    pub env: String,
    pub release_id: Option<String>,
    pub draft_rev: Option<i64>,
    pub draft_fp: Option<String>,
    pub draft_changes: usize,
    pub activity: Option<TraceActivity>,
    pub procedure: Option<String>,
    /// B5 (gate verdicts): empty until then.
    pub gates: Vec<Value>,
    pub steps: Vec<TraceStep>,
    pub ops: Vec<TraceOp>,
    pub checks: Vec<TraceCheck>,
    pub cites: Vec<String>,
    /// B5 (post-checks): empty until then.
    pub post_checks: Vec<Value>,
    pub differs: Vec<TraceDiff>,
    pub exercised: Vec<String>,
    /// D108 (KH T5): the layer markers the turn's prompt was assembled from, in prompt order.
    #[serde(default)]
    pub layers: Vec<String>,
    /// KH T5: the assistant rounds' layer markers (empty when no assistant ran).
    #[serde(default)]
    pub assistant_layers: Vec<String>,
    /// KH T5: set when the prompt had no template layer section (`template_errors` | `no_layers`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_warning: Option<String>,
    /// Why the design side (fingerprint, exercised, differs) is missing: the error code of its read. The reply is
    /// never held back for its trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

/// The [`TraceDiff`] kind of a differing place of design kind `kind`.
pub fn diff_kind(kind: &str) -> &'static str {
    match kind {
        "route" => "route",
        "rule_values" | "rule_passage" | "gate" | "gate_question" | "gate_setting" => "rule",
        "procedure" | "activity" | "space_required" | "narrowing" => "outcome",
        _ => "label",
    }
}

/// The rule evaluations among the facts (`rule.evaluate` results: `{rule, outcome, code}` under their `as` name).
pub fn checks_of(facts: &Map<String, Value>) -> Vec<TraceCheck> {
    let mut out = Vec::new();
    for (name, v) in facts {
        let (Some(rule), Some(outcome)) = (
            v.get("rule").and_then(Value::as_str),
            v.get("outcome").and_then(Value::as_str),
        ) else {
            continue;
        };
        if !matches!(
            outcome,
            "eligible" | "ineligible" | "escalate" | "placeholder"
        ) {
            continue;
        }
        out.push(TraceCheck {
            id: rule.to_string(),
            label: name.clone(),
            verdict: if outcome == "eligible" {
                "pass"
            } else {
                "fail"
            }
            .to_string(),
            detail: v
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or(outcome)
                .to_string(),
            draft_key: None,
        });
    }
    out
}

/// A differing place: its place key, design kind and the ledger key of its newest unpublished change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delta {
    pub place: String,
    pub kind: String,
    pub change: Option<String>,
}

/// `exercised` and `differs` of a turn that read the places `read`: `changed` are the place keys with an
/// unpublished change (the ledger), `deltas` the places where the draft differs from live (B2's diff). Pure.
pub fn exercised_and_differs(
    read: &[String],
    changed: &[String],
    deltas: &[Delta],
) -> (Vec<String>, Vec<TraceDiff>) {
    let read: BTreeSet<&str> = read.iter().map(String::as_str).collect();
    let exercised: BTreeSet<String> = changed
        .iter()
        .chain(deltas.iter().map(|d| &d.place))
        .filter(|k| read.contains(k.as_str()))
        .cloned()
        .collect();
    let differs = deltas
        .iter()
        .filter(|d| read.contains(d.place.as_str()))
        .map(|d| TraceDiff {
            kind: diff_kind(&d.kind).to_string(),
            key: Some(d.change.clone().unwrap_or_else(|| d.place.clone())),
            summary: format!(
                "Live differs at {} ({}): this turn read the draft's value.",
                d.place, d.kind
            ),
            other_text: None,
        })
        .collect();
    (exercised.into_iter().collect(), differs)
}

/// The trail of a test-env reply without its design side.
pub fn base(case: &CaseDesign, input: &TraceInput, facts: &Map<String, Value>) -> ReplyTrace {
    let (release_id, draft_rev) = match case {
        CaseDesign::Live { release_id } => (release_id.clone(), None),
        CaseDesign::Draft { draft_rev } => (None, Some(*draft_rev)),
    };
    ReplyTrace {
        design: if draft_rev.is_some() { "draft" } else { "live" }.to_string(),
        env: if draft_rev.is_some() { "test" } else { "prod" }.to_string(),
        release_id,
        draft_rev,
        draft_fp: None,
        draft_changes: 0,
        activity: input.activity.clone(),
        procedure: input.procedure.clone(),
        gates: Vec::new(),
        steps: input.steps.clone(),
        ops: input.ops.clone(),
        checks: checks_of(facts),
        cites: input.cites.clone(),
        post_checks: Vec::new(),
        differs: Vec::new(),
        exercised: Vec::new(),
        layers: input.layers.clone(),
        assistant_layers: input.assistant_layers.clone(),
        prompt_warning: input.prompt_warning.clone(),
        unavailable: None,
    }
}

/// Add the design side to `t` on the job-scoped connection `c` (reads the draft ledger and B2's diff).
pub async fn add_design(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    read: &[String],
    t: &mut ReplyTrace,
) -> ApiResult<()> {
    let rows = ledger::of_job(&mut *c, job).await?;
    let keys: Vec<String> = rows
        .iter()
        .map(|r| ledger::change_key(&r.key, &r.after))
        .collect();
    let changed: Vec<String> = rows.iter().map(|r| r.key.clone()).collect();
    let deltas: Vec<Delta> = design::diff(&mut *c, reg, job)
        .await?
        .into_iter()
        // an uncovered kind's place: Production reads the working rows too, so the turn's read does not differ
        .filter(|d| d.uncovered.is_none())
        .map(|d| Delta {
            change: d.changes.last().map(|r| r.key.clone()),
            place: d.place.key,
            kind: d.place.kind,
        })
        .collect();
    let (exercised, differs) = exercised_and_differs(read, &changed, &deltas);
    t.draft_fp = Some(design::fingerprint(&keys));
    t.draft_changes = rows.len();
    t.exercised = exercised;
    t.differs = differs;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn checks_come_from_rule_evaluations_only() {
        let facts = json!({
            "order": {"item": "kettle"},
            "window": {"rule": "RET-01", "outcome": "ineligible", "code": "ineligible:RET-01"},
            "odd": {"rule": "X", "outcome": "maybe"},
        });
        let checks = checks_of(facts.as_object().unwrap());
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].id, "RET-01");
        assert_eq!(checks[0].verdict, "fail");
        assert_eq!(checks[0].detail, "ineligible:RET-01");
    }

    #[test]
    fn only_places_the_turn_read_are_exercised_or_differ() {
        let deltas = vec![
            Delta {
                place: "route:order.read".into(),
                kind: "route".into(),
                change: Some("route:order.read".into()),
            },
            Delta {
                place: "setting:stance".into(),
                kind: "setting".into(),
                change: None,
            },
        ];
        let read = vec!["route:order.read".to_string(), "languages".to_string()];
        let changed = vec!["languages".to_string(), "setting:stance".to_string()];
        let (ex, diffs) = exercised_and_differs(&read, &changed, &deltas);
        assert_eq!(
            ex,
            vec!["languages".to_string(), "route:order.read".to_string()]
        );
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].kind, "route");
        assert_eq!(diffs[0].key.as_deref(), Some("route:order.read"));
    }
}
