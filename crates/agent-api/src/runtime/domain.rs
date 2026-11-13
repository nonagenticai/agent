//! The runtime's message payloads and the orchestrator's plan (port of care-bus-agents@96c608c
//! `crates/carebus/src/domain.rs`, B4 T7). Worker / tool shapes and the `TOOLS` / `AREAS` constants are not carried:
//! a step is an operation (`object.verb{variant}`) run by the runner (R1), and the areas come from the job's context
//! spaces ([`super::spaces::SpaceCatalog`]).
//!
//! | Payload | Topic | Stream | From → to |
//! |---|---|---|---|
//! | [`QualifiedRequest`] | `request.qualified` | `bus:customer:{area}` | drain (T9) → orchestrator |
//! | [`OpCall`] | `op.call` | `bus:op` | orchestrator → runner (T6) |
//! | [`OpResult`] | `op.result` | `bus:op:results` | runner → orchestrator |
//! | [`Resolution`] | `request.resolved` | `bus:customer:{area}` | orchestrator → interaction (T8) |
//!
//! Shared in wave W2: T7 owns this file; T6 / T8 / T9 append their own fields and types (serde defaults keep older
//! payloads readable). Customer text never travels in a payload (K50): requests carry slots and ids, replies are
//! composed by the interaction daemon from the [`Resolution`] facts.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::conditions::{CaseFacts, Condition, child, parse_filter, pick};
use super::reply::KnowledgeUse;
use super::scope::Env;
use super::trace::TraceInput;
use crate::design::CaseDesign;
use crate::job_design::types::ProcedureStep;

pub use super::bus::topics::{OP_CALL, OP_RESULT, REQUEST_QUALIFIED, REQUEST_RESOLVED};

/// The `agent_role` of a procedure step's call (12 §2.1). The assistant loop (T7a) sends [`ROLE_ASSISTANT`].
pub const ROLE_PROCEDURE: &str = "procedure";
/// The `agent_role` of the assistant loop's calls (T7a, F-40).
pub const ROLE_ASSISTANT: &str = "assistant";

/// The `provenance` of a call the orchestrator makes to read the identified customer's records at case start (B4
/// known customer): the runner skips the activity / ToolPoint gates for it and admits only the read operations of
/// [`crate::runtime::customer_context::CONTEXT_OPS`] (anything else is `not_context_op`).
pub const PROVENANCE_CONTEXT: &str = "context";

/// A2: the `provenance` of a producer call the runner makes itself to fill a declared `state` input of another call
/// (`runner::declared`): the tenant's declared chain is its authorisation, so the runner skips the confirmation (step
/// 4), the ToolPoint gate (5b) and the ownership gate (3b) for it, never resolves further inputs (the parent already
/// planned the whole chain), and audits it like any call (`operation_call`, phase `resolve`; keys, never values).
pub const PROVENANCE_RESOLVE: &str = "resolve";

/// The fields a request carries, by name (the context space's `slots`, e.g. `order_id`). A blank value is no value.
pub type Slots = BTreeMap<String, String>;

/// `request.qualified`: the drain qualified a customer message into a context space and stored the case row (R6:
/// the row exists before this is published).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct QualifiedRequest {
    /// `CS-<n>`.
    pub case_id: String,
    pub conversation_id: String,
    /// Department: the `{area}` of `bus:customer:{area}`.
    pub area: String,
    /// Ontology key, e.g. `retail:order-support:returns`.
    pub context_space: String,
    /// The space's request type (`return`); the last key segment when the space names none.
    #[serde(default)]
    pub request_type: String,
    /// The activity the request was qualified into (B5 activities; `None` until then).
    #[serde(default)]
    pub activity: Option<String>,
    /// The ontology object the request is about (`order`).
    #[serde(default)]
    pub object: Option<String>,
    /// `chat` | `voice` | … (the UI's `Modality`).
    #[serde(default)]
    pub modality: Option<String>,
    #[serde(default)]
    pub slots: Slots,
    #[serde(default)]
    pub customer_id: Option<String>,
    /// The case's verified identity level (0..=3) at qualification.
    #[serde(default)]
    pub identity_level: u8,
    /// The inbound customer message the request was qualified from: the `confirm_ref` of the plan's W/X/P calls
    /// (12 §3.2: an inbound customer message id, never LLM or backend text).
    #[serde(default)]
    pub message_id: Option<String>,
    /// The staff member who started the conversation (staff test chat), for `op.call.initiator_sub`.
    #[serde(default)]
    pub initiator_sub: Option<String>,
    /// The design read at qualification (diagnostic, `agent_case.qualified_*`); the case runs on its PLAN's design.
    #[serde(default)]
    pub qualified_release_id: Option<String>,
    #[serde(default)]
    pub qualified_draft_rev: Option<i64>,
    /// The advisory route hint of the turn's P1 verdict (F-19, B4 T9b; `None` until B5's `RouteHinter`).
    #[serde(default)]
    pub route_hint: Option<super::gates::RouteHint>,
    /// D108 (KH T5): the layer markers of the qualifier's system message (crate::runtime::prompt::SystemMessage::layers,
    /// `[layer <key>@<version>]`), as the case's `qualified` event stores them; the test-env reply trace lists them.
    #[serde(default)]
    pub layers: Vec<String>,
    /// KH T5: why that message had no template layer section (`template_errors` | `no_layers`; `None` = it had).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_warning: Option<String>,
}

