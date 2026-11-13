//! Catalogue resolution (plan B6 §6, port of `web/src/lib/model/logic.ts`): the `invalid_state:<s>` errors an
//! object's state machine derives per verb (11 §3.2, §6.1), the `ineligible:<rule>` guard rules, variant
//! inheritance of `operation` rows (`*` + overrides) and each operation's generated error list. Pure.
//!
//! The web semantics are kept 1:1, including JS spread semantics for attributes: a variant attribute that is
//! present overrides the `*` row's, even when it is `null`; an absent one inherits.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::kinds::catalog::{
    Discriminator, Expectation, Idempotency, IdentityLevel, ObjectFields, ObjectTransition,
    OperationAttrs, OperationFields, SideEffect,
};
use crate::kinds::split_operation_key;

/// The attribute columns a variant row may override (web `ATTR_KEYS`), in the web's order.
pub const ATTR_KEYS: [&str; 12] = [
    "modality",
    "is_derive",
    "identity_level",
    "side_effect",
    "idempotency",
    "input_fields",
    "output_fields",
    "confirm_back",
    "verify_read",
    "builtin",
    "op_kind",
    "guard_rules",
];

/// The system errors every connector-routed operation can end in (doc 10 §3.1); built-ins run in agent.
pub const SYSTEM_ERRORS: [&str; 4] = [
    "upstream_unavailable",
    "timeout",
    "rate_limited",
    "connector_auth_failed",
];

/// The `*` variant: the row that holds every column.
pub const STAR: &str = "*";

/// A backend event (`backend:<event>`) is a transition that is not an operation.
pub fn is_backend_event(verb: &str) -> bool {
    verb.starts_with("backend:")
}

/// `order.cancel` (variant `*`), `case.create{return}` (web `opKey`).
pub fn op_key(object: &str, verb: &str, variant: &str) -> String {
    if variant == STAR {
        format!("{object}.{verb}")
    } else {
        format!("{object}.{verb}{{{variant}}}")
    }
}

/// `order.cancel` → (`order`, `cancel`, `*`); `case.create{return}` → (`case`, `create`, `return`). `None` when the
/// key is not an operation key.
pub fn parse_op_key(key: &str) -> Option<(String, String, String)> {
    let (object, verb, variant) = split_operation_key(key)?;
    Some((
        object.to_owned(),
        verb.to_owned(),
        variant.unwrap_or(STAR).to_owned(),
    ))
}

/// An object as the catalogue resolves it (web `ObjectDef`): the row key plus the `object` row fields. Serialises
/// as the web type (not deserialised: build it with [`ObjectDef::from_row`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ObjectDef {
    pub object: String,
    #[serde(flatten)]
    pub def: ObjectFields,
}

impl ObjectDef {
    /// From an `object` row (`add` or catalogue `patch`: the whole item).
    pub fn from_row(key: &str, fields: &Value) -> Result<ObjectDef, String> {
        let def: ObjectFields =
            serde_json::from_value(fields.clone()).map_err(|e| format!("object {key}: {e}"))?;
        Ok(ObjectDef {
            object: key.to_owned(),
            def,
        })
    }
}

/// Per operation verb of a state machine: the states it may run from and the ones it may not (web `VerbStates`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerbStates {
    pub verb: String,
    pub allowed_from: Vec<String>,
    pub invalid_states: Vec<String>,
}

/// Every operation verb of the state machine (backend events excluded) in first-transition order, with the states
/// it may run from and every other state (web `verbStates`, "complete by construction", 11 §6.1). Both lists keep
/// the declared state order.
pub fn verb_states(def: &ObjectFields) -> Vec<VerbStates> {
    let mut verbs: Vec<&str> = Vec::new();
    for t in &def.transitions {
        if !is_backend_event(&t.verb) && !verbs.contains(&t.verb.as_str()) {
            verbs.push(t.verb.as_str());
        }
    }
    verbs
        .into_iter()
        .map(|verb| {
            let from: BTreeSet<&str> = def
                .transitions
                .iter()
                .filter(|t| t.verb == verb)
                .map(|t| t.from_status.as_str())
                .collect();
            let (allowed, invalid): (Vec<String>, Vec<String>) = def
                .states
                .iter()
                .map(|s| s.status.clone())
                .partition(|s| from.contains(s.as_str()));
            VerbStates {
                verb: verb.to_owned(),
                allowed_from: allowed,
                invalid_states: invalid,
            }
        })
        .collect()
}

