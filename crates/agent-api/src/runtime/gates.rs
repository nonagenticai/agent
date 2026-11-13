//! Gate hooks and their verdicts (B4 T9b; D94, R15, F-19). B5 implements the hooks in-process (Laya, T10a); B4
//! ships the no-ops: [`NoGate`] answers [`Decision::Allow`] and [`NoHint`] gives no route hint.
//!
//! - [`RequestGate`] is P1, asked by the drain (crate::turn::drain) on every turn before the qualifier.
//! - [`ReplyGate`] is R1 (B5 T12), asked by the interaction daemon (crate::runtime::interaction) inside the reply's
//!   transaction, before the reply is appended: `handover` queues a handover and stores the notice instead of the
//!   reply, `block` stores nothing for the customer (a `gate` case event). The P2 tool gate is the runner's
//!   `ToolGate` (crate::runtime::runner::authorise), asked per call.
//! - A verdict's [`VerdictCause`] tells "a gate answered and fired" ([`VerdictCause::Fired`]) from "the gate
//!   service could not answer and the point fails closed" ([`VerdictCause::Unavailable`]). A handover verdict maps to
//!   the handover reason `gate_blocked` or `gate_unavailable` ([`GateVerdict::handover_reason`]); both follow the
//!   N-12 open-handover rule (one item per conversation: the drain's step 2 absorbs every later message into it).
//!   The P2 (`ToolGate`) and R1 (`ReplyGate`) verdicts B5 adds map the same way.
//! - [`RouteHinter`] (F-19) turns the P1 verdict into an advisory [`RouteHint`]; the qualifier still decides the
//!   context space. The drain stores the hint on [`super::domain::QualifiedRequest::route_hint`] and in the
//!   `qualified` case event (ids, codes and a confidence only, K50).
//!
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::design::RuntimeDesign;
use super::scope::Env;
use crate::design::JobRef;
use crate::state::AppState;

/// What a gate point decided for the turn (B5: any `handover` beats any `block` beats `allow`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Block,
    Handover,
}

/// Why a gate point decided what it did (D94).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictCause {
    /// The gate service answered: a question fired, or none did (`allow`).
    Fired,
    /// The gate service did not answer (timeout, non-2xx, bad body, unreadable secret). With
    /// [`Decision::Handover`] the point failed closed; with [`Decision::Allow`] it failed open.
    Unavailable,
}

/// One asked question's answer as a verdict carries it (B5 T10a): the question key, the chosen option key and the
/// per-option probabilities — ids, codes and numbers only, never text (K50). P1's GS / GA answers feed the
/// [`RouteHinter`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateAnswerLite {
    pub gate: String,
    pub value: String,
    /// Option key → probability (0..=1).
    #[serde(default)]
    pub probabilities: std::collections::BTreeMap<String, f64>,
}

/// One gate point's verdict.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GateVerdict {
    pub decision: Decision,
    pub cause: VerdictCause,
    /// The question keys that decided (fired, or the closed keys of a failed-closed point).
    pub deciding: Vec<String>,
    /// B5's `gate_evaluation.id` of the call (`None` for the no-op hook).
    pub evaluation_id: Option<i64>,
    /// The answers of the asked questions (B5 T10a; empty for the no-op hook and for a point Laya did not answer).
    /// Not serialised when empty, so a verdict without answers keeps B4's shape.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answers: Vec<GateAnswerLite>,
}

impl GateVerdict {
    /// The no-op hook's verdict: allow, nothing decided, no evaluation.
    pub fn allow() -> GateVerdict {
        GateVerdict {
            decision: Decision::Allow,
            cause: VerdictCause::Fired,
            deciding: Vec::new(),
            evaluation_id: None,
            answers: Vec::new(),
        }
    }

    /// The handover reason of a handover verdict: `gate_blocked` (fired) or `gate_unavailable` (the gate service
    /// did not answer); `None` for `allow` and `block`.
    pub fn handover_reason(&self) -> Option<&'static str> {
        match (self.decision, self.cause) {
            (Decision::Handover, VerdictCause::Fired) => Some("gate_blocked"),
            (Decision::Handover, VerdictCause::Unavailable) => Some("gate_unavailable"),
            (Decision::Allow | Decision::Block, _) => None,
        }
    }

    /// The `gate` case event body (ids and codes only, K50).
    pub fn event_body(&self, point: &str) -> serde_json::Value {
        serde_json::json!({
            "point": point, "decision": self.decision, "cause": self.cause, "deciding": self.deciding,
            "evaluation_id": self.evaluation_id,
        })
    }
}

