//! Declared tool inputs (A2) and tenant IAM procedures (A3), pure: the serde shapes of the design-ledger kind
//! `tool_policy` and of `iam_procedure.steps`, their validators and the backwards chain planner. No IO.
//!
//! A tool input is `model` (the model supplies it) or `state` (the runtime fills it from conversation state, else
//! from a producer tool, else by asking the user). A tool output may be declared to write a state key. The
//! *producer index* of a job is the declared outputs of its `tool_policy` rows plus the steps of its attached IAM
//! procedures; [`plan_chain`] walks it backwards from the missing keys to the producers to run, dependencies first.
//!
//! An IAM *procedure* may additionally establish the person's identity (owner 2026-11-01): one of its steps may
//! write `subject`, and optionally `email` and `name` — never any other reserved key, and a plain tool policy may
//! write none of them. Its [`IdentityStrength`] says what was established: `identified` (level 1) or `verified`
//! (level 2, e.g. a one-time code).
//!
//! Validation refuses (422 codes, [`DeclError::code`]): `unknown_operation`, `invalid_key`, `reserved_key` (an output
//! to a platform-written channel key), `identity_incomplete` (email/name without exactly one subject, or an identity
//! key written twice), `identity_not_plain` (a sealed or expiring identity key), `empty_prompt`, `invalid_retries`,
//! `invalid_ttl`, `duplicate_output_key`, `empty_input_name`. Cycles and depth are structural and found by the
//! planner ([`ChainError`]).
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Keys the platform writes from the channel/caller; a declaration may read them, never produce them.
pub const RESERVED_KEYS: [&str; 7] = [
    "subject",
    "email",
    "name",
    "channel_subject",
    "caller",
    "caller_label",
    "identity_source",
];

/// The reserved keys an IAM procedure (and only a procedure) may produce: the person's identity.
pub const IDENTITY_KEYS: [&str; 3] = ["subject", "email", "name"];

/// What an IAM procedure established: who the person is, or proof of it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdentityStrength {
    /// Level 1: the procedure established who the person is (e.g. their email).
    #[default]
    Identified,
    /// Level 2: the procedure proved it (e.g. a one-time code).
    Verified,
}

impl IdentityStrength {
    pub fn as_str(self) -> &'static str {
        match self {
            IdentityStrength::Identified => "identified",
            IdentityStrength::Verified => "verified",
        }
    }

    /// The conversation identity level this strength establishes.
    pub fn level(self) -> u8 {
        match self {
            IdentityStrength::Identified => 1,
            IdentityStrength::Verified => 2,
        }
    }
}

/// Who declares an output: a tool policy (a plain tool) or a step of a tenant IAM procedure. Only the procedure
/// may write the person's identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityScope {
    Tool,
    Procedure,
}

impl IdentityScope {
    /// May an output write `key`?
    fn admits(self, key: &str) -> bool {
        match self {
            IdentityScope::Tool => !RESERVED_KEYS.contains(&key),
            IdentityScope::Procedure => {
                !RESERVED_KEYS.contains(&key) || IDENTITY_KEYS.contains(&key)
            }
        }
    }
}

/// The longest producer chain the planner follows (cycle-free), counted in producers on the stack.
pub const MAX_DEPTH: usize = 6;

/// Most `ask_user` retries a declaration may allow.
pub const MAX_RETRIES: u8 = 5;

fn default_retries() -> u8 {
    2
}

/// `confirmation`: who may confirm a write. `by_user` (default): a message of the conversation's user; `auto`:
/// worker jobs, the board's gates control.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confirmation {
    #[default]
    ByUser,
    Auto,
}

/// `on_failure`: the outcome when an input cannot be resolved (retries exhausted, producer failed).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnFailure {
    #[default]
    Handover,
    Refuse,
}

/// The literal `"state"` of `{"from": "state"}`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FromState {
    State,
}

/// Where a `state` input comes from when the key is not in state yet: `"state"` (nothing to run: the key's producer
/// in the index, if any), `{tool, output}` (that tool's output field) or `{ask_user, secret, retries}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum StateFrom {
    State(FromState),
    Tool {
        tool: String,
        output: String,
    },
    AskUser {
        ask_user: String,
        #[serde(default)]
        secret: bool,
        #[serde(default = "default_retries")]
        retries: u8,
    },
}

impl Default for StateFrom {
    fn default() -> Self {
        StateFrom::State(FromState::State)
    }
}

/// One tool input: `{"source":"model"}` or `{"source":"state","key":..,"from":..}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputDecl {
    Model,
    State {
        key: String,
        #[serde(default)]
        from: StateFrom,
    },
}

