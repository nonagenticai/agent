//! A2 in the runner: declared tool inputs, the producer chain, the ask-user pause and state-mapped outputs
//! ([`crate::runtime::declared`] holds the pure data model and planner; this is the IO half).
//!
//! | Where | What |
//! |---|---|
//! | step 3 | [`required_keys`]: the keys the op needs in conversation state (`policy.requires` when the job declares one, else `subject` when the catalogue level is 1 or above); a missing one is the refusal `state_required` (detail: the key names) |
//! | step 3c | [`Runner::resolve_inputs`]: each `state` input of the op's policy is read from state and put into the request; a missing key plans the producer chain ([`plan_chain`]) and runs each producer as a `resolve` sub-call through the whole runner (`Runner::handle`, same case and conversation, provenance [`PROVENANCE_RESOLVE`], confirmation skipped); a missing `ask_user` key answers `input_required` |
//! | after step 10b | [`Runner::store_outputs`]: the output fields declared to write state are taken out of the answer and stored (`value` or sealed `secret`, optional TTL, optional carry-over to the subject); the model sees only that the key is now present |
//!
//! Outcome codes: `state_required` (denied), `input_required` (error, not a handover: the orchestrator parks the case
//! on the [`Ask`] the result's `output` carries), `input_unresolved` (a producer failed or a key has no source; the
//! policy's `on_failure`: `handover` -> error, the orchestrator hands over as `tool_failed`; `refuse` -> denied),
//! `secret_store_unavailable` (a secret cannot be sealed: no keyring), `state_unavailable`.
//!
//! Secret values and inputs/outputs never reach a log line, an audit row, a case event or a model message: only key
//! names, op keys and codes do. A resolved `secret` input is read ([`state::get`]) into the in-process request only.
use std::collections::BTreeMap;

use anyhow::Result;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::Runner;
use super::ops::{OpOutcome, OpRequest, OpState, failed};
use crate::connectors::opkey::parse_op_key;
use crate::conversations::state::{self, StateValue};
use crate::design::JobRef;
use crate::runtime::declared::{
    ChainStep, InputDecl, OutputDecl, OutputKind, Producer, StateFrom, plan_chain,
};
use crate::runtime::design::RuntimeDesign;
use crate::runtime::domain::{self, OpCall, PROVENANCE_RESOLVE};

/// Step 3: a key the op requires is not in conversation state.
pub const STATE_REQUIRED: &str = "state_required";
/// A producer (`resolve` sub-call) whose operation is irreversible (X) or a payment (P): producers run without the
/// user's confirmation, so only reads and reversible writes may be one.
pub const PRODUCER_NOT_ALLOWED: &str = "producer_not_allowed";
/// Step 3c: the op needs the user to supply a state key; the case parks.
pub const INPUT_REQUIRED: &str = "input_required";
/// Step 3c: a declared input could not be filled (a producer failed, no source).
pub const INPUT_UNRESOLVED: &str = "input_unresolved";
/// A secret output or answer cannot be sealed: the runner has no keyring.
pub const SECRET_STORE_UNAVAILABLE: &str = "secret_store_unavailable";
/// A declared output could not be written to state.
pub const STATE_UNAVAILABLE: &str = "state_unavailable";

/// The pause an `ask_user` input needs: what to ask and how to treat the answer. A key and a question, never a value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ask {
    pub key: String,
    pub prompt: String,
    #[serde(default)]
    pub secret: bool,
    /// Empty answers tolerated before the policy's `on_failure` applies.
    #[serde(default)]
    pub retries: u8,
}

impl Ask {
    /// The `output` of the `input_required` result.
    pub fn to_output(&self) -> Map<String, Value> {
        match serde_json::to_value(self) {
            Ok(Value::Object(m)) => m,
            _ => Map::new(),
        }
    }

    /// Read the pause back from an `input_required` result's `output`.
    pub fn from_output(v: &Value) -> Option<Ask> {
        serde_json::from_value(v.clone()).ok()
    }
}