impl QualifiedRequest {
    /// The space inside the area: the context-space key's last segment (`returns`).
    pub fn space(&self) -> &str {
        self.context_space.rsplit(':').next().unwrap_or_default()
    }

    /// The topic of `event` in the request's context space, e.g. `order-support.returns.request.resolved`.
    pub fn topic(&self, event: &str) -> String {
        format!("{}.{}.{event}", self.area, self.space())
    }
}

/// A procedure version (`name@version`): what `agent_case.procedure_name` / `procedure_version` record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcedureRef {
    pub name: String,
    pub version: u32,
}

impl ProcedureRef {
    /// `name@version`, as `Envelope::procedure` carries it.
    pub fn label(&self) -> String {
        format!("{}@{}", self.name, self.version)
    }
}

/// One procedure step: the UI's `ProcedureStep` (`web/src/lib/api/types/ontology.ts`). Steps of one phase run in
/// parallel; phases run in order. The runner reads side effect, identity level and idempotency from the operation
/// catalogue (R9), never from the step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Step {
    #[serde(default)]
    pub step_number: u32,
    /// 0-based phase.
    #[serde(default)]
    pub phase: u32,
    /// The name the step's output is stored under; later steps reference it as `$name.field`.
    pub name: String,
    /// `object.verb`.
    pub operation: String,
    /// Variant without braces (`RET-01`, `return`), or none.
    #[serde(default)]
    pub variant: Option<String>,
    #[serde(default)]
    pub modality: Option<String>,
    /// `R` | `W` | `X` | `P` as the design shows it (informational: the catalogue decides).
    #[serde(default)]
    pub side_effect: Option<String>,
    #[serde(default)]
    pub identity_level: Option<String>,
    #[serde(default)]
    pub confirm_back: bool,
    /// The case ends `not_eligible` when this step answers false ([`answered_false`]).
    #[serde(default)]
    pub gate: bool,
    /// Input bindings `arg ← $slot` / `$step.field` / a literal ([`resolve_args`]).
    #[serde(default)]
    pub args: Map<String, Value>,
    /// T6a (F-20): checked by the runner before the call is sent (after the identity check); any false → `denied`
    /// / `precondition_unmet`. B5 fills them; empty = none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<Condition>,
    /// T6a (F-20): checked by the runner after an `ok` answer; any false → the call stays `ok`, the case alerts and
    /// hands over (`postcondition_failed`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub postconditions: Vec<Condition>,
    /// T6a: the agent role that may run the step (`None` = any); another role is refused `not_entitled`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub performed_by: Option<String>,
    /// T6a (F-15): the per-case cap of an X/P step, 1..=20 (`None` = 1). R/W steps have no cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_case: Option<u8>,
}

impl Step {
    /// The step's conditions read case facts ([`CaseFacts`] travel with its call).
    pub fn has_conditions(&self) -> bool {
        !self.preconditions.is_empty() || !self.postconditions.is_empty()
    }
}

impl Step {
    /// A step with no T6a fields (the built-in plans and tests).
    pub fn op(name: &str, operation: &str, variant: Option<&str>, args: Value) -> Step {
        Step {
            step_number: 0,
            phase: 0,
            name: name.to_owned(),
            operation: operation.to_owned(),
            variant: variant.map(str::to_owned),
            modality: None,
            side_effect: None,
            identity_level: None,
            confirm_back: false,
            gate: false,
            args: match args {
                Value::Object(m) => m,
                _ => Map::new(),
            },
            preconditions: Vec::new(),
            postconditions: Vec::new(),
            performed_by: None,
            max_per_case: None,
        }
    }

