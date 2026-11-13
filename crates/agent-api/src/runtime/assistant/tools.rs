//! The tools the assistant loop offers (B4 T7a, F-40): the operations the role's toolset grants for the activity,
//! described from the job's operation catalogue — the canonical inputs only, never another call's outputs. Each tool
//! call the model makes becomes an ordinary `op.call` (crate::runtime::runner, steps 1–11 unchanged); a name the
//! loop did not offer is answered `{"error": "not_entitled"}` and never sent.
//!
//! Inputs the runtime fills itself ([`RUNTIME_INPUTS`]) are never offered: the model cannot name them, and a value
//! it sends anyway is dropped by [`input_of`] (prompt-injection safety: a conversation must not choose whom a
//! ticket is raised for).
//!
//! A2: an input the job's declaration for the op maps to conversation state (`InputDecl::State`) is not offered
//! either, and [`input_of`] drops it if the model sends it anyway — the runner fills it (runner step 3c), from state,
//! a producer chain or a question to the user. The model never names a token or an id the declaration owns.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::runtime::catalog::OperationCatalog;
use crate::runtime::declared::InputDecl;
use crate::runtime::design::RuntimeDesign;

/// The toolset index (`ToolsetIndex.allows(design, role, activity, op)`): may `role` run `op` for `activity` in the
/// case's design `d` (Live in Production, Draft in Test)? B5's [`DesignToolsets`](crate::job_design::toolset::DesignToolsets)
/// answers from `d.job_design.toolsets` and is the ONE source the assistant's offer and the runner's `ToolGate`
/// consult (B4 owner ruling 5). B4 ships [`NoToolset`]: the assistant is offered no tools.
pub trait ToolsetIndex: Send + Sync {
    fn allows(&self, d: &RuntimeDesign, role: &str, activity: Option<&str>, op: &str) -> bool;
}

/// B4's index: grants nothing (the assistant answers without operations).
pub struct NoToolset;

impl ToolsetIndex for NoToolset {
    fn allows(&self, _d: &RuntimeDesign, _role: &str, _activity: Option<&str>, _op: &str) -> bool {
        false
    }
}

/// Declared inputs the runtime fills, never the model: `customer_email` of `case.create` is the case party's email
/// (crate::runtime::runner::ops::with_customer_email).
pub const RUNTIME_INPUTS: [&str; 1] = [crate::runtime::runner::ops::CUSTOMER_EMAIL];

/// One offered tool: the function name the model calls and the operation it runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tool {
    /// `^[A-Za-z0-9_-]{1,64}$` (the chat-completions function-name rule).
    pub name: String,
    /// `object.verb` or `object.verb{variant}`.
    pub op_key: String,
    /// The catalogue's canonical input fields the model may set (without [`RUNTIME_INPUTS`]).
    pub inputs: Vec<String>,
    /// `R` | `W` | `X` | `P`.
    pub side_effect: String,
    /// What an input declares beyond its name (type, description), e.g. from an MCP tool's `inputSchema`; an input
    /// without an entry is offered by name only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub input_types: BTreeMap<String, InputType>,
}

pub use crate::connectors::model::InputType;

impl InputType {
    /// The JSON-schema property of an input named `name`: a known type, else the name as description only.
    fn property(&self, name: &str) -> Value {
        const TYPES: [&str; 6] = ["string", "integer", "number", "boolean", "array", "object"];
        let description = if self.description.trim().is_empty() {
            name
        } else {
            self.description.trim()
        };
        if TYPES.contains(&self.kind.as_str()) {
            json!({"type": self.kind, "description": description})
        } else {
            json!({"description": description})
        }
    }
}

/// The function name of an operation key: `order.read` → `order_read`, `rule.evaluate{RET-01}` →
/// `rule_evaluate__RET-01`; anything outside `[A-Za-z0-9_-]` becomes `_`, at most 64 characters.
pub fn tool_name(op_key: &str) -> String {
    let mut out = String::with_capacity(op_key.len() + 1);
    for c in op_key.chars() {
        match c {
            '{' => out.push_str("__"),
            '}' => {}
            c if c.is_ascii_alphanumeric() || c == '_' || c == '-' => out.push(c),
            _ => out.push('_'),
        }
    }
    out.chars().take(64).collect()
}

