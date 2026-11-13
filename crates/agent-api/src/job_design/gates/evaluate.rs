//! The gate rule evaluator: `lib/ontology/gateRules.ts` `applyRule` / `decide` ported 1:1, the Laya answer → rule
//! measure mapping, which questions a point asks, and one gate point's whole evaluation ([`evaluate`]).
//!
//! - Measure: noul p(true) (value `true` when ≥ 0.5, confidence `max(p, 1 − p)`); choice the top confidence (value
//!   the chosen key); score the 1-based level (value `round(level)`).
//! - [`apply_rule`]: below `min_confidence` the answer is unsure: it never fires, and a fail-closed question hands
//!   over. Otherwise `gte` / `lt` compare the measure with the threshold, `in` checks the chosen key.
//! - [`decide`]: any handover beats any block beats allow (`annotate` never changes the decision).
//! - Failure (timeout, non-2xx, bad body, an unreadable secret the caller reports with [`PointResult::failed`]):
//!   `failed_closed` (decision handover, `deciding` = the closed keys) when an asked question fails closed, else
//!   `failed_open` (allow). Unbound provider: every question `skipped`, decision allow.
use std::collections::BTreeMap;
use std::time::Instant;

use agent_model::kinds::catalog::SideEffect;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::laya::{GATE_BUDGET, LayaAnswer, LayaClient, LayaError, LayaReply};
use crate::job_design::grant_covers;
use crate::job_design::types::{
    AnswerProbability, EffGateQuestion, GateAction, GateDecision, GateEvalAnswer, GateFail,
    GateFireWhen, GatePoint, GateRule, SkippedQuestion,
};
use crate::runtime::gates::{Decision, GateAnswerLite, GateVerdict, VerdictCause};

/// The skip reason of an unbound `gates` use (UI `LLM_USE_RULES.gates.fallback`).
pub const NO_PROVIDER: &str = "no Laya provider bound";

/// The value a rule compares (web `RawAnswer`).
#[derive(Clone, Debug, PartialEq)]
pub struct RawAnswer {
    /// Chosen option key (`true` / `false` for noul, the level for score).
    pub value: String,
    /// noul: p(true); choice: confidence; score: level.
    pub measure: f64,
    pub confidence: f64,
}

/// What a rule made of one answer (web `applyRule`'s result).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RuleOutcome {
    pub fired: bool,
    pub unsure: bool,
    pub action: Option<GateAction>,
}

/// Web `applyRule`.
pub fn apply_rule(
    rule: &GateRule,
    a: &RawAnswer,
    fail: GateFail,
    action: GateAction,
) -> RuleOutcome {
    let unsure = rule.min_confidence.is_some_and(|m| a.confidence < m);
    if unsure {
        return RuleOutcome {
            fired: false,
            unsure,
            action: (fail == GateFail::Closed).then_some(GateAction::Handover),
        };
    }
    let fired = match (rule.fire_when, rule.threshold) {
        (GateFireWhen::In, _) => rule
            .fire_values
            .as_deref()
            .unwrap_or_default()
            .contains(&a.value),
        (_, None) => false,
        (GateFireWhen::Gte, Some(t)) => a.measure >= t,
        (GateFireWhen::Lt, Some(t)) => a.measure < t,
    };
    RuleOutcome {
        fired,
        unsure,
        action: fired.then_some(action),
    }
}

/// Web `decide`: any handover, then any block, else allow.
pub fn decide(actions: impl IntoIterator<Item = Option<GateAction>>) -> GateDecision {
    let mut d = GateDecision::Allow;
    for a in actions.into_iter().flatten() {
        match a {
            GateAction::Handover => return GateDecision::Handover,
            GateAction::Block => d = GateDecision::Block,
            GateAction::Annotate => {}
        }
    }
    d
}

/// The rule measure of a Laya answer.
pub fn raw_answer(q: &EffGateQuestion, a: &LayaAnswer) -> RawAnswer {
    match a {
        LayaAnswer::Noul { p_true } => RawAnswer {
            value: if *p_true >= 0.5 { "true" } else { "false" }.to_owned(),
            measure: *p_true,
            confidence: p_true.max(1.0 - p_true),
        },
        LayaAnswer::Choice {
            choice, confidence, ..
        } => RawAnswer {
            value: choice.clone(),
            measure: *confidence,
            confidence: *confidence,
        },
        LayaAnswer::Score {
            level, confidence, ..
        } => {
            let n = q.options.len().max(1) as f64;
            RawAnswer {
                value: (level.round().clamp(1.0, n) as u64).to_string(),
                measure: *level,
                confidence: *confidence,
            }
        }
    }
}