/// The `invalid_state:<s>` codes of one verb (`order.cancel`); empty when the verb is not a state transition
/// (web `invalidStatesFor`).
pub fn invalid_states_for(def: &ObjectFields, verb: &str) -> Vec<String> {
    verb_states(def)
        .into_iter()
        .find(|v| v.verb == verb)
        .map(|v| {
            v.invalid_states
                .iter()
                .map(|s| format!("invalid_state:{s}"))
                .collect()
        })
        .unwrap_or_default()
}

/// The guard rules on the transitions of one verb, unique, in transition order (web `guardRulesFor`; each becomes
/// an `ineligible:<rule>` error).
pub fn guard_rules_for(transitions: &[ObjectTransition], verb: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in transitions.iter().filter(|t| t.verb == verb) {
        if let Some(rule) = t.guard_rule.as_deref()
            && !rule.is_empty()
            && !out.iter().any(|r| r == rule)
        {
            out.push(rule.to_owned());
        }
    }
    out
}

fn push_unique(out: &mut Vec<String>, code: &str) {
    if !out.iter().any(|c| c == code) {
        out.push(code.to_owned());
    }
}

/// The error list an operation can return (web `errorStatesFor`, 11 §3.2), unique, in the UI order:
/// `not_found` (read/verify) / `ambiguous` (list), `identity_required`, `invalid_input` + `conflict` (not a read),
/// `duplicate` (`key+dup`), the row's extra errors, `invalid_state:*` from the object's state machine,
/// `ineligible:*` (the attributes' guard rules, then the transitions'), the system errors (not a built-in), and
/// `unknown_outcome` (a write that is not a built-in). An unset attribute counts as the web's `undefined`.
pub fn error_states_for(
    attrs: &OperationAttrs,
    verb: &str,
    def: Option<&ObjectDef>,
    extra: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if verb == "read" || verb == "verify" {
        push_unique(&mut out, "not_found");
    }
    if verb == "list" {
        push_unique(&mut out, "ambiguous");
    }
    if attrs.identity_level != Some(IdentityLevel::Public) {
        push_unique(&mut out, "identity_required");
    }
    let writes = attrs.side_effect != Some(SideEffect::R);
    if writes {
        push_unique(&mut out, "invalid_input");
        push_unique(&mut out, "conflict");
    }
    if attrs.idempotency == Some(Idempotency::KeyDup) {
        push_unique(&mut out, "duplicate");
    }
    for e in extra {
        push_unique(&mut out, e);
    }
    let mut rules: Vec<String> = attrs.guard_rules.clone().unwrap_or_default();
    if let Some(d) = def {
        let full = format!("{}.{verb}", d.object);
        for s in invalid_states_for(&d.def, &full) {
            push_unique(&mut out, &s);
        }
        rules.extend(guard_rules_for(&d.def.transitions, &full));
    }
    for r in &rules {
        push_unique(&mut out, &format!("ineligible:{r}"));
    }
    let builtin = attrs.builtin == Some(true);
    if !builtin {
        for e in SYSTEM_ERRORS {
            push_unique(&mut out, e);
        }
        if writes {
            push_unique(&mut out, "unknown_outcome");
        }
    }
    out
}

/// A stored `operation` row (web `OperationDefRow`): the key parts plus the row fields. `raw_attrs` is the row's
/// `attrs` object as stored (which columns it sets: the JS `k in r.attrs`), `attrs` the same parsed.
#[derive(Debug, Clone, PartialEq)]
pub struct OperationDefRow {
    pub object: String,
    pub verb: String,
    pub variant: String,
    pub discriminator: Option<Discriminator>,
    pub attrs: OperationAttrs,
    pub raw_attrs: Map<String, Value>,
    pub description: String,
    pub retail: Option<Expectation>,
    pub stages: Vec<String>,
    pub extra_errors: Vec<String>,
}