/// The inputs of `op_key` the job's declaration maps to conversation state (the model does not offer them).
pub fn state_inputs(d: &RuntimeDesign, op_key: &str) -> Vec<String> {
    d.policy(op_key)
        .map(|p| {
            p.inputs
                .iter()
                .filter(|(_, decl)| matches!(decl, InputDecl::State { .. }))
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// The tools of `role` for `activity` in design `d`: every operation of `catalog` (the design's) the toolset grants,
/// sorted by key. Two keys with one function name get a numeric suffix. Inputs the declaration maps to state are left
/// out ([`state_inputs`]).
pub fn offered(
    catalog: &dyn OperationCatalog,
    d: &RuntimeDesign,
    toolset: &dyn ToolsetIndex,
    role: &str,
    activity: Option<&str>,
) -> Vec<Tool> {
    let mut tools: Vec<Tool> = Vec::new();
    for key in catalog.keys() {
        if !toolset.allows(d, role, activity, key) {
            continue;
        }
        let Some(def) = catalog.get(key) else {
            continue;
        };
        let base = tool_name(key);
        let mut name = base.clone();
        let mut n = 2;
        while tools.iter().any(|t| t.name == name) {
            name = format!("{}_{n}", base.chars().take(60).collect::<String>());
            n += 1;
        }
        let from_state = state_inputs(d, key);
        tools.push(Tool {
            name,
            op_key: key.to_owned(),
            inputs: def
                .inputs
                .iter()
                .filter(|i| !RUNTIME_INPUTS.contains(&i.as_str()) && !from_state.contains(i))
                .cloned()
                .collect(),
            side_effect: format!("{:?}", def.side_effect),
            // the catalogue (runtime::catalog::OpDef) declares names only; a job design that knows more fills this
            input_types: BTreeMap::new(),
        });
    }
    tools
}

/// The chat-completions `tools` array of `tools`.
pub fn specs(tools: &[Tool]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            let properties: Map<String, Value> = t
                .inputs
                .iter()
                .map(|i| {
                    let p = t
                        .input_types
                        .get(i)
                        .map_or_else(|| json!({"description": i}), |ty| ty.property(i));
                    (i.clone(), p)
                })
                .collect();
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": format!("Runs the operation {} (side effect {}).", t.op_key, t.side_effect),
                    "parameters": {"type": "object", "properties": properties, "additionalProperties": false},
                },
            })
        })
        .collect()
}

/// The offered tool named `name`, if any.
pub fn by_name<'a>(tools: &'a [Tool], name: &str) -> Option<&'a Tool> {
    tools.iter().find(|t| t.name == name)
}