/// The `chat:{conversation}` hash field a parked `input_required` case waits in.
pub const INPUT_FIELD: &str = "input";
/// How long a parked question stays answerable (30 minutes).
pub const INPUT_TTL_MS: i64 = 30 * 60 * 1000;

/// A question waiting for the user's answer (`chat:{conversation}` field [`INPUT_FIELD`], written by the orchestrator
/// when a call answers `input_required`, read and cleared by the next turn of the drain). The next message is the
/// value of `key`: stored as state (sealed when `secret`) and the request re-run as a new case with `slots`. Holds the
/// question and bookkeeping, never a value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputPause {
    pub case_id: String,
    pub context_space: String,
    /// The request's other slots (no system slot, no hint).
    pub slots: BTreeMap<String, String>,
    /// The inbound message the parked request was qualified from.
    #[serde(default)]
    pub message_id: Option<String>,
    pub key: String,
    pub prompt: String,
    #[serde(default)]
    pub secret: bool,
    #[serde(default)]
    pub retries: u8,
    /// Empty answers so far.
    #[serde(default)]
    pub attempts: u8,
    /// Epoch ms after which the question is stale.
    pub until_ms: i64,
    /// B2: the question is an MCP task's (crate::runtime::task_relay), not a declared input: the reply goes to the
    /// task, and nothing is re-run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskAsk>,
}

/// The MCP task question a conversation waits on: which task, which of its `inputRequests`, the schema field this
/// reply fills, and the fields given so far (none secret: a secret field is sealed as state until it is sent).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskAsk {
    pub task_id: String,
    pub question: String,
    pub field: String,
    #[serde(default)]
    pub collected: BTreeMap<String, String>,
}

impl InputPause {
    /// Another empty answer: `Some(pause)` to ask again, `None` when the retries are used up.
    pub fn retry(mut self) -> Option<InputPause> {
        self.attempts = self.attempts.saturating_add(1);
        (self.attempts <= self.retries).then_some(self)
    }

    /// Still answerable at `now_ms`.
    pub fn fresh(&self, now_ms: i64) -> bool {
        self.until_ms >= now_ms
    }
}

/// Step 3: the state keys `op` needs. A declaration's `requires` when the job declares one for the op (empty =
/// nothing is required, e.g. a worker job's board ops), else `subject` when the catalogue level is 1 or above
/// (`identified`).
pub fn required_keys(
    policy: Option<&crate::runtime::declared::ToolPolicy>,
    identified: bool,
) -> Vec<String> {
    match policy {
        Some(p) => {
            let mut keys = p.requires.clone();
            keys.sort();
            keys.dedup();
            keys
        }
        None if identified => vec![state::SUBJECT.to_owned()],
        None => Vec::new(),
    }
}

/// The keys of `required` that `have` does not report present, in order.
pub fn missing_keys(required: &[String], have: &dyn Fn(&str) -> bool) -> Vec<String> {
    required.iter().filter(|k| !have(k)).cloned().collect()
}

/// One declared output taken out of an answer, to be stored. Its `Debug` leaves the value out.
pub struct Produced {
    pub key: String,
    pub secret: bool,
    pub ttl_s: Option<u32>,
    pub persist_for_subject: bool,
    value: String,
}

impl std::fmt::Debug for Produced {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Produced")
            .field("key", &self.key)
            .field("secret", &self.secret)
            .finish_non_exhaustive()
    }
}

impl Produced {
    pub fn value(&self) -> &str {
        &self.value
    }
}

/// Remove every field `decls` maps to a state key from `output` and return what is to be stored. A null or blank
/// field is removed and stores nothing (the key stays absent); a non-string field is stored as its JSON text.
pub fn take_outputs(
    output: &mut Map<String, Value>,
    decls: &BTreeMap<String, OutputDecl>,
) -> Vec<Produced> {
    let mut taken = Vec::new();
    for (field, d) in decls {
        let Some(v) = output.remove(field) else {
            continue;
        };
        let text = match v {
            Value::Null => continue,
            Value::String(s) => s,
            other => other.to_string(),
        };
        if text.trim().is_empty() {
            continue;
        }
        taken.push(Produced {
            key: d.key.clone(),
            secret: d.kind == OutputKind::Secret,
            ttl_s: d.ttl_s,
            persist_for_subject: d.persist_for_subject,
            value: text,
        });
    }
    taken
}