/// What P1 is asked about one turn. `text` is the (card-redacted) customer text of the turn: data, never logged.
pub struct RequestInput<'a> {
    pub tenant: Uuid,
    pub job: &'a JobRef,
    pub env: Env,
    pub conversation_id: &'a str,
    pub text: &'a str,
    pub design: &'a RuntimeDesign,
}

/// P1: the request gate, asked by the drain before the qualifier. Infallible: an outage is a verdict
/// ([`VerdictCause::Unavailable`]), never an error.
pub trait RequestGate: Send + Sync {
    fn check<'a>(
        &'a self,
        st: &'a AppState,
        input: RequestInput<'a>,
    ) -> Pin<Box<dyn Future<Output = GateVerdict> + Send + 'a>>;
}

/// B4's P1 until B5: every turn is allowed.
pub struct NoGate;

impl RequestGate for NoGate {
    fn check<'a>(
        &'a self,
        _st: &'a AppState,
        _input: RequestInput<'a>,
    ) -> Pin<Box<dyn Future<Output = GateVerdict> + Send + 'a>> {
        Box::pin(async { GateVerdict::allow() })
    }
}

/// What R1 is asked about one reply (B5 T12). `reply` is the composed agent reply and `operations` the turn's
/// `(operation, outcome)` pairs: data, never logged.
pub struct ReplyInput<'a> {
    pub tenant: Uuid,
    pub job: &'a JobRef,
    pub env: Env,
    pub conversation_id: &'a str,
    pub case_id: &'a str,
    pub reply: &'a str,
    pub operations: &'a [(String, String)],
    pub design: &'a RuntimeDesign,
}

/// R1: the reply gate (B5), asked before the agent's reply is stored. Infallible, as [`RequestGate`].
pub trait ReplyGate: Send + Sync {
    fn check<'a>(
        &'a self,
        st: &'a AppState,
        input: ReplyInput<'a>,
    ) -> Pin<Box<dyn Future<Output = GateVerdict> + Send + 'a>>;
}

/// B4's R1: every reply is allowed.
pub struct NoReplyGate;

impl ReplyGate for NoReplyGate {
    fn check<'a>(
        &'a self,
        _st: &'a AppState,
        _input: ReplyInput<'a>,
    ) -> Pin<Box<dyn Future<Output = GateVerdict> + Send + 'a>> {
        Box::pin(async { GateVerdict::allow() })
    }
}

/// The likely request type B5 reads from P1's stage/activity answers (F-19). Advisory: the qualifier decides.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RouteHint {
    pub request_type: String,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub activity: Option<String>,
    /// 0.0..=1.0.
    pub confidence: f64,
}

/// F-19: the route hint of a turn from its P1 verdict. B5 T10a implements it.
pub trait RouteHinter: Send + Sync {
    fn hint(&self, p1: &GateVerdict, d: &RuntimeDesign) -> Option<RouteHint>;
}

/// B4's hinter until B5: no hint (the field serialises `null`).
pub struct NoHint;

impl RouteHinter for NoHint {
    fn hint(&self, _p1: &GateVerdict, _d: &RuntimeDesign) -> Option<RouteHint> {
        None
    }
}

/// The gate hooks ([`Default`]: B4's no-ops). `AppState.gates` holds B5's (`state::build` installs them); the drain,
/// the probe and the interaction daemon run with them.
#[derive(Clone)]
pub struct Gates {
    pub request: Arc<dyn RequestGate>,
    pub hinter: Arc<dyn RouteHinter>,
    /// R1 (B5 T12).
    pub reply: Arc<dyn ReplyGate>,
}

impl Default for Gates {
    fn default() -> Gates {
        Gates {
            request: Arc::new(NoGate),
            hinter: Arc::new(NoHint),
            reply: Arc::new(NoReplyGate),
        }
    }
}