/// The call's input: the model's arguments for the tool's offered inputs only (never a [`RUNTIME_INPUTS`] one, even
/// on a hand-built [`Tool`]).
pub fn input_of(tool: &Tool, args: &Map<String, Value>) -> Map<String, Value> {
    args.iter()
        .filter(|(k, _)| tool.inputs.contains(k) && !RUNTIME_INPUTS.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// `object.verb{variant}` → (`object.verb`, `Some(variant)`); no or an empty variant → `None`.
pub fn split_op_key(op_key: &str) -> (String, Option<String>) {
    match op_key.split_once('{') {
        Some((base, rest)) => (
            base.to_owned(),
            rest.strip_suffix('}')
                .filter(|v| !v.is_empty() && *v != "*")
                .map(str::to_owned),
        ),
        None => (op_key.to_owned(), None),
    }
}

/// What the model reads for a refused or failed call: the outcome code only.
pub fn refusal(code: &str) -> Value {
    json!({ "error": code })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::catalog::OpDef;

    #[test]
    fn names_follow_the_function_name_rule() {
        assert_eq!(tool_name("order.read"), "order_read");
        assert_eq!(tool_name("rule.evaluate{RET-01}"), "rule_evaluate__RET-01");
        assert_eq!(tool_name("case.create{return}"), "case_create__return");
        assert_eq!(tool_name(&"x".repeat(80)).len(), 64);
        assert_eq!(
            split_op_key("rule.evaluate{RET-01}"),
            ("rule.evaluate".to_owned(), Some("RET-01".to_owned()))
        );
        assert_eq!(split_op_key("order.read"), ("order.read".to_owned(), None));
        assert_eq!(split_op_key("a.b{*}"), ("a.b".to_owned(), None));
    }

    /// Grants every operation.
    struct AllTools;

    impl ToolsetIndex for AllTools {
        fn allows(
            &self,
            _d: &RuntimeDesign,
            _role: &str,
            _activity: Option<&str>,
            _op: &str,
        ) -> bool {
            true
        }
    }

    /// One operation.
    struct One(OpDef);

    impl OperationCatalog for One {
        fn get(&self, key: &str) -> Option<&OpDef> {
            (key == self.0.key).then_some(&self.0)
        }

        fn keys(&self) -> Vec<&str> {
            vec![self.0.key.as_str()]
        }
    }

    /// A declared type and description reach the model's spec; an input without them stays name-only, and an input the
    /// declaration maps to state (hidden from `inputs`) never appears even when typed.
    #[test]
    fn typed_inputs_carry_their_type_and_description() {
        let mut types = BTreeMap::new();
        types.insert(
            "card_id".to_owned(),
            InputType {
                kind: "string".into(),
                description: "The card to move.".into(),
            },
        );
        types.insert(
            "position".to_owned(),
            InputType {
                kind: "integer".into(),
                description: String::new(),
            },
        );
        types.insert(
            "odd".to_owned(),
            InputType {
                kind: "weird".into(),
                description: "kept".into(),
            },
        );
        types.insert(
            "token".to_owned(),
            InputType {
                kind: "string".into(),
                description: "hidden: state".into(),
            },
        );
        let tool = Tool {
            name: "card_move".into(),
            op_key: "card.move".into(),
            inputs: vec![
                "card_id".into(),
                "position".into(),
                "odd".into(),
                "title".into(),
            ],
            side_effect: "W".into(),
            input_types: types,
        };
        assert_eq!(
            specs(&[tool])[0]["function"]["parameters"]["properties"],
            json!({
                "card_id": {"type": "string", "description": "The card to move."},
                "position": {"type": "integer", "description": "position"},
                "odd": {"description": "kept"},
                "title": {"description": "title"},
            })
        );
    }

    /// `customer_email` (platform seed v2, a `case.create` input) is filled by the runner from the case party: the
    /// model is never offered it, and a value it sends anyway is dropped.
    #[test]
    fn the_model_is_never_offered_customer_email() {
        let mut def = crate::runtime::catalog::builtin_defs()
            .into_iter()
            .find(|d| d.key == "case.create{complaint}")
            .expect("a built-in case.create");
        def.inputs = vec!["subject".into(), "customer_email".into()];
        let catalog = One(def);
        let d = RuntimeDesign::blank(
            crate::design::JobRef {
                id: uuid::Uuid::nil(),
                key: "care".into(),
            },
            crate::design::DesignOf::Draft,
        );
        let tools = offered(&catalog, &d, &AllTools, "assistant", None);
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].inputs, ["subject"]);
        let spec = &specs(&tools)[0];
        assert_eq!(
            spec["function"]["parameters"]["properties"],
            json!({"subject": {"description": "subject"}})
        );
        let args = json!({"subject": "Broken", "customer_email": "attacker@evil.test"})
            .as_object()
            .cloned()
            .unwrap();
        assert_eq!(
            Value::Object(input_of(&tools[0], &args)),
            json!({"subject": "Broken"})
        );
        // even a tool built with it listed never passes it on
        let mut leaky = tools[0].clone();
        leaky.inputs.push("customer_email".into());
        assert_eq!(
            Value::Object(input_of(&leaky, &args)),
            json!({"subject": "Broken"})
        );
    }

    #[test]
    fn inputs_keep_only_the_declared_fields() {
        let t = Tool {
            name: "order_read".into(),
            op_key: "order.read".into(),
            inputs: vec!["order_id".into()],
            side_effect: "R".into(),
            input_types: Default::default(),
        };
        let args = json!({"order_id": "ORD-1", "amount": 9})
            .as_object()
            .cloned()
            .unwrap();
        assert_eq!(
            Value::Object(input_of(&t, &args)),
            json!({"order_id": "ORD-1"})
        );
        let spec = &specs(std::slice::from_ref(&t))[0];
        assert_eq!(spec["function"]["name"], "order_read");
        assert_eq!(
            spec["function"]["parameters"]["properties"],
            json!({"order_id": {"description": "order_id"}})
        );
        assert_eq!(refusal("not_entitled"), json!({"error": "not_entitled"}));
    }

    /// A2: an input the declaration maps to state is neither offered nor accepted from the model.
    #[test]
    fn state_mapped_inputs_are_not_offered() {
        use crate::runtime::declared::{StateFrom, ToolPolicy};
        let mut def = crate::runtime::catalog::builtin_defs()
            .into_iter()
            .find(|d| d.key == "case.create{complaint}")
            .expect("a built-in case.create");
        def.inputs = vec!["subject".into(), "auth_token".into()];
        let key = def.key.clone();
        let catalog = One(def);
        let mut d = RuntimeDesign::blank(
            crate::design::JobRef {
                id: uuid::Uuid::nil(),
                key: "care".into(),
            },
            crate::design::DesignOf::Draft,
        );
        d.policies.insert(
            key,
            ToolPolicy {
                inputs: [(
                    "auth_token".to_owned(),
                    InputDecl::State {
                        key: "auth_token".into(),
                        from: StateFrom::default(),
                    },
                )]
                .into(),
                ..ToolPolicy::default()
            },
        );
        let tools = offered(&catalog, &d, &AllTools, "assistant", None);
        assert_eq!(tools[0].inputs, ["subject"]);
        let args = json!({"subject": "x", "auth_token": "forged"})
            .as_object()
            .cloned()
            .unwrap();
        assert_eq!(
            Value::Object(input_of(&tools[0], &args)),
            json!({"subject": "x"})
        );
    }
}