/// A state value's kind: `secret` is sealed at rest and never shown to the model or the transcript.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputKind {
    #[default]
    Value,
    Secret,
}

/// One tool output field written to a state key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputDecl {
    pub key: String,
    #[serde(default)]
    pub kind: OutputKind,
    #[serde(default)]
    pub ttl_s: Option<u32>,
    #[serde(default)]
    pub persist_for_subject: bool,
}

/// The ledger value of kind `tool_policy` (key = op key).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolPolicy {
    #[serde(default)]
    pub inputs: BTreeMap<String, InputDecl>,
    #[serde(default)]
    pub outputs: BTreeMap<String, OutputDecl>,
    #[serde(default)]
    pub requires: Vec<String>,
    #[serde(default)]
    pub confirmation: Confirmation,
    #[serde(default)]
    pub on_failure: OnFailure,
}

/// One step of an IAM procedure: an op with the same input/output declarations as a policy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IamStep {
    pub op: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, InputDecl>,
    #[serde(default)]
    pub outputs: BTreeMap<String, OutputDecl>,
}

/// A tenant IAM procedure (`iam_procedure`): a named set of producing steps and the identity strength it
/// establishes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IamProcedure {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// What the procedure establishes (owner 2026-11-01). An absent field reads as `identified` (0248's default).
    #[serde(default)]
    pub strength: IdentityStrength,
    #[serde(default)]
    pub steps: Vec<IamStep>,
}

/// A validation refusal: a machine `code` (422) and a sentence for the admin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclError {
    pub code: &'static str,
    pub detail: String,
}

fn err(code: &'static str, detail: String) -> DeclError {
    DeclError { code, detail }
}