/// Add every output field of `decls` the request does not already ask the connector for.
pub fn request_outputs(outputs: &mut Vec<String>, decls: &BTreeMap<String, OutputDecl>) {
    for f in decls.keys() {
        if !outputs.iter().any(|o| o == f) {
            outputs.push(f.clone());
        }
    }
}

/// The producer call `parent` makes for `op` (an op key) with `input`: same case and conversation, a fresh call id,
/// role/initiator `runtime`, no confirmation, provenance [`PROVENANCE_RESOLVE`].
pub fn resolve_call(parent: &OpCall, op: &str, input: Map<String, Value>) -> OpCall {
    let r = parse_op_key(op);
    let variant = (!r.variant.is_empty() && r.variant != "*").then(|| r.variant.clone());
    OpCall {
        call_id: format!("opc_{}", uuid::Uuid::new_v4().hyphenated()),
        step: format!("resolve:{op}"),
        attempt: 1,
        agent_role: "runtime".to_owned(),
        operation: format!("{}.{}", r.object, r.verb),
        variant,
        input,
        idempotency_key: None,
        confirm_ref: None,
        initiator: "runtime".to_owned(),
        initiator_sub: None,
        preconditions: Vec::new(),
        postconditions: Vec::new(),
        performed_by: None,
        max_per_case: None,
        facts: Default::default(),
        provenance: Some(PROVENANCE_RESOLVE.to_owned()),
        ..parent.clone()
    }
}

/// How the producer chain ended.
enum ChainEnd {
    Done,
    Ask(Ask),
    Failed(String),
}

/// What step 3c made of the call's declared inputs.
pub(super) enum Resolved {
    /// Every state input is in the request (or the op declares none).
    Ready,
    /// A key must come from the user: answer `input_required`.
    Ask(Ask),
    /// An input cannot be filled; the detail names a key or op, never a value.
    Failed(String),
}

impl Runner {
    /// `handle` as a boxed future: the recursion of a `resolve` sub-call.
    fn handle_boxed<'a>(&'a self, call: &'a OpCall) -> BoxFuture<'a, Result<domain::OpResult>> {
        Box::pin(self.handle(call))
    }

    /// Read `key` of the conversation as text; a secret is opened (runtime use only). `None` = absent.
    async fn read_state(
        &self,
        call: &OpCall,
        have: &BTreeMap<String, StateValue>,
        key: &str,
    ) -> Result<Option<String>> {
        match have.get(key) {
            None => Ok(None),
            Some(StateValue::Value(v)) => Ok(Some(v.clone())),
            Some(StateValue::Secret) => {
                let Some(kr) = self.st.provider_keyring.as_deref() else {
                    anyhow::bail!("{SECRET_STORE_UNAVAILABLE}");
                };
                let (mut tx, _) = self.begin().await?;
                let v = state::get(&mut tx, kr, &call.conversation_id, key).await?;
                tx.commit().await?;
                Ok(v)
            }
        }
    }

    async fn load_state(&self, call: &OpCall) -> Result<BTreeMap<String, StateValue>> {
        let (mut tx, _) = self.begin().await?;
        let have = state::load(&mut tx, &call.conversation_id).await?;
        tx.commit().await?;
        Ok(have)
    }

    /// The inputs of producer `p` read from state; `Err(key)` = a state input has no value.
    async fn producer_input(
        &self,
        call: &OpCall,
        p: &Producer,
    ) -> Result<std::result::Result<Map<String, Value>, String>> {
        let have = self.load_state(call).await?;
        let mut input = Map::new();
        for (name, decl) in &p.inputs {
            let InputDecl::State { key, .. } = decl else {
                continue;
            };
            match self.read_state(call, &have, key).await? {
                Some(v) => {
                    input.insert(name.clone(), Value::String(v));
                }
                None => return Ok(Err(key.clone())),
            }
        }
        Ok(Ok(input))
    }