impl OperationDefRow {
    /// From an `operation` row: its key (`object.verb` / `object.verb{variant}`) and fields.
    pub fn from_row(key: &str, fields: &Value) -> Result<OperationDefRow, String> {
        let (object, verb, variant) =
            parse_op_key(key).ok_or_else(|| format!("operation {key}: not an operation key"))?;
        let f: OperationFields =
            serde_json::from_value(fields.clone()).map_err(|e| format!("operation {key}: {e}"))?;
        let raw_attrs = fields
            .get("attrs")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        Ok(OperationDefRow {
            object,
            verb,
            variant,
            discriminator: f.discriminator,
            attrs: f.attrs,
            raw_attrs,
            description: f.description,
            retail: f.retail,
            stages: f.stages,
            extra_errors: f.extra_errors,
        })
    }

    /// The row's key (web `opKey`).
    pub fn key(&self) -> String {
        op_key(&self.object, &self.verb, &self.variant)
    }

    fn is_star(&self) -> bool {
        self.variant == STAR
    }
}

/// A resolved operation as the release cut copies it (web `OperationRow`): the merged attribute columns
/// (flattened, exactly the keys the `*` row or the variant sets), then the identity, which columns the variant
/// overrides / inherits, and the generated `error_states`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationRow {
    #[serde(flatten)]
    pub attrs: Map<String, Value>,
    pub key: String,
    pub object: String,
    pub verb: String,
    pub variant: String,
    pub discriminator: Option<Discriminator>,
    pub overridden: Vec<String>,
    pub inherited: Vec<String>,
    pub description: String,
    pub retail: Option<Expectation>,
    pub stages: Vec<String>,
    pub error_states: Vec<String>,
}

impl OperationRow {
    /// The merged attribute columns, parsed.
    pub fn typed_attrs(&self) -> Result<OperationAttrs, serde_json::Error> {
        serde_json::from_value(Value::Object(self.attrs.clone()))
    }
}

fn pick<T: Clone>(
    name: &str,
    own: &Option<T>,
    base: &Option<T>,
    set: &Map<String, Value>,
) -> Option<T> {
    if set.contains_key(name) {
        own.clone()
    } else {
        base.clone()
    }
}

/// `{...base.attrs, ...row.attrs}`, parsed: a column the row sets wins (even when null), else the base's.
fn merge_attrs(base: &OperationDefRow, row: &OperationDefRow) -> OperationAttrs {
    let (b, r, set) = (&base.attrs, &row.attrs, &row.raw_attrs);
    OperationAttrs {
        modality: pick("modality", &r.modality, &b.modality, set),
        is_derive: pick("is_derive", &r.is_derive, &b.is_derive, set),
        identity_level: pick("identity_level", &r.identity_level, &b.identity_level, set),
        side_effect: pick("side_effect", &r.side_effect, &b.side_effect, set),
        idempotency: pick("idempotency", &r.idempotency, &b.idempotency, set),
        input_fields: pick("input_fields", &r.input_fields, &b.input_fields, set),
        output_fields: pick("output_fields", &r.output_fields, &b.output_fields, set),
        confirm_back: pick("confirm_back", &r.confirm_back, &b.confirm_back, set),
        verify_read: pick("verify_read", &r.verify_read, &b.verify_read, set),
        builtin: pick("builtin", &r.builtin, &b.builtin, set),
        op_kind: pick("op_kind", &r.op_kind, &b.op_kind, set),
        guard_rules: pick("guard_rules", &r.guard_rules, &b.guard_rules, set),
        raises_identity_level: pick(
            "raises_identity_level",
            &r.raises_identity_level,
            &b.raises_identity_level,
            set,
        ),
    }
}