    /// A designed step (B5's [`ProcedureStep`], a PUBLISHED tenant version) as the orchestrator runs it. Name,
    /// operation, variant, gate, conditions and `performed_by` map 1:1; modality and side effect become the string
    /// their serde form writes (`observe`, `X`); args become string values. A `max_per_case` that does not fit the
    /// runner's `u8` is refused (`Err`), never wrapped: the API caps it at 20, so only a value written past it lands
    /// here. The runner still reads side effect, identity level and idempotency from the catalogue (R9).
    pub fn from_design(s: &ProcedureStep) -> Result<Step, String> {
        let max_per_case = s
            .max_per_case
            .map(u8::try_from)
            .transpose()
            .map_err(|_| format!("step {}: max_per_case is out of range", s.name))?;
        let word =
            |v: serde_json::Result<Value>| v.ok().and_then(|v| v.as_str().map(str::to_owned));
        Ok(Step {
            step_number: s.step_number,
            phase: s.phase,
            name: s.name.clone(),
            operation: s.operation.clone(),
            variant: s.variant.clone(),
            modality: word(serde_json::to_value(s.modality)),
            side_effect: word(serde_json::to_value(s.side_effect)),
            identity_level: Some(s.identity_level.clone()),
            confirm_back: s.confirm_back,
            gate: s.gate,
            args: s
                .args
                .iter()
                .map(|(k, v)| (k.clone(), Value::String(v.clone())))
                .collect(),
            preconditions: s.preconditions.clone(),
            postconditions: s.postconditions.clone(),
            performed_by: s.performed_by.clone(),
            max_per_case,
        })
    }

    /// This step as a gate (`gate: true`).
    pub fn gated(mut self) -> Step {
        self.gate = true;
        self
    }

    /// The catalogue key: `object.verb` or `object.verb{variant}`.
    pub fn op_key(&self) -> String {
        match self
            .variant
            .as_deref()
            .filter(|v| !v.is_empty() && *v != "*")
        {
            Some(v) => format!("{}{{{v}}}", self.operation),
            None => self.operation.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Phase {
    pub steps: Vec<Step>,
}

/// The orchestrator's plan for one case. **First plan wins** (`cb:case.rs:170-176`): it is stored in the case hash
/// once, and the case runs on its `design` even when a publish lands mid-case.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// The procedure version the plan came from; `None` for a plan with no procedure.
    #[serde(default)]
    pub procedure: Option<ProcedureRef>,
    /// The release (prod) or draft revision (test) the plan was read at (D63).
    #[serde(with = "case_design_wire")]
    pub design: CaseDesign,
    pub phases: Vec<Phase>,
    /// T6a (F-20): checked when the last phase finished; any false → the case hands over
    /// (`success_criteria_unmet`). Empty: the case resolves as before.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub success_criteria: Vec<Condition>,
    /// T6a: operations (`object.verb{variant}`) the procedure needs; one routed `not_offered` hands the case over
    /// before its first step.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_requirements: Vec<String>,
    /// T7a (F-40): the case runs the `assistant` role's tool-calling loop (crate::runtime::assistant) instead of
    /// phases (`procedure` `None`, `phases` empty). `None`: a procedure plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<AssistantPlan>,
}

/// T7a: the role and activity an assistant plan runs for (plan `Plan::Assistant{role, activity}`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantPlan {
    /// [`ROLE_ASSISTANT`].
    pub role: String,
    #[serde(default)]
    pub activity: Option<String>,
}

impl Plan {
    /// `name@version` of the procedure, if any.
    pub fn procedure_label(&self) -> Option<String> {
        self.procedure.as_ref().map(ProcedureRef::label)
    }

    /// The step `name` of phase `phase`.
    pub fn step(&self, phase: usize, name: &str) -> Option<&Step> {
        self.phases
            .get(phase)
            .and_then(|p| p.steps.iter().find(|s| s.name == name))
    }
}

/// [`CaseDesign`] on the wire: `{"kind":"live","release_id":…}` or `{"kind":"draft","draft_rev":…}`.
pub mod case_design_wire {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::design::CaseDesign;

    #[derive(Serialize, Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum Wire {
        Live { release_id: Option<String> },
        Draft { draft_rev: i64 },
    }