    /// Run the planned producers in order, each as a `resolve` sub-call through [`Runner::handle`].
    async fn run_chain(
        &self,
        design: &RuntimeDesign,
        call: &OpCall,
        steps: &[ChainStep],
    ) -> Result<ChainEnd> {
        for step in steps {
            match step {
                ChainStep::AskUser {
                    key,
                    prompt,
                    secret,
                    retries,
                } => {
                    return Ok(ChainEnd::Ask(Ask {
                        key: key.clone(),
                        prompt: prompt.clone(),
                        secret: *secret,
                        retries: *retries,
                    }));
                }
                ChainStep::Run { op, writes } => {
                    let Some(producer) = design.producers.producer_op(op) else {
                        return Ok(ChainEnd::Failed(format!("{op}: no producer")));
                    };
                    let input = match self.producer_input(call, producer).await? {
                        Ok(i) => i,
                        Err(key) => return Ok(ChainEnd::Failed(format!("{op}: {key} missing"))),
                    };
                    let sub = resolve_call(call, op, input);
                    let res = self.handle_boxed(&sub).await?;
                    if res.state != domain::OpState::Ok || res.outcome_code.is_some() {
                        let code = res.outcome_code.as_deref().unwrap_or("error");
                        return Ok(ChainEnd::Failed(format!("{op}: {code}")));
                    }
                    let have = self.load_state(call).await?;
                    if let Some(k) = writes.iter().find(|k| !have.contains_key(*k)) {
                        return Ok(ChainEnd::Failed(format!("{op}: wrote no {k}")));
                    }
                }
            }
        }
        Ok(ChainEnd::Done)
    }

    /// Step 3c: fill the `state` inputs of `op_key`'s policy into `req.inputs` (a value the model sent for such an
    /// input is dropped), running the producer chain for missing keys. A `resolve` sub-call never resolves further.
    pub(super) async fn resolve_inputs(
        &self,
        design: &RuntimeDesign,
        call: &OpCall,
        op_key: &str,
        req: &mut OpRequest,
    ) -> Result<Resolved> {
        let Some(policy) = design.policy(op_key) else {
            return Ok(Resolved::Ready);
        };
        if call.provenance.as_deref() == Some(PROVENANCE_RESOLVE) {
            return Ok(Resolved::Ready);
        }
        let wanted: Vec<(&String, &String, &StateFrom)> = policy
            .inputs
            .iter()
            .filter_map(|(name, d)| match d {
                InputDecl::State { key, from } => Some((name, key, from)),
                InputDecl::Model => None,
            })
            .collect();
        if wanted.is_empty() && policy.requires.is_empty() {
            return Ok(Resolved::Ready);
        }
        for (name, ..) in &wanted {
            req.inputs.remove(*name);
        }
        let mut have = self.load_state(call).await?;
        let absent = |have: &BTreeMap<String, StateValue>| -> Vec<(String, StateFrom)> {
            wanted
                .iter()
                .filter(|(_, k, _)| !have.contains_key(*k))
                .map(|(_, k, f)| ((*k).clone(), (*f).clone()))
                .collect()
        };
        let missing = absent(&have);
        // `requires` keys a producer makes join the chain (step 3 let them through for this)
        let required: Vec<&str> = policy
            .requires
            .iter()
            .map(String::as_str)
            .filter(|k| !have.contains_key(*k) && design.producers.producer_of(k).is_some())
            .collect();
        if !missing.is_empty() || !required.is_empty() {
            let mut chain_keys: Vec<&str> = missing
                .iter()
                .filter(|(_, f)| !matches!(f, StateFrom::AskUser { .. }))
                .map(|(k, _)| k.as_str())
                .collect();
            for k in required {
                if !chain_keys.contains(&k) {
                    chain_keys.push(k);
                }
            }
            if !chain_keys.is_empty() {
                let present = |k: &str| have.contains_key(k);
                let steps = match plan_chain(&chain_keys, &present, &design.producers) {
                    Ok(s) => s,
                    Err(e) => return Ok(Resolved::Failed(e.code().to_owned())),
                };
                match self.run_chain(design, call, &steps).await? {
                    ChainEnd::Done => {}
                    ChainEnd::Ask(a) => return Ok(Resolved::Ask(a)),
                    ChainEnd::Failed(d) => return Ok(Resolved::Failed(d)),
                }
                have = self.load_state(call).await?;
            }
            for (key, from) in absent(&have) {
                if let StateFrom::AskUser {
                    ask_user,
                    secret,
                    retries,
                } = from
                {
                    return Ok(Resolved::Ask(Ask {
                        key,
                        prompt: ask_user,
                        secret,
                        retries,
                    }));
                }
            }
        }
        for (name, key, _) in &wanted {
            match self.read_state(call, &have, key).await {
                Ok(Some(v)) => {
                    req.inputs.insert((*name).clone(), Value::String(v));
                }
                Ok(None) => return Ok(Resolved::Failed(format!("{key} missing"))),
                Err(e) if e.to_string() == SECRET_STORE_UNAVAILABLE => {
                    return Ok(Resolved::Failed(SECRET_STORE_UNAVAILABLE.to_owned()));
                }
                Err(_) => return Ok(Resolved::Failed(format!("{key} unreadable"))),
            }
        }
        Ok(Resolved::Ready)
    }