fn label_of(q: &EffGateQuestion, key: &str) -> String {
    q.options
        .iter()
        .find(|o| o.key == key)
        .map(|o| o.label.clone())
        .unwrap_or_else(|| key.to_owned())
}

/// One answer as the playground shows it, the rule applied.
pub fn eval_answer(q: &EffGateQuestion, a: &LayaAnswer) -> GateEvalAnswer {
    let raw = raw_answer(q, a);
    let outcome = apply_rule(&q.rule, &raw, q.fail, q.action);
    let probs: BTreeMap<String, f64> = match a {
        LayaAnswer::Noul { p_true } => BTreeMap::from([
            ("true".to_owned(), *p_true),
            ("false".to_owned(), 1.0 - p_true),
        ]),
        LayaAnswer::Choice { probabilities, .. } | LayaAnswer::Score { probabilities, .. } => {
            probabilities.clone()
        }
    };
    let mut probabilities: Vec<AnswerProbability> = probs
        .into_iter()
        .map(|(key, p)| AnswerProbability {
            label: label_of(q, &key),
            key,
            p,
        })
        .collect();
    probabilities.sort_by(|x, y| y.p.total_cmp(&x.p));
    GateEvalAnswer {
        gate: q.key.clone(),
        question: q.question.clone(),
        kind: q.kind,
        answer: label_of(q, &raw.value),
        value: raw.value,
        probabilities,
        confidence: raw.confidence,
        rule: q.rule.clone(),
        fail: q.fail,
        safety: q.safety,
        fired: outcome.fired,
        unsure: outcome.unsure,
        action: outcome.action,
    }
}

/// The questions asked at `point` (P1 / R1: all of them; use [`asked_for_tool`] for P2).
pub fn asked_at(questions: &[EffGateQuestion], point: GatePoint) -> Vec<&EffGateQuestion> {
    questions.iter().filter(|q| q.point == point).collect()
}

/// The P2 questions asked before a call of `op_key` (with its side effect): a question listing tools is asked for
/// the tools it covers (`grant_covers`, so `refund.issue` covers `refund.issue{full}`); one without tools (null or
/// empty) for every W / X / P operation, never for a read.
pub fn asked_for_tool<'a>(
    questions: &'a [EffGateQuestion],
    op_key: &str,
    side_effect: SideEffect,
) -> Vec<&'a EffGateQuestion> {
    questions
        .iter()
        .filter(|q| q.point == GatePoint::P2)
        .filter(|q| match q.tools.as_deref() {
            Some(tools) if !tools.is_empty() => tools.iter().any(|t| grant_covers(t, op_key)),
            _ => side_effect != SideEffect::R,
        })
        .collect()
}

/// How a gate point went.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Laya answered (or nothing was asked).
    Answered,
    /// No Laya provider is bound: every question skipped, allow.
    Skipped,
    /// Laya gave no answer and no asked question fails closed: allow.
    FailedOpen,
    /// Laya gave no answer and an asked question fails closed: handover.
    FailedClosed,
}