    pub fn serialize<S: Serializer>(d: &CaseDesign, s: S) -> Result<S::Ok, S::Error> {
        match d {
            CaseDesign::Live { release_id } => Wire::Live {
                release_id: release_id.clone(),
            },
            CaseDesign::Draft { draft_rev } => Wire::Draft {
                draft_rev: *draft_rev,
            },
        }
        .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<CaseDesign, D::Error> {
        Ok(match Wire::deserialize(d)? {
            Wire::Live { release_id } => CaseDesign::Live { release_id },
            Wire::Draft { draft_rev } => CaseDesign::Draft { draft_rev },
        })
    }
}

/// `op.call` (12 §3.2 without `routing_rev`, D63). The payload carries no tenant: the scoped bus stamps the envelope
/// and the runner takes the scope from the stream (`scope_mismatch` guard).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpCall {
    /// `opc_<uuid>`, deterministic per (case, phase, step, attempt) ([`call_id`]): a redelivered dispatch is the
    /// same call, a retry a new one.
    pub call_id: String,
    pub case_id: String,
    pub conversation_id: String,
    /// The plan phase and step name the result belongs to.
    pub phase: usize,
    pub step: String,
    pub attempt: u32,
    /// `procedure` (the orchestrator) or `assistant` (T7a).
    pub agent_role: String,
    #[serde(default)]
    pub activity: Option<String>,
    /// `object.verb`.
    pub operation: String,
    #[serde(default)]
    pub variant: Option<String>,
    /// Canonical input fields (the step's resolved args). Never on the console (K50).
    #[serde(default)]
    pub input: Map<String, Value>,
    /// `{case}:{phase}:{step}`: the same across retries, so a write runs once. `None` for a read (R).
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub env: Env,
    /// The plan's design (D63): `release_id` in prod (`None` = baseline), `draft_rev` in test.
    #[serde(default)]
    pub release_id: Option<String>,
    #[serde(default)]
    pub draft_rev: Option<i64>,
    /// The inbound customer message confirming a W/X/P call (runner step 4).
    #[serde(default)]
    pub confirm_ref: Option<String>,
    /// `procedure@<consumer>`: who sent the call (`operation_call.initiator`).
    pub initiator: String,
    #[serde(default)]
    pub initiator_sub: Option<String>,
    /// The case's identity level when the call was sent.
    #[serde(default)]
    pub identity_level: u8,
    /// The procedure version (`name@version`), if any.
    #[serde(default)]
    pub procedure: Option<String>,
    /// Epoch ms after which the case is failed anyway (the case deadline).
    pub deadline_ms: u64,
    /// T6a: the step's preconditions (runner, after step 3).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<Condition>,
    /// T6a: the step's postconditions (runner, after an `ok` answer).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub postconditions: Vec<Condition>,
    /// T6a: the role the step must be run by (`None` = any).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub performed_by: Option<String>,
    /// T6a (F-15): the step's per-case cap (X/P only; `None` = 1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_case: Option<u8>,
    /// T6a: the case facts the conditions read (empty when the step has none). Case data: never on the console.
    #[serde(default, skip_serializing_if = "CaseFacts::is_empty")]
    pub facts: CaseFacts,
    /// Where the call comes from besides its role: [`PROVENANCE_CONTEXT`] for the customer-context reads (`None` = a
    /// plan step or an assistant tool call). It lifts the activity grant and the ToolPoint gate for a call, and in
    /// exchange admits read operations of the allowlist only; identity, ownership and the audit trail still apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
}

/// The final state of a call (`operation_call.state` without `pending`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpState {
    Ok,
    Error,
    UnknownOutcome,
    Denied,
}

/// `op.result`: the runner's answer to one [`OpCall`] (canonical outcome, doc 11 §3.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OpResult {
    pub call_id: String,
    pub case_id: String,
    pub phase: usize,
    pub step: String,
    pub attempt: u32,
    pub operation: String,
    pub state: OpState,
    /// The canonical outcome or error code (`ok`, `not_found`, `timeout`, `identity_required`, …).
    #[serde(default)]
    pub outcome_code: Option<String>,
    /// The declared outputs of the operation (connector output is untrusted data), minus the fields declared to
    /// write conversation state (A2: the model and the transcript see only that the key is now present). For the
    /// outcome `input_required` it is instead the pause the orchestrator parks on: `{key, prompt, secret, retries}`
    /// (`runner::declared::Ask`; a key and a question, never a value).
    #[serde(default)]
    pub output: Option<Value>,
    /// The runner may send the call again (a new attempt, same idempotency key). Never for `unknown_outcome`.
    #[serde(default)]
    pub retryable: bool,
    #[serde(default)]
    pub route_kind: Option<String>,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    /// The connector instance that ran the call (`None` for a built-in, a stub-less route or a stored outcome).
    #[serde(default)]
    pub instance_id: Option<String>,
}

/// How a case ended (or paused for the customer), and the `agent_case.status` it is written as.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseOutcome {
    /// Every phase ran (status `resolved`).
    Resolved,
    /// A gate step answered false.
    NotEligible,
    /// A step failed for good, or the case passed its deadline.
    Failed,
    /// The case is in the handover queue (staff, the runner's `unknown_outcome`, or no procedure).
    Handover,
    /// A step needs a higher identity level: the interaction daemon asks the customer to verify.
    IdentityRequired,
    /// A W/X/P step needs the customer's confirmation.
    ConfirmationRequired,
    /// The scope was removed (tenant suspended / job offboarded) while the case was open.
    CancelledOffboard,
}