    /// After an `ok` answer: take the fields declared to write state out of `out.output` (always, so no stored value
    /// ever reaches the result, the `operation_call` object change or the model) and store them. A call that already
    /// carries an outcome code (a failed postcondition) stores nothing. A store that fails turns the answer into the
    /// error `secret_store_unavailable` / `state_unavailable` (detail: the key).
    pub(super) async fn store_outputs(
        &self,
        design: &RuntimeDesign,
        call: &OpCall,
        job: &JobRef,
        op_key: &str,
        out: &mut OpOutcome,
    ) {
        let Some(decls) = design.output_decls(op_key) else {
            return;
        };
        if out.state != OpState::Ok {
            return;
        }
        let produced = take_outputs(&mut out.output, decls);
        if produced.is_empty() || out.outcome_code.is_some() {
            return;
        }
        // A3 (owner 2026-11-01): a step of an attached IAM procedure may establish the person's identity. The
        // procedure and its declared strength are on the design by this op key.
        let identity = design
            .identity
            .get(op_key)
            .or_else(|| {
                op_key
                    .split_once('{')
                    .and_then(|(base, _)| design.identity.get(base))
            })
            .copied();
        if let Err((code, key)) = self
            .write_produced(call, job, op_key, identity, &produced)
            .await
        {
            tracing::warn!(call_id = %call.call_id, op = op_key, key = %key, code, "declared output not stored");
            let mut f = failed(code, format!("state key {key}"));
            f.instance_id = out.instance_id.take();
            f.template = out.template.take();
            f.latency_ms = out.latency_ms;
            *out = f;
        }
    }