/// One gate point's evaluation.
#[derive(Clone, Debug, PartialEq)]
pub struct PointResult {
    pub decision: GateDecision,
    pub outcome: Outcome,
    /// Keys that decided: fired (or unsure + closed) with handover / block, or the closed keys of a failed point.
    pub deciding: Vec<String>,
    pub answers: Vec<GateEvalAnswer>,
    /// The rule measure of each answer by question key (noul p(true), choice confidence, score level): the verdict
    /// log's `gate_answer.measure`.
    pub measures: BTreeMap<String, f64>,
    pub skipped: Vec<SkippedQuestion>,
    pub latency_ms: u64,
    /// The failure's code ([`LayaError::code`]), when Laya gave no answer.
    pub error: Option<&'static str>,
    /// The model Laya routed to, when it said.
    pub model: Option<String>,
    /// Laya's usage, for the visibility row (`usage::emit(source="gate")`).
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl PointResult {
    fn empty(decision: GateDecision, outcome: Outcome) -> Self {
        PointResult {
            decision,
            outcome,
            deciding: Vec::new(),
            answers: Vec::new(),
            measures: BTreeMap::new(),
            skipped: Vec::new(),
            latency_ms: 0,
            error: None,
            model: None,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    /// No provider bound: every question skipped, allow.
    pub fn unbound(questions: &[&EffGateQuestion]) -> Self {
        let mut r = Self::empty(GateDecision::Allow, Outcome::Skipped);
        r.skipped = questions
            .iter()
            .map(|q| SkippedQuestion {
                gate: q.key.clone(),
                reason: NO_PROVIDER.to_owned(),
            })
            .collect();
        r
    }

    /// Laya gave no answer (`err`), or the provider could not be used (an unreadable secret: the caller passes a
    /// [`LayaError`] it chose): failed closed if any asked question fails closed, else failed open.
    pub fn failed(questions: &[&EffGateQuestion], err: &LayaError) -> Self {
        let closed: Vec<String> = questions
            .iter()
            .filter(|q| q.fail == GateFail::Closed)
            .map(|q| q.key.clone())
            .collect();
        let mut r = if closed.is_empty() {
            Self::empty(GateDecision::Allow, Outcome::FailedOpen)
        } else {
            Self::empty(GateDecision::Handover, Outcome::FailedClosed)
        };
        r.deciding = closed;
        r.error = Some(err.code());
        r.skipped = questions
            .iter()
            .map(|q| SkippedQuestion {
                gate: q.key.clone(),
                reason: format!("Laya unavailable ({})", err.code()),
            })
            .collect();
        r
    }

    /// Laya answered every asked question.
    pub fn answered(questions: &[&EffGateQuestion], reply: &LayaReply) -> Self {
        let answers: Vec<GateEvalAnswer> = questions
            .iter()
            .filter_map(|q| reply.answers.get(&q.key).map(|a| eval_answer(q, a)))
            .collect();
        let mut r = Self::empty(decide(answers.iter().map(|a| a.action)), Outcome::Answered);
        r.deciding = answers
            .iter()
            .filter(|a| matches!(a.action, Some(GateAction::Handover | GateAction::Block)))
            .map(|a| a.gate.clone())
            .collect();
        r.measures = questions
            .iter()
            .filter_map(|q| {
                reply
                    .answers
                    .get(&q.key)
                    .map(|a| (q.key.clone(), raw_answer(q, a).measure))
            })
            .collect();
        r.answers = answers;
        r.model = reply.model.clone();
        r.input_tokens = reply.input_tokens;
        r.output_tokens = reply.output_tokens;
        r
    }

    /// B4's verdict of this point (D94): `Unavailable` for a failed point, else `Fired`; it carries every answer's
    /// option key and probabilities (T10a, the route hint's input), never text.
    pub fn verdict(&self, evaluation_id: Option<i64>) -> GateVerdict {
        GateVerdict {
            decision: match self.decision {
                GateDecision::Allow => Decision::Allow,
                GateDecision::Block => Decision::Block,
                GateDecision::Handover => Decision::Handover,
            },
            cause: match self.outcome {
                Outcome::FailedClosed | Outcome::FailedOpen => VerdictCause::Unavailable,
                Outcome::Answered | Outcome::Skipped => VerdictCause::Fired,
            },
            deciding: self.deciding.clone(),
            evaluation_id,
            answers: self
                .answers
                .iter()
                .map(|a| GateAnswerLite {
                    gate: a.gate.clone(),
                    value: a.value.clone(),
                    probabilities: a
                        .probabilities
                        .iter()
                        .map(|p| (p.key.clone(), p.p))
                        .collect(),
                })
                .collect(),
        }
    }
}

/// Ask `questions` (all at one gate point) about `state`: one Laya call within [`GATE_BUDGET`], no call when nothing
/// is asked or `client` is `None` (unbound).
pub async fn evaluate(
    client: Option<&LayaClient>,
    state: Value,
    questions: &[&EffGateQuestion],
) -> PointResult {
    if questions.is_empty() {
        return PointResult::empty(GateDecision::Allow, Outcome::Answered);
    }
    let Some(client) = client else {
        return PointResult::unbound(questions);
    };
    let started = Instant::now();
    let reply = match tokio::time::timeout(GATE_BUDGET, client.ask(state, questions)).await {
        Ok(r) => r,
        Err(_) => Err(LayaError::Timeout),
    };
    let mut r = match reply {
        Ok(reply) => PointResult::answered(questions, &reply),
        Err(e) => PointResult::failed(questions, &e),
    };
    r.latency_ms = started.elapsed().as_millis() as u64;
    r
}