impl CaseOutcome {
    /// The `agent_case.status` value.
    pub fn status(self) -> &'static str {
        match self {
            CaseOutcome::Resolved => "resolved",
            CaseOutcome::NotEligible => "not_eligible",
            CaseOutcome::Failed => "failed",
            CaseOutcome::Handover => "handover",
            CaseOutcome::IdentityRequired => "identity_required",
            CaseOutcome::ConfirmationRequired => "confirmation_required",
            CaseOutcome::CancelledOffboard => "cancelled_offboard",
        }
    }

    /// The status closes the case (`resolved_at`, `duration_ms` are stamped).
    pub fn closes(self) -> bool {
        matches!(
            self,
            CaseOutcome::Resolved
                | CaseOutcome::NotEligible
                | CaseOutcome::Failed
                | CaseOutcome::CancelledOffboard
        )
    }
}

/// `request.resolved`: the case's outcome for the interaction daemon (T8), which composes the reply. First
/// resolution wins: every replica publishes the stored one.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    pub case_id: String,
    pub conversation_id: String,
    pub context_space: String,
    #[serde(default)]
    pub request_type: String,
    pub outcome: CaseOutcome,
    /// A machine code (`deadline_exceeded`, `not_found`, the gate's `ineligible:<id>`, …); never raw error text.
    #[serde(default)]
    pub code: Option<String>,
    /// The reason, never a URL or host. For `not_eligible` it is the platform rule's (`rule.evaluate`) authored,
    /// customer-facing text, which the reply shows; a connector's gate leaves it unset. For `failed` it is staff-facing ("order.read is unavailable", names an operation key): it goes
    /// to the handover item's trigger only, and `reply::compose` shows the customer a cause phrase instead.
    #[serde(default)]
    pub reason: Option<String>,
    /// Every step output by step name (untrusted connector data).
    #[serde(default)]
    pub facts: Map<String, Value>,
    #[serde(default)]
    pub procedure: Option<String>,
    /// The design the case ran on (the plan's; `None` when no plan was made).
    #[serde(default, with = "opt_case_design_wire")]
    pub design: Option<CaseDesign>,
    /// How the case used knowledge (a `knowledge.*` step ran): the reply's grounding (T8, §0 R3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge: Option<KnowledgeUse>,
    /// What the case did, for the test-env reply trace (T8); never sent in prod.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<TraceInput>,
    /// B4 T16a (F-14, acceptance A15): the case's own J21 item was queued while the scope was closed (set by the
    /// orchestrator's `finish` for a `handover` or `failed` case), so the reply names the next open time. Not a
    /// step output: the reply gate's operations (the `facts` keys) never see it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out_of_hours: Option<OutOfHoursNote>,
    /// A write step of the case (catalogue side effect W/X/P) answered `ok`: only then may a generic `resolved`
    /// reply say the request is done (live CS-53: a read-only case was told its refund was done).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub wrote: bool,
    /// A4 `case_per_thread`: how often the case was reopened for a later message of its thread (0 = never). Each
    /// round has its own reply (`reply_ref`); the first round's wire is unchanged.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub round: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

impl Resolution {
    /// The key part that makes this round's reply (and its relay claim) its own: the case id for the first round,
    /// `<case id>:<round>` after a reopen.
    pub fn reply_ref(&self) -> String {
        match self.round {
            0 => self.case_id.clone(),
            n => format!("{}:{n}", self.case_id),
        }
    }
}

/// B4 T16a: an item queued out of hours, as the customer is told (the shape of the `case.create` output of
/// crate::runtime::runner::agent_cases).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutOfHoursNote {
    pub handover_id: String,
    pub next_open: chrono::DateTime<chrono::Utc>,
    /// The next open time on the policy's wall clock, for people: `Mon 26 Oct 09:00 (Europe/Amsterdam)`.
    pub next_open_local: String,
    /// `callback` | `queue` (crate::handover::hours::OutOfHours).
    pub out_of_hours: String,
}

/// `Option<CaseDesign>` on the wire (`null` or [`case_design_wire`]'s form).
pub mod opt_case_design_wire {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::design::CaseDesign;