    /// Write `produced` in one transaction; `Err((code, key))` on the first that cannot be stored. The person's
    /// identity (`subject`/`email`/`name` of an IAM procedure's step) goes through [`upgrade_identity`] in the same
    /// transaction, so the token a sign-in wrote and the identity it established land together or not at all.
    async fn write_produced(
        &self,
        call: &OpCall,
        job: &JobRef,
        op_key: &str,
        identity: Option<crate::runtime::design::IdentityProducer>,
        produced: &[Produced],
    ) -> std::result::Result<(), (&'static str, String)> {
        let first = produced.first().map_or_else(String::new, |p| p.key.clone());
        let by = format!("tool:{op_key}");
        let keyring = self.st.provider_keyring.as_deref();
        let mut tx =
            match crate::conversations::store::begin_job(&self.st.db, self.scope.tenant, job.id)
                .await
            {
                Ok(tx) => tx,
                Err(_) => return Err((STATE_UNAVAILABLE, first)),
            };
        let (ident, ordinary): (Vec<&Produced>, Vec<&Produced>) = produced
            .iter()
            .partition(|p| crate::runtime::declared::IDENTITY_KEYS.contains(&p.key.as_str()));
        if !ident.is_empty() {
            // a policy can never declare these (validate_policy refuses them): this is a stored declaration the
            // platform cannot honour, never a way for a tool to become a person
            let Some(ip) = identity else {
                return Err((STATE_UNAVAILABLE, first));
            };
            let field = |key: &str| produced.iter().find(|p| p.key == key).map(Produced::value);
            let write = crate::iam::IdentityWrite {
                subject: field(state::SUBJECT),
                email: field(state::EMAIL),
                name: field(state::NAME),
            };
            crate::iam::upgrade_identity(
                &mut tx,
                crate::iam::IdentityUpgrade {
                    conversation: &call.conversation_id,
                    write,
                    procedure: ip.procedure,
                    strength: ip.strength,
                    step: op_key,
                    actor_role: &call.agent_role,
                    job_key: &job.key,
                },
            )
            .await
            .map_err(|code| (code, first.clone()))?;
        }
        for p in ordinary {
            let ttl = p.ttl_s.map(i64::from);
            let stored = if p.secret {
                match keyring {
                    Some(kr) => {
                        state::put_secret(
                            &mut tx,
                            kr,
                            &call.conversation_id,
                            &p.key,
                            p.value(),
                            &by,
                            ttl,
                        )
                        .await
                    }
                    None => return Err((SECRET_STORE_UNAVAILABLE, p.key.clone())),
                }
            } else {
                state::put_value(&mut tx, &call.conversation_id, &p.key, p.value(), &by, ttl).await
            };
            if stored.is_err() {
                return Err((STATE_UNAVAILABLE, p.key.clone()));
            }
            if p.persist_for_subject
                && state::remember_for_subject(&mut tx, keyring, &call.conversation_id, &p.key)
                    .await
                    .is_err()
            {
                return Err((STATE_UNAVAILABLE, p.key.clone()));
            }
        }
        tx.commit().await.map_err(|_| (STATE_UNAVAILABLE, first))
    }
}

#[cfg(test)]
mod tests {
    use super::super::op_key_of;
    use super::*;
    use crate::runtime::declared::ToolPolicy;
    use serde_json::json;

    fn decl(key: &str, kind: OutputKind) -> OutputDecl {
        OutputDecl {
            key: key.into(),
            kind,
            ttl_s: Some(60),
            persist_for_subject: false,
        }
    }

    #[test]
    fn required_keys_follow_the_declaration_else_the_level() {
        assert_eq!(required_keys(None, true), vec!["subject"]);
        assert!(required_keys(None, false).is_empty());
        let p = ToolPolicy {
            requires: vec!["b".into(), "a".into(), "b".into()],
            ..ToolPolicy::default()
        };
        assert_eq!(required_keys(Some(&p), true), vec!["a", "b"]);
        // a declaration with no `requires` overrides the level default: a worker job's board op needs nothing
        assert!(required_keys(Some(&ToolPolicy::default()), true).is_empty());
    }

    #[test]
    fn missing_keys_keep_order_and_skip_present_ones() {
        let req = vec![
            "subject".to_owned(),
            "auth_token".to_owned(),
            "x".to_owned(),
        ];
        let have = |k: &str| k == "auth_token";
        assert_eq!(missing_keys(&req, &have), vec!["subject", "x"]);
    }

    #[test]
    fn state_mapped_fields_are_taken_out_of_the_answer() {
        let mut out =
            json!({"token": "s3cret", "name": "Ada", "n": 7, "blank": " ", "nothing": null})
                .as_object()
                .cloned()
                .unwrap();
        let decls = BTreeMap::from([
            ("token".to_owned(), decl("auth_token", OutputKind::Secret)),
            ("n".to_owned(), decl("count", OutputKind::Value)),
            ("blank".to_owned(), decl("blank_key", OutputKind::Value)),
            ("nothing".to_owned(), decl("nothing_key", OutputKind::Value)),
            ("absent".to_owned(), decl("absent_key", OutputKind::Value)),
        ]);
        let taken = take_outputs(&mut out, &decls);
        assert_eq!(
            out.keys().collect::<Vec<_>>(),
            vec!["name"],
            "only business fields stay"
        );
        let keys: Vec<(&str, bool, &str)> = taken
            .iter()
            .map(|p| (p.key.as_str(), p.secret, p.value()))
            .collect();
        assert_eq!(
            keys,
            vec![("count", false, "7"), ("auth_token", true, "s3cret")]
        );
        assert!(
            !format!("{taken:?}").contains("s3cret"),
            "Debug never shows a value"
        );
    }