/// Resolve `operation` rows as the release cut does (web `resolveOperations`): every `*` row, in row order,
/// followed by its variants (same object and verb, in row order); a variant is merged with its `*` row column by
/// column (absent = inherit). A variant without a `*` row is not an operation and is left out, as in the web.
/// `objects` is searched first-match for each row's state machine.
pub fn resolve_operations(rows: &[OperationDefRow], objects: &[ObjectDef]) -> Vec<OperationRow> {
    let mut out: Vec<OperationRow> = Vec::new();
    for base in rows.iter().filter(|r| r.is_star()) {
        let variants = rows
            .iter()
            .filter(|r| !r.is_star() && r.object == base.object && r.verb == base.verb);
        for r in std::iter::once(base).chain(variants) {
            let mut raw = base.raw_attrs.clone();
            raw.extend(r.raw_attrs.clone());
            let attrs = merge_attrs(base, r);
            let (overridden, inherited): (Vec<String>, Vec<String>) = if r.is_star() {
                (Vec::new(), Vec::new())
            } else {
                ATTR_KEYS
                    .iter()
                    .map(|k| (*k).to_owned())
                    .partition(|k| r.raw_attrs.contains_key(k))
            };
            let def = objects.iter().find(|o| o.object == r.object);
            let mut extra = base.extra_errors.clone();
            extra.extend(r.extra_errors.iter().cloned());
            out.push(OperationRow {
                attrs: raw,
                key: r.key(),
                object: r.object.clone(),
                verb: r.verb.clone(),
                variant: r.variant.clone(),
                discriminator: r.discriminator.clone(),
                overridden,
                inherited,
                description: r.description.clone(),
                retail: r.retail,
                stages: r.stages.clone(),
                error_states: error_states_for(&attrs, &r.verb, def, &extra),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn op_keys_round_trip() {
        assert_eq!(op_key("order", "cancel", "*"), "order.cancel");
        assert_eq!(op_key("case", "create", "return"), "case.create{return}");
        assert_eq!(
            parse_op_key("case.create{return}"),
            Some(("case".to_owned(), "create".to_owned(), "return".to_owned()))
        );
        assert_eq!(
            parse_op_key("order.cancel"),
            Some(("order".to_owned(), "cancel".to_owned(), "*".to_owned()))
        );
        assert_eq!(parse_op_key("Order.cancel"), None);
    }

    #[test]
    fn raises_identity_level_survives_resolution() {
        let star = OperationDefRow::from_row(
            "credential.verify",
            &json!({"attrs": {"side_effect": "R", "identity_level": "1", "raises_identity_level": "2"}}),
        )
        .expect("star");
        let inherits = OperationDefRow::from_row("credential.verify{a}", &json!({"attrs": {}}))
            .expect("variant a");
        let own = OperationDefRow::from_row(
            "credential.verify{b}",
            &json!({"attrs": {"raises_identity_level": "3"}}),
        )
        .expect("variant b");
        let ops = resolve_operations(&[star, inherits, own], &[]);
        let level = |i: usize| ops[i].typed_attrs().expect("typed").raises_identity_level;
        assert_eq!(level(0), Some(IdentityLevel::Verified));
        assert_eq!(level(1), Some(IdentityLevel::Verified), "absent → inherit");
        assert_eq!(level(2), Some(IdentityLevel::StepUp));
        let merged = merge_attrs(
            &OperationDefRow::from_row(
                "credential.verify",
                &json!({"attrs": {"raises_identity_level": "2"}}),
            )
            .unwrap(),
            &OperationDefRow::from_row(
                "credential.verify{c}",
                &json!({"attrs": {"raises_identity_level": null}}),
            )
            .unwrap(),
        );
        assert_eq!(merged.raises_identity_level, None, "present null overrides");
    }

    #[test]
    fn a_present_null_overrides_and_an_absent_column_inherits() {
        let star = OperationDefRow::from_row(
            "order.read",
            &json!({"attrs": {"side_effect": "R", "verify_read": "order.list", "identity_level": "0"}}),
        )
        .expect("star");
        let variant = OperationDefRow::from_row(
            "order.read{by_ref}",
            &json!({"attrs": {"verify_read": null}, "extra_errors": ["outside_hours"]}),
        )
        .expect("variant");
        let ops = resolve_operations(&[variant, star], &[]);
        assert_eq!(
            ops.iter().map(|o| o.key.as_str()).collect::<Vec<_>>(),
            vec!["order.read", "order.read{by_ref}"]
        );
        let v = &ops[1];
        assert_eq!(v.attrs.get("verify_read"), Some(&Value::Null));
        assert_eq!(v.attrs.get("side_effect"), Some(&json!("R")));
        assert_eq!(v.overridden, vec!["verify_read"]);
        assert_eq!(v.inherited.len(), ATTR_KEYS.len() - 1);
        assert_eq!(
            v.error_states,
            vec![
                "not_found",
                "outside_hours",
                "upstream_unavailable",
                "timeout",
                "rate_limited",
                "connector_auth_failed"
            ]
        );
        assert_eq!(
            v.typed_attrs().expect("typed").verify_read,
            None,
            "null overrides the star's verify_read"
        );
    }
}