    #[derive(Serialize, Deserialize)]
    struct W(#[serde(with = "super::case_design_wire")] CaseDesign);

    pub fn serialize<S: Serializer>(d: &Option<CaseDesign>, s: S) -> Result<S::Ok, S::Error> {
        d.clone().map(W).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<CaseDesign>, D::Error> {
        Ok(Option::<W>::deserialize(d)?.map(|w| w.0))
    }
}

/// `opc_<uuid>` of (case, phase, step, attempt): the same inputs give the same id (a redelivered dispatch is one
/// call), and it matches `operation_call.call_id`'s CHECK.
pub fn call_id(case_id: &str, phase: usize, step: &str, attempt: u32) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{case_id}\u{1f}{phase}\u{1f}{step}\u{1f}{attempt}"));
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    format!("opc_{}", uuid::Uuid::from_bytes(bytes).hyphenated())
}

/// `{case}:{phase}:{step}`: the idempotency key of a write, the same across retries.
pub fn idempotency_key(case_id: &str, phase: usize, step: &str) -> String {
    format!("{case_id}:{phase}:{step}")
}

/// Replace `"$…"` strings with values from the case context ([`lookup`]); anything else is kept as is.
pub fn resolve_args(args: &Map<String, Value>, ctx: &Map<String, Value>) -> Map<String, Value> {
    args.iter()
        .map(|(k, v)| {
            let resolved = match v.as_str().and_then(|s| s.strip_prefix('$')) {
                Some(path) => lookup(ctx, path),
                None => v.clone(),
            };
            (k.clone(), resolved)
        })
        .collect()
}

/// `request.<slot>` → the request's slot (and `customer_id`); `<step>.<path>` → that step's output; a bare `<name>`
/// → the slot `name`, else the whole output of step `name`. Anything missing is `null`.
///
/// A path segment may be `key[field=value]` (one filter form): `key` is a list, and the segment selects its FIRST
/// element whose `field` equals `value` (`$name` = [`lookup`] of `name`, else a literal); no such element is a
/// missing path, so `null`. `$payments.rows[order_id=$order_id].payment_id` is the payment of this order (`rows`: the step
/// output's equal-length array outputs zipped into objects, see `conditions::rows_of`).
pub fn lookup(ctx: &Map<String, Value>, path: &str) -> Value {
    let mut parts = path.split('.');
    let Some(first) = parts.next() else {
        return Value::Null;
    };
    let rest: Vec<&str> = parts.collect();
    let start = if rest.is_empty() {
        ctx.get("request")
            .and_then(|r| r.get(first))
            .or_else(|| ctx.get(first))
    } else {
        ctx.get(first)
    };
    let Some(mut cur) = start.cloned() else {
        return Value::Null;
    };
    for p in rest {
        let next = match parse_filter(p) {
            Some(Ok(f)) => f
                .resolve(|name| Some(lookup(ctx, name)))
                .and_then(|want| pick(&cur, &f, &want)),
            Some(Err(())) => None,
            None => child(&cur, p),
        };
        match next {
            Some(v) => cur = v,
            None => return Value::Null,
        }
    }
    cur
}

/// A gate step's output says no: `eligible: false`, `outcome` `ineligible` / `not_eligible` / `false`, or a bare
/// `false`. Anything else (including no output) lets the case go on.
pub fn answered_false(output: Option<&Value>) -> bool {
    let Some(v) = output else {
        return false;
    };
    if v == &Value::Bool(false) {
        return true;
    }
    if v.get("eligible") == Some(&Value::Bool(false)) {
        return true;
    }
    match v.get("outcome") {
        Some(Value::Bool(false)) => true,
        Some(Value::String(s)) => matches!(s.as_str(), "ineligible" | "not_eligible" | "false"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn args_resolve_slots_step_outputs_and_literals() {
        let ctx = obj(json!({
            "request": {"order_id": "ORD-1", "customer_id": "C-1"},
            "order": {"status": "delivered", "lines": [{"sku": "S"}], "nested": {"x": 3}},
        }));
        let args = obj(json!({
            "a": "$order_id", "b": "$request.customer_id", "c": "$order.status", "d": "$order.nested.x",
            "e": "literal", "f": "$missing.y", "g": "$order", "h": 7
        }));
        assert_eq!(
            Value::Object(resolve_args(&args, &ctx)),
            json!({"a": "ORD-1", "b": "C-1", "c": "delivered", "d": 3, "e": "literal", "f": null,
                   "g": {"status": "delivered", "lines": [{"sku": "S"}], "nested": {"x": 3}}, "h": 7})
        );
    }

    #[test]
    fn a_filter_selects_the_first_matching_element_or_nothing() {
        let ctx = obj(json!({
            "request": {"order_id": "ORD-2", "customer_id": "C-1"},
            "payments": {"items": [
                {"order_id": "ORD-1", "payment_id": "P-1", "status": "refunded"},
                {"order_id": "ORD-2", "payment_id": "P-2", "status": "captured", "card": {"last4": "4242"}, "n": 8},
                {"order_id": "ORD-2", "payment_id": "P-3", "status": "pending"}
            ]},
        }));
        let args = obj(json!({
            "ref_slot": "$payments.items[order_id=$order_id].payment_id",
            "ref_request": "$payments.items[order_id=$customer_id].payment_id",
            "literal": "$payments.items[order_id=ORD-1].status",
            "number": "$payments.items[n=8].payment_id",
            "nested": "$payments.items[order_id=$order_id].card.last4",
            "whole": "$payments.items[order_id=$order_id]",
            "no_match": "$payments.items[order_id=ORD-9].payment_id",
            "no_field": "$payments.items[nope=ORD-2].payment_id",
            "no_ref": "$payments.items[order_id=$missing].payment_id",
            "no_list": "$payments.rows[order_id=ORD-2].payment_id",
            "no_key": "$other.items[order_id=ORD-2].payment_id",
            "missing_after": "$payments.items[order_id=$order_id].nope",
            "bad": "$payments.items[order_id=].payment_id",
            "plain": "$payments.items",
        }));
        let out = resolve_args(&args, &ctx);
        let get = |k: &str| out.get(k).cloned().unwrap();
        assert_eq!(
            get("ref_slot"),
            json!("P-2"),
            "the first row of this order, not of the list"
        );
        assert_eq!(
            get("ref_request"),
            json!(null),
            "C-1 is no order of any payment"
        );
        assert_eq!(get("literal"), json!("refunded"));
        assert_eq!(get("number"), json!("P-2"));
        assert_eq!(get("nested"), json!("4242"));
        assert_eq!(get("whole")["payment_id"], json!("P-2"));
        for k in [
            "no_match",
            "no_field",
            "no_ref",
            "no_list",
            "no_key",
            "missing_after",
            "bad",
        ] {
            assert_eq!(get(k), json!(null), "{k}");
        }
        assert_eq!(
            get("plain").as_array().map(Vec::len),
            Some(3),
            "a plain path is unchanged"
        );
    }

    #[test]
    fn rows_zip_the_array_outputs_of_a_step() {
        let ctx = obj(json!({
            "request": {"order_id": "ORD-2"},
            "payments": {
                "order_id": ["ORD-1", "ORD-2"],
                "payment_id": ["P-1", "P-2"],
                "customer_id": "C-1"
            },
            "ragged": {"order_id": ["ORD-2"], "payment_id": ["P-1", "P-2"]},
        }));
        let args = obj(json!({
            "hit": "$payments.rows[order_id=$order_id].payment_id",
            "scalar_not_a_column": "$payments.rows[order_id=$order_id].customer_id",
            "ragged": "$ragged.rows[order_id=$order_id].payment_id",
            "none": "$payments.rows[order_id=ORD-9].payment_id",
        }));
        let out = resolve_args(&args, &ctx);
        assert_eq!(out["hit"], json!("P-2"));
        assert_eq!(out["scalar_not_a_column"], json!(null));
        assert_eq!(out["ragged"], json!(null), "unequal lengths: no rows");
        assert_eq!(out["none"], json!(null));
    }

    #[test]
    fn gate_answers() {
        for no in [
            json!(false),
            json!({"eligible": false}),
            json!({"outcome": "ineligible", "reason": "too late"}),
            json!({"outcome": "not_eligible"}),
            json!({"outcome": false}),
        ] {
            assert!(answered_false(Some(&no)), "{no}");
        }
        for yes in [
            json!({"outcome": "eligible"}),
            json!({"outcome": "escalate"}),
            json!({"eligible": true}),
            json!({}),
            json!(null),
        ] {
            assert!(!answered_false(Some(&yes)), "{yes}");
        }
        assert!(!answered_false(None));
    }

    #[test]
    fn call_ids_are_deterministic_and_match_the_column_check() {
        let a = call_id("CS-1", 0, "order", 1);
        assert_eq!(a, call_id("CS-1", 0, "order", 1));
        assert_ne!(a, call_id("CS-1", 0, "order", 2));
        assert_ne!(a, call_id("CS-1", 1, "order", 1));
        let hex = a.strip_prefix("opc_").unwrap();
        let groups: Vec<usize> = hex.split('-').map(str::len).collect();
        assert_eq!(groups, [8, 4, 4, 4, 12]);
        assert!(
            hex.chars()
                .all(|c| c == '-' || c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
    }

    #[test]
    fn plans_keep_their_design_on_the_wire() {
        for design in [
            CaseDesign::Live {
                release_id: Some("r-2".into()),
            },
            CaseDesign::Live { release_id: None },
            CaseDesign::Draft { draft_rev: 7 },
        ] {
            let plan = Plan {
                procedure: Some(ProcedureRef {
                    name: "return_request".into(),
                    version: 2,
                }),
                design: design.clone(),
                phases: vec![Phase {
                    steps: vec![Step::op(
                        "order",
                        "order.read",
                        None,
                        json!({"order_id": "$order_id"}),
                    )],
                }],
                success_criteria: Vec::new(),
                tool_requirements: Vec::new(),
                assistant: None,
            };
            let wire = serde_json::to_value(&plan).unwrap();
            assert!(
                wire.get("assistant").is_none(),
                "a procedure plan keeps its old shape"
            );
            assert_eq!(serde_json::from_value::<Plan>(wire).unwrap(), plan);
            let assist = Plan {
                procedure: None,
                phases: Vec::new(),
                assistant: Some(AssistantPlan {
                    role: ROLE_ASSISTANT.into(),
                    activity: Some("support:track_refund".into()),
                }),
                ..plan
            };
            let wire = serde_json::to_value(&assist).unwrap();
            assert_eq!(wire["assistant"]["role"], "assistant");
            assert_eq!(serde_json::from_value::<Plan>(wire).unwrap(), assist);
        }
        let r = Resolution {
            case_id: "CS-1".into(),
            conversation_id: "conv-1".into(),
            context_space: "retail:order-support:returns".into(),
            request_type: "return".into(),
            outcome: CaseOutcome::Resolved,
            code: None,
            reason: None,
            facts: Map::new(),
            procedure: None,
            design: Some(CaseDesign::Draft { draft_rev: 3 }),
            knowledge: None,
            trace: None,
            out_of_hours: None,
            wrote: false,
            round: 0,
        };
        let wire = serde_json::to_value(&r).unwrap();
        assert!(
            wire.get("out_of_hours").is_none() && wire.get("wrote").is_none(),
            "the B4 T16a / CS-53 fields are omitted when unset (an older reader sees the old wire)"
        );
        assert_eq!(wire["design"], json!({"kind": "draft", "draft_rev": 3}));
        assert_eq!(serde_json::from_value::<Resolution>(wire).unwrap(), r);
    }

    #[test]
    fn steps_without_t6a_fields_parse_and_serialise_unchanged() {
        let ui = json!({
            "step_number": 1, "phase": 0, "name": "order", "operation": "order.read", "variant": null,
            "modality": "read", "side_effect": "R", "identity_level": "identified", "confirm_back": false,
            "gate": false, "args": {"order_id": "$order_id"}
        });
        let s: Step = serde_json::from_value(ui.clone()).unwrap();
        assert!(s.preconditions.is_empty() && s.max_per_case.is_none());
        assert_eq!(serde_json::to_value(&s).unwrap(), ui);
        assert_eq!(s.op_key(), "order.read");
        let v = Step::op("rule", "rule.evaluate", Some("RET-01"), json!({}));
        assert_eq!(v.op_key(), "rule.evaluate{RET-01}");
    }

    #[test]
    fn a_designed_step_maps_to_a_runtime_step_and_refuses_an_oversized_cap() {
        let mut d: ProcedureStep = serde_json::from_value(json!({
            "step_number": 2, "phase": 1, "name": "refund", "operation": "payment.refund", "variant": "full",
            "modality": "do", "side_effect": "X", "identity_level": "2", "confirm_back": true, "gate": false,
            "args": {"order_id": "$order_id"},
            "preconditions": [{"id": "found", "fact": "steps.order.outcome", "op": "eq", "value": "ok"}],
            "performed_by": "procedure", "max_per_case": 2
        }))
        .unwrap();
        let s = Step::from_design(&d).unwrap();
        assert_eq!((s.step_number, s.phase), (2, 1));
        assert_eq!(s.op_key(), "payment.refund{full}");
        assert_eq!(s.modality.as_deref(), Some("do"));
        assert_eq!(s.side_effect.as_deref(), Some("X"));
        assert_eq!(s.identity_level.as_deref(), Some("2"));
        assert!(s.confirm_back && !s.gate);
        assert_eq!(s.args, obj(json!({"order_id": "$order_id"})));
        assert_eq!(s.preconditions[0].id, "found");
        assert_eq!(
            (s.performed_by.as_deref(), s.max_per_case),
            (Some("procedure"), Some(2))
        );
        d.max_per_case = Some(256);
        assert!(Step::from_design(&d).is_err(), "never wrapped to 0");
    }

    #[test]
    fn request_topics_keep_the_context_space_prefix() {
        let req: QualifiedRequest = serde_json::from_value(json!({
            "case_id": "CS-1", "conversation_id": "conv-1", "area": "order-support",
            "context_space": "retail:order-support:returns"
        }))
        .unwrap();
        assert_eq!(
            req.topic(REQUEST_RESOLVED),
            "order-support.returns.request.resolved"
        );
        assert_eq!(
            super::super::bus::event_of(&req.topic(REQUEST_QUALIFIED)),
            Some(REQUEST_QUALIFIED)
        );
    }
}