/// `^[a-z][a-z0-9_]{0,63}$`.
pub fn valid_key(k: &str) -> bool {
    let mut cs = k.chars();
    k.len() <= 64
        && cs.next().is_some_and(|c| c.is_ascii_lowercase())
        && cs.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn check_key(k: &str, what: &str) -> Result<(), DeclError> {
    if valid_key(k) {
        Ok(())
    } else {
        Err(err(
            "invalid_key",
            format!(
                "{what} `{k}` must be lowercase letters, digits and underscores, starting with a letter (max 64)."
            ),
        ))
    }
}

fn check_known(op: &str, known: &dyn Fn(&str) -> bool, what: &str) -> Result<(), DeclError> {
    if known(op) {
        Ok(())
    } else {
        Err(err(
            "unknown_operation",
            format!("{what} `{op}` is not an operation of this job."),
        ))
    }
}

fn check_inputs(
    inputs: &BTreeMap<String, InputDecl>,
    known: &dyn Fn(&str) -> bool,
) -> Result<(), DeclError> {
    for (name, decl) in inputs {
        if name.trim().is_empty() {
            return Err(err("empty_input_name", "An input needs a name.".into()));
        }
        let InputDecl::State { key, from } = decl else {
            continue;
        };
        check_key(key, "State key")?;
        match from {
            StateFrom::State(_) => {}
            StateFrom::Tool { tool, output } => {
                check_known(tool, known, "Producer tool")?;
                if output.trim().is_empty() {
                    return Err(err(
                        "empty_input_name",
                        format!("Input `{name}` names no output of `{tool}`."),
                    ));
                }
            }
            StateFrom::AskUser {
                ask_user, retries, ..
            } => {
                // Owner 2026-11-01: the never-ask-for-ids rule governs our demos, not the platform. A declaration
                // (a sign-in procedure in particular) may ask for an email, a phone, a postcode or an id.
                if ask_user.trim().is_empty() {
                    return Err(err(
                        "empty_prompt",
                        format!("Input `{name}` asks the user with no prompt."),
                    ));
                }
                if *retries > MAX_RETRIES {
                    return Err(err(
                        "invalid_retries",
                        format!("Retries are at most {MAX_RETRIES}."),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn check_outputs(
    outputs: &BTreeMap<String, OutputDecl>,
    scope: IdentityScope,
) -> Result<(), DeclError> {
    let mut seen = BTreeSet::new();
    for (field, o) in outputs {
        if field.trim().is_empty() {
            return Err(err(
                "empty_input_name",
                "An output needs a field name.".into(),
            ));
        }
        check_key(&o.key, "Output key")?;
        if !scope.admits(&o.key) {
            return Err(err(
                "reserved_key",
                format!(
                    "`{}` is written by the platform; a tool cannot produce it.",
                    o.key
                ),
            ));
        }
        if IDENTITY_KEYS.contains(&o.key.as_str())
            && (o.kind == OutputKind::Secret || o.ttl_s.is_some())
        {
            return Err(err(
                "identity_not_plain",
                format!(
                    "`{}` is the person's identity: a plain value, with no expiry.",
                    o.key
                ),
            ));
        }
        if o.ttl_s == Some(0) {
            return Err(err(
                "invalid_ttl",
                format!("`{}` has a ttl of 0 seconds.", o.key),
            ));
        }
        if !seen.insert(o.key.as_str()) {
            return Err(err(
                "duplicate_output_key",
                format!("Two outputs write `{}`.", o.key),
            ));
        }
    }
    Ok(())
}

/// Validate the policy of operation `op` (`known_ops` = the job's expected catalogue).
pub fn validate_policy(
    op: &str,
    p: &ToolPolicy,
    known_ops: &dyn Fn(&str) -> bool,
) -> Result<(), DeclError> {
    check_known(op, known_ops, "Operation")?;
    check_inputs(&p.inputs, known_ops)?;
    check_outputs(&p.outputs, IdentityScope::Tool)?;
    for k in &p.requires {
        check_key(k, "Required key")?;
    }
    Ok(())
}

/// Validate a procedure: a name of 1..=80 chars, each step like a policy (`name_taken` is the store's).
pub fn validate_procedure(
    p: &IamProcedure,
    known_ops: &dyn Fn(&str) -> bool,
) -> Result<(), DeclError> {
    let n = p.name.trim().chars().count();
    if !(1..=80).contains(&n) {
        return Err(err(
            "invalid_name",
            "A procedure name is 1 to 80 characters.".into(),
        ));
    }
    for s in &p.steps {
        check_known(&s.op, known_ops, "Step operation")?;
        check_inputs(&s.inputs, known_ops)?;
        check_outputs(&s.outputs, IdentityScope::Procedure)?;
    }
    // The identity a procedure establishes is one person and one shape: `subject` exactly once across its steps,
    // `email` and `name` each at most once and only beside a subject.
    let mut subjects = 0usize;
    let mut seen: BTreeMap<&str, ()> = BTreeMap::new();
    for s in &p.steps {
        for o in s.outputs.values() {
            if !IDENTITY_KEYS.contains(&o.key.as_str()) {
                continue;
            }
            if o.key == "subject" {
                subjects += 1;
            }
            if seen.insert(o.key.as_str(), ()).is_some() {
                return Err(err(
                    "identity_incomplete",
                    format!(
                        "Two steps write `{}`; the person's identity is written once.",
                        o.key
                    ),
                ));
            }
        }
    }
    if subjects > 1 {
        return Err(err(
            "identity_incomplete",
            "A procedure establishes one person: exactly one step writes `subject`.".into(),
        ));
    }
    if subjects == 0 && !seen.is_empty() {
        return Err(err(
            "identity_incomplete",
            "`email` and `name` describe a person: the procedure must also write `subject`.".into(),
        ));
    }
    Ok(())
}

// ---- producer index and chain planner ------------------------------------------------------------------------------

/// An operation that writes state keys, with the inputs it needs first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Producer {
    pub op: String,
    pub inputs: BTreeMap<String, InputDecl>,
    /// Output field -> state key.
    pub outputs: BTreeMap<String, String>,
}

/// State key -> its producer, built from a job's policies and attached procedures. The first producer of a key wins
/// (policies first, in the order given).
#[derive(Clone, Debug, Default)]
pub struct ProducerIndex {
    producers: Vec<Producer>,
    by_key: BTreeMap<String, usize>,
}

impl ProducerIndex {
    /// `policies`: `(op key, policy)` of the job; `procedures`: the attached ones.
    pub fn build(policies: &[(String, ToolPolicy)], procedures: &[IamProcedure]) -> Self {
        let mut ix = ProducerIndex::default();
        for (op, p) in policies {
            ix.add(op, &p.inputs, &p.outputs);
        }
        for pr in procedures {
            for s in &pr.steps {
                ix.add(&s.op, &s.inputs, &s.outputs);
            }
        }
        ix
    }

    fn add(
        &mut self,
        op: &str,
        inputs: &BTreeMap<String, InputDecl>,
        outputs: &BTreeMap<String, OutputDecl>,
    ) {
        if outputs.is_empty() {
            return;
        }
        let i = self.producers.len();
        self.producers.push(Producer {
            op: op.to_owned(),
            inputs: inputs.clone(),
            outputs: outputs
                .iter()
                .map(|(f, o)| (f.clone(), o.key.clone()))
                .collect(),
        });
        for o in outputs.values() {
            self.by_key.entry(o.key.clone()).or_insert(i);
        }
    }

    /// The producer of `key`.
    pub fn producer_of(&self, key: &str) -> Option<&Producer> {
        self.by_key.get(key).map(|&i| &self.producers[i])
    }

    /// The producer op `op` offers (when it declares outputs).
    pub fn producer_op(&self, op: &str) -> Option<&Producer> {
        self.producers.iter().find(|p| p.op == op)
    }

    /// Does any op produce `key`?
    pub fn produces(&self, key: &str) -> bool {
        self.by_key.contains_key(key)
    }
}

/// One step of a planned chain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainStep {
    /// Run `op` as a resolve sub-call; it writes `writes` (state keys, sorted).
    Run { op: String, writes: Vec<String> },
    /// Stop and ask the user for `key` (always the last step).
    AskUser {
        key: String,
        prompt: String,
        secret: bool,
        retries: u8,
    },
}

/// Why no chain exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainError {
    Cycle { op: String },
    DepthExceeded,
    NoProducer { key: String },
}

impl ChainError {
    /// The 422 / outcome code.
    pub fn code(&self) -> &'static str {
        match self {
            ChainError::Cycle { .. } => "cycle",
            ChainError::DepthExceeded => "depth_exceeded",
            ChainError::NoProducer { .. } => "no_producer",
        }
    }
}

struct Planner<'a> {
    idx: &'a ProducerIndex,
    present: &'a dyn Fn(&str) -> bool,
    /// Keys that will be present once the steps so far ran.
    planned: BTreeSet<String>,
    stack: Vec<String>,
    steps: Vec<ChainStep>,
}

impl Planner<'_> {
    fn have(&self, key: &str) -> bool {
        (self.present)(key) || self.planned.contains(key)
    }

    /// Plan so that `key` becomes present. `Ok(true)` = an ask-user step was emitted: stop everything.
    fn need(&mut self, key: &str, via: Option<&StateFrom>) -> Result<bool, ChainError> {
        if self.have(key) {
            return Ok(false);
        }
        let producer = match via {
            Some(StateFrom::Tool { tool, output }) => self
                .idx
                .producer_op(tool)
                .filter(|p| p.outputs.contains_key(output))
                .ok_or_else(|| ChainError::NoProducer {
                    key: key.to_owned(),
                })?,
            _ => self
                .idx
                .producer_of(key)
                .ok_or_else(|| ChainError::NoProducer {
                    key: key.to_owned(),
                })?,
        };
        if self.stack.contains(&producer.op) {
            return Err(ChainError::Cycle {
                op: producer.op.clone(),
            });
        }
        if self.stack.len() >= MAX_DEPTH {
            return Err(ChainError::DepthExceeded);
        }
        self.stack.push(producer.op.clone());
        for decl in producer.inputs.values() {
            let InputDecl::State { key: k, from } = decl else {
                continue;
            };
            if self.have(k) {
                continue;
            }
            if let StateFrom::AskUser {
                ask_user,
                secret,
                retries,
            } = from
            {
                self.steps.push(ChainStep::AskUser {
                    key: k.clone(),
                    prompt: ask_user.clone(),
                    secret: *secret,
                    retries: *retries,
                });
                return Ok(true);
            }
            if self.need(k, Some(from))? {
                return Ok(true);
            }
        }
        self.stack.pop();
        let mut writes: Vec<String> = producer.outputs.values().cloned().collect();
        writes.sort();
        writes.dedup();
        self.planned.extend(writes.iter().cloned());
        self.steps.push(ChainStep::Run {
            op: producer.op.clone(),
            writes,
        });
        Ok(false)
    }
}

/// Plan the producers to run so every key of `missing` becomes present, dependencies first. Keys already
/// `present` are skipped. Stops at the first input that needs the user: the result then ends with
/// [`ChainStep::AskUser`] (re-plan after the answer is stored).
pub fn plan_chain(
    missing: &[&str],
    present: &dyn Fn(&str) -> bool,
    idx: &ProducerIndex,
) -> Result<Vec<ChainStep>, ChainError> {
    let mut p = Planner {
        idx,
        present,
        planned: BTreeSet::new(),
        stack: Vec::new(),
        steps: Vec::new(),
    };
    for k in missing {
        if p.need(k, None)? {
            break;
        }
    }
    Ok(p.steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn state(key: &str, from: StateFrom) -> InputDecl {
        InputDecl::State {
            key: key.into(),
            from,
        }
    }
    fn out(key: &str) -> OutputDecl {
        OutputDecl {
            key: key.into(),
            kind: OutputKind::Value,
            ttl_s: None,
            persist_for_subject: false,
        }
    }
    fn ask(prompt: &str, secret: bool) -> StateFrom {
        StateFrom::AskUser {
            ask_user: prompt.into(),
            secret,
            retries: 2,
        }
    }
    fn all_known(_: &str) -> bool {
        true
    }

    /// customer_id <- crm.lookup(email, auth_token); auth_token <- iam.verify(one_time_code); one_time_code <- user.
    fn acceptance() -> ProducerIndex {
        let crm = ToolPolicy {
            inputs: BTreeMap::from([
                ("email".into(), state("email", StateFrom::default())),
                ("token".into(), state("auth_token", StateFrom::default())),
                ("q".into(), InputDecl::Model),
            ]),
            outputs: BTreeMap::from([("id".into(), out("customer_id"))]),
            ..Default::default()
        };
        let verify = IamStep {
            op: "iam.verify".into(),
            inputs: BTreeMap::from([(
                "code".into(),
                state("one_time_code", ask("Enter the code we sent you.", true)),
            )]),
            outputs: BTreeMap::from([("token".into(), out("auth_token"))]),
        };
        let proc = IamProcedure {
            id: Uuid::nil(),
            name: "Acme sign-in".into(),
            description: String::new(),
            strength: IdentityStrength::Verified,
            steps: vec![verify],
        };
        ProducerIndex::build(&[("crm.lookup".into(), crm)], &[proc])
    }

    #[test]
    fn json_shape_round_trips() {
        let v = json!({
            "inputs": {
                "q": {"source": "model"},
                "email": {"source": "state", "key": "email", "from": "state"},
                "tok": {"source": "state", "key": "auth_token", "from": {"tool": "iam.verify", "output": "token"}},
                "code": {"source": "state", "key": "one_time_code",
                         "from": {"ask_user": "Code?", "secret": true, "retries": 2}}
            },
            "outputs": {"id": {"key": "customer_id", "kind": "secret", "ttl_s": 3600, "persist_for_subject": true}},
            "requires": ["customer_id"],
            "confirmation": "auto",
            "on_failure": "refuse"
        });
        let p: ToolPolicy = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(p.confirmation, Confirmation::Auto);
        assert_eq!(p.on_failure, OnFailure::Refuse);
        assert!(matches!(p.inputs["q"], InputDecl::Model));
        assert_eq!(serde_json::to_value(&p).unwrap(), v);
    }

    #[test]
    fn defaults_and_unknown_fields() {
        let p: ToolPolicy = serde_json::from_value(json!({})).unwrap();
        assert_eq!(p.confirmation, Confirmation::ByUser);
        assert_eq!(p.on_failure, OnFailure::Handover);
        let i: InputDecl = serde_json::from_value(json!({"source": "state", "key": "k"})).unwrap();
        assert_eq!(i, state("k", StateFrom::default()));
        assert!(serde_json::from_value::<ToolPolicy>(json!({"nope": 1})).is_err());
        assert!(serde_json::from_value::<OutputDecl>(json!({"key": "k", "x": 1})).is_err());
        assert!(
            serde_json::from_value::<InputDecl>(
                json!({"source": "state", "key": "k", "from": "elsewhere"})
            )
            .is_err()
        );
    }

    #[test]
    fn validates_unknown_op_keys_and_reserved() {
        let known = |o: &str| o == "crm.lookup";
        let ok = ToolPolicy {
            outputs: BTreeMap::from([("id".into(), out("customer_id"))]),
            ..Default::default()
        };
        assert!(validate_policy("crm.lookup", &ok, &known).is_ok());
        assert_eq!(
            validate_policy("x.y", &ok, &known).unwrap_err().code,
            "unknown_operation"
        );
        let tool_from = ToolPolicy {
            inputs: BTreeMap::from([(
                "t".into(),
                state(
                    "auth_token",
                    StateFrom::Tool {
                        tool: "nope.x".into(),
                        output: "t".into(),
                    },
                ),
            )]),
            ..Default::default()
        };
        assert_eq!(
            validate_policy("crm.lookup", &tool_from, &known)
                .unwrap_err()
                .code,
            "unknown_operation"
        );
        for k in ["subject", "email", "caller", "identity_source"] {
            let p = ToolPolicy {
                outputs: BTreeMap::from([("f".into(), out(k))]),
                ..Default::default()
            };
            assert_eq!(
                validate_policy("crm.lookup", &p, &known).unwrap_err().code,
                "reserved_key"
            );
        }
        let too_long = "a".repeat(65);
        for k in ["Bad", "1x", "a-b", "", too_long.as_str()] {
            let p = ToolPolicy {
                outputs: BTreeMap::from([("f".into(), out(k))]),
                ..Default::default()
            };
            assert_eq!(
                validate_policy("crm.lookup", &p, &known).unwrap_err().code,
                "invalid_key"
            );
        }
        assert!(valid_key(&"a".repeat(64)));
        let dup = ToolPolicy {
            outputs: BTreeMap::from([("a".into(), out("k")), ("b".into(), out("k"))]),
            ..Default::default()
        };
        assert_eq!(
            validate_policy("crm.lookup", &dup, &known)
                .unwrap_err()
                .code,
            "duplicate_output_key"
        );
        let ttl = ToolPolicy {
            outputs: BTreeMap::from([(
                "a".into(),
                OutputDecl {
                    ttl_s: Some(0),
                    ..out("k")
                },
            )]),
            ..Default::default()
        };
        assert_eq!(
            validate_policy("crm.lookup", &ttl, &known)
                .unwrap_err()
                .code,
            "invalid_ttl"
        );
        let bad_req = ToolPolicy {
            requires: vec!["Bad Key".into()],
            ..Default::default()
        };
        assert_eq!(
            validate_policy("crm.lookup", &bad_req, &known)
                .unwrap_err()
                .code,
            "invalid_key"
        );
    }

    #[test]
    fn asks_the_user_for_any_key_the_tenant_declares() {
        // Owner 2026-11-01: the never-ask-for-ids rule governs our demos, not the platform. A sign-in procedure may
        // ask for an email or an id.
        for k in [
            "order_id",
            "customer_id",
            "email",
            "postcode",
            "claim_number",
        ] {
            let p = ToolPolicy {
                inputs: BTreeMap::from([("x".into(), state(k, ask("What is it?", false)))]),
                ..Default::default()
            };
            assert!(validate_policy("a.b", &p, &all_known).is_ok(), "{k}");
        }
        for k in ["one_time_code", "password", "promo_code"] {
            let p = ToolPolicy {
                inputs: BTreeMap::from([("x".into(), state(k, ask("Code?", true)))]),
                ..Default::default()
            };
            assert!(validate_policy("a.b", &p, &all_known).is_ok(), "{k}");
        }
        let empty = ToolPolicy {
            inputs: BTreeMap::from([("x".into(), state("otp", ask("  ", true)))]),
            ..Default::default()
        };
        assert_eq!(
            validate_policy("a.b", &empty, &all_known).unwrap_err().code,
            "empty_prompt"
        );
        let many = ToolPolicy {
            inputs: BTreeMap::from([(
                "x".into(),
                state(
                    "otp",
                    StateFrom::AskUser {
                        ask_user: "Code?".into(),
                        secret: true,
                        retries: 9,
                    },
                ),
            )]),
            ..Default::default()
        };
        assert_eq!(
            validate_policy("a.b", &many, &all_known).unwrap_err().code,
            "invalid_retries"
        );
    }

    #[test]
    fn validates_procedures() {
        let proc = |steps: Vec<IamStep>, strength| IamProcedure {
            id: Uuid::nil(),
            name: "Acme sign-in".into(),
            description: String::new(),
            strength,
            steps,
        };
        let step = |op: &str, field: &str, key: &str| IamStep {
            op: op.into(),
            inputs: BTreeMap::new(),
            outputs: BTreeMap::from([(field.into(), out(key))]),
        };
        let mut p = proc(
            vec![step("iam.verify", "t", "auth_token")],
            IdentityStrength::Verified,
        );
        assert!(validate_procedure(&p, &all_known).is_ok());
        assert_eq!(p.strength, IdentityStrength::Verified);
        assert_eq!(
            validate_procedure(&p, &|_| false).unwrap_err().code,
            "unknown_operation"
        );

        // a procedure may establish the person (subject, optionally email/name) — a tool policy may not
        let sign_in = proc(
            vec![
                step("iam.verify", "t", "auth_token"),
                IamStep {
                    op: "iam.verify".into(),
                    inputs: BTreeMap::new(),
                    outputs: BTreeMap::from([
                        ("id".into(), out("subject")),
                        ("mail".into(), out("email")),
                    ]),
                },
            ],
            IdentityStrength::Verified,
        );
        assert!(validate_procedure(&sign_in, &all_known).is_ok());
        // other reserved keys stay refused
        let channel = proc(
            vec![step("iam.verify", "c", "channel_subject")],
            IdentityStrength::Identified,
        );
        assert_eq!(
            validate_procedure(&channel, &all_known).unwrap_err().code,
            "reserved_key"
        );
        // email/name need the subject, and exactly one subject
        let no_subject = proc(
            vec![step("iam.verify", "m", "email")],
            IdentityStrength::Identified,
        );
        assert_eq!(
            validate_procedure(&no_subject, &all_known)
                .unwrap_err()
                .code,
            "identity_incomplete"
        );
        let two_subjects = proc(
            vec![
                step("iam.verify", "a", "subject"),
                step("iam.verify", "b", "subject"),
            ],
            IdentityStrength::Verified,
        );
        assert_eq!(
            validate_procedure(&two_subjects, &all_known)
                .unwrap_err()
                .code,
            "identity_incomplete"
        );
        let twice = proc(
            vec![
                IamStep {
                    op: "iam.verify".into(),
                    inputs: BTreeMap::new(),
                    outputs: BTreeMap::from([
                        ("a".into(), out("subject")),
                        ("m1".into(), out("email")),
                    ]),
                },
                step("iam.verify", "m2", "email"),
            ],
            IdentityStrength::Verified,
        );
        assert_eq!(
            validate_procedure(&twice, &all_known).unwrap_err().code,
            "identity_incomplete"
        );
        // the identity is a plain value with no expiry
        for bad in [
            OutputDecl {
                key: "subject".into(),
                kind: OutputKind::Secret,
                ttl_s: None,
                persist_for_subject: false,
            },
            OutputDecl {
                key: "subject".into(),
                kind: OutputKind::Value,
                ttl_s: Some(600),
                persist_for_subject: false,
            },
        ] {
            let p = proc(
                vec![IamStep {
                    op: "iam.verify".into(),
                    inputs: BTreeMap::new(),
                    outputs: BTreeMap::from([("id".into(), bad)]),
                }],
                IdentityStrength::Identified,
            );
            assert_eq!(
                validate_procedure(&p, &all_known).unwrap_err().code,
                "identity_not_plain"
            );
        }
        p.name = " ".into();
        assert_eq!(
            validate_procedure(&p, &all_known).unwrap_err().code,
            "invalid_name"
        );
    }

    #[test]
    fn an_absent_strength_reads_as_identified() {
        let v = json!({"id": Uuid::nil(), "name": "Acme sign-in"});
        let p: IamProcedure = serde_json::from_value(v).expect("a procedure");
        assert_eq!(p.strength, IdentityStrength::Identified);
        assert_eq!(p.strength.level(), 1);
        assert_eq!(IdentityStrength::Verified.level(), 2);
        assert_eq!(
            serde_json::to_value(IdentityStrength::Verified).unwrap(),
            json!("verified")
        );
    }

    #[test]
    fn chain_stops_at_the_one_time_code() {
        let ix = acceptance();
        let present = |k: &str| k == "email";
        let steps = plan_chain(&["customer_id"], &present, &ix).unwrap();
        assert_eq!(
            steps,
            vec![ChainStep::AskUser {
                key: "one_time_code".into(),
                prompt: "Enter the code we sent you.".into(),
                secret: true,
                retries: 2,
            }]
        );
    }

    #[test]
    fn chain_completes_once_the_code_is_stored() {
        let ix = acceptance();
        let present = |k: &str| k == "email" || k == "one_time_code";
        let steps = plan_chain(&["customer_id"], &present, &ix).unwrap();
        assert_eq!(
            steps,
            vec![
                ChainStep::Run {
                    op: "iam.verify".into(),
                    writes: vec!["auth_token".into()]
                },
                ChainStep::Run {
                    op: "crm.lookup".into(),
                    writes: vec!["customer_id".into()]
                },
            ]
        );
        // a present key needs nothing
        let all = |_: &str| true;
        assert!(plan_chain(&["customer_id"], &all, &ix).unwrap().is_empty());
        // a token already in state skips the sign-in
        let has_token = |k: &str| k == "email" || k == "auth_token";
        assert_eq!(
            plan_chain(&["customer_id"], &has_token, &ix).unwrap(),
            vec![ChainStep::Run {
                op: "crm.lookup".into(),
                writes: vec!["customer_id".into()]
            }]
        );
    }

    #[test]
    fn from_tool_names_the_producing_op() {
        let a = ToolPolicy {
            inputs: BTreeMap::from([(
                "t".into(),
                state(
                    "session",
                    StateFrom::Tool {
                        tool: "login.start".into(),
                        output: "sid".into(),
                    },
                ),
            )]),
            outputs: BTreeMap::from([("n".into(), out("profile"))]),
            ..Default::default()
        };
        let b = ToolPolicy {
            outputs: BTreeMap::from([("sid".into(), out("session"))]),
            ..Default::default()
        };
        let ix = ProducerIndex::build(&[("crm.profile".into(), a), ("login.start".into(), b)], &[]);
        let steps = plan_chain(&["profile"], &|_| false, &ix).unwrap();
        assert_eq!(
            steps
                .iter()
                .map(|s| match s {
                    ChainStep::Run { op, .. } => op.as_str(),
                    _ => "ask",
                })
                .collect::<Vec<_>>(),
            ["login.start", "crm.profile"]
        );
        // naming an output the tool lacks has no producer
        let bad = ToolPolicy {
            inputs: BTreeMap::from([(
                "t".into(),
                state(
                    "session",
                    StateFrom::Tool {
                        tool: "login.start".into(),
                        output: "missing".into(),
                    },
                ),
            )]),
            outputs: BTreeMap::from([("n".into(), out("profile"))]),
            ..Default::default()
        };
        let b2 = ToolPolicy {
            outputs: BTreeMap::from([("sid".into(), out("session"))]),
            ..Default::default()
        };
        let ix = ProducerIndex::build(
            &[("crm.profile".into(), bad), ("login.start".into(), b2)],
            &[],
        );
        assert_eq!(
            plan_chain(&["profile"], &|_| false, &ix).unwrap_err(),
            ChainError::NoProducer {
                key: "session".into()
            }
        );
    }

    #[test]
    fn no_producer_cycle_and_depth() {
        let ix = ProducerIndex::default();
        let e = plan_chain(&["customer_id"], &|_| false, &ix).unwrap_err();
        assert_eq!(
            e,
            ChainError::NoProducer {
                key: "customer_id".into()
            }
        );
        assert_eq!(e.code(), "no_producer");

        let policy = |needs: &str, makes: &str| ToolPolicy {
            inputs: BTreeMap::from([("i".into(), state(needs, StateFrom::default()))]),
            outputs: BTreeMap::from([("o".into(), out(makes))]),
            ..Default::default()
        };
        let cyc = ProducerIndex::build(
            &[
                ("a.x".into(), policy("kb", "ka")),
                ("b.x".into(), policy("ka", "kb")),
            ],
            &[],
        );
        let e = plan_chain(&["ka"], &|_| false, &cyc).unwrap_err();
        assert!(matches!(e, ChainError::Cycle { .. }));
        assert_eq!(e.code(), "cycle");

        // k0 <- k1 <- ... <- kN; kN has no inputs
        let chain = |n: usize| {
            let mut ps = Vec::new();
            for i in 0..=n {
                let outk = format!("k{i}");
                let p = if i == n {
                    ToolPolicy {
                        outputs: BTreeMap::from([("o".into(), out(&outk))]),
                        ..Default::default()
                    }
                } else {
                    policy(&format!("k{}", i + 1), &outk)
                };
                ps.push((format!("op{i}.run"), p));
            }
            ProducerIndex::build(&ps, &[])
        };
        // six producers deep is allowed
        let steps = plan_chain(&["k0"], &|_| false, &chain(5)).unwrap();
        assert_eq!(steps.len(), 6);
        assert!(matches!(&steps[0], ChainStep::Run { op, .. } if op == "op5.run"));
        // seven is not
        assert_eq!(
            plan_chain(&["k0"], &|_| false, &chain(6)).unwrap_err(),
            ChainError::DepthExceeded
        );
    }

    #[test]
    fn shared_dependencies_run_once() {
        let login = ToolPolicy {
            outputs: BTreeMap::from([("t".into(), out("auth_token"))]),
            ..Default::default()
        };
        let needs_token = |out_key: &str| ToolPolicy {
            inputs: BTreeMap::from([("t".into(), state("auth_token", StateFrom::default()))]),
            outputs: BTreeMap::from([("o".into(), out(out_key))]),
            ..Default::default()
        };
        let ix = ProducerIndex::build(
            &[
                ("iam.login".into(), login),
                ("crm.a".into(), needs_token("ka")),
                ("crm.b".into(), needs_token("kb")),
            ],
            &[],
        );
        let steps = plan_chain(&["ka", "kb"], &|_| false, &ix).unwrap();
        let ops: Vec<_> = steps
            .iter()
            .filter_map(|s| match s {
                ChainStep::Run { op, .. } => Some(op.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ops, ["iam.login", "crm.a", "crm.b"]);
    }
}