    #[test]
    fn requested_outputs_gain_the_state_fields_once() {
        let mut outs = vec!["name".to_owned(), "token".to_owned()];
        let decls = BTreeMap::from([
            ("token".to_owned(), decl("t", OutputKind::Secret)),
            ("id".to_owned(), decl("i", OutputKind::Value)),
        ]);
        request_outputs(&mut outs, &decls);
        assert_eq!(outs, vec!["name", "token", "id"]);
    }

    #[test]
    fn ask_round_trips_through_the_result_output() {
        let ask = Ask {
            key: "one_time_code".into(),
            prompt: "What is the code we sent you?".into(),
            secret: true,
            retries: 2,
        };
        let v = Value::Object(ask.to_output());
        assert_eq!(v["key"], "one_time_code");
        assert_eq!(Ask::from_output(&v), Some(ask));
        assert_eq!(Ask::from_output(&json!({"prompt": "no key"})), None);
    }

    #[test]
    fn a_pause_allows_its_retries_then_gives_up() {
        let pause = InputPause {
            case_id: "CS-1".into(),
            context_space: "x:y".into(),
            slots: BTreeMap::new(),
            message_id: None,
            key: "otp".into(),
            prompt: "Code?".into(),
            secret: true,
            retries: 2,
            attempts: 0,
            until_ms: 100,
            task: None,
        };
        let json = serde_json::to_string(&pause).unwrap();
        assert_eq!(serde_json::from_str::<InputPause>(&json).unwrap(), pause);
        assert!(pause.fresh(100) && !pause.fresh(101));
        let one = pause.retry().unwrap();
        assert_eq!(one.attempts, 1);
        let two = one.retry().unwrap();
        assert_eq!(two.attempts, 2);
        assert!(
            two.retry().is_none(),
            "the third empty answer exhausts retries = 2"
        );
    }

    #[test]
    fn a_resolve_call_is_a_fresh_confirmation_free_runtime_call_of_the_same_case() {
        let parent: OpCall = serde_json::from_value(json!({
            "call_id": "opc_00000000-0000-0000-0000-000000000001", "case_id": "CS-1", "conversation_id": "conv-1",
            "phase": 2, "step": "tool1_0", "attempt": 3, "agent_role": "assistant", "operation": "order.read",
            "env": "test", "initiator": "assistant@t", "deadline_ms": 9, "confirm_ref": "m1",
            "idempotency_key": "k", "performed_by": "assistant", "identity_level": 2
        }))
        .unwrap();
        let sub = resolve_call(&parent, "crm.lookup{by_email}", Map::new());
        assert_ne!(sub.call_id, parent.call_id);
        assert!(sub.call_id.starts_with("opc_"));
        assert_eq!(
            (sub.operation.as_str(), sub.variant.as_deref()),
            ("crm.lookup", Some("by_email"))
        );
        assert_eq!(op_key_of(&sub), "crm.lookup{by_email}");
        assert_eq!(
            (sub.case_id.as_str(), sub.conversation_id.as_str()),
            ("CS-1", "conv-1")
        );
        assert_eq!(
            (sub.agent_role.as_str(), sub.initiator.as_str()),
            ("runtime", "runtime")
        );
        assert_eq!(sub.provenance.as_deref(), Some(PROVENANCE_RESOLVE));
        assert!(
            sub.confirm_ref.is_none()
                && sub.idempotency_key.is_none()
                && sub.performed_by.is_none()
        );
        assert_eq!((sub.attempt, sub.phase, sub.identity_level), (1, 2, 2));
        assert_eq!(
            resolve_call(&parent, "iam.verify", Map::new()).variant,
            None
        );
    }
}
