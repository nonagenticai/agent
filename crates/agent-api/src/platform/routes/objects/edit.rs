//! The pure edits behind the object-model writes (plan B6 §5 T11, T12c, T12f): each takes the catalogue row's
//! fields, applies one change with the fixture's validation (`web/src/lib/api/areas/model.ts`) and returns what the
//! route answers. The route then writes the whole item as the row's next revision (`write_authored`).
use std::collections::BTreeSet;

use agent_model::catalog::OperationRow;
use agent_model::kinds::catalog::{
    FieldType, Idempotency, IdentityLevel, ObjectField, ObjectFields, ObjectRelation, ObjectState,
    ObjectTransition, RelationCardinality, SideEffect,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::error::{ApiError, ApiResult};
use crate::platform::routes::{blocked, conflict, missing, unprocessable};

/// `^[a-z][a-z0-9_]{0,63}$` (the fixture's `SNAKE`, with the agent-model length bound).
pub fn snake(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && s.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A relation predicate as the seed writes them (`places`, `fulfilled by`): lowercase words, single spaces, ≤ 64.
pub fn predicate_ok(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && s.len() <= 64
        && !s.ends_with(' ')
        && !s.contains("  ")
        && chars.all(|c| c.is_ascii_lowercase() || c == '_' || c == ' ')
}

/// An enum value of the model from its wire string (`"enum"`, `"1:n"`, `"key+dup"`).
fn parse_wire<T: DeserializeOwned>(s: &str) -> Option<T> {
    serde_json::from_value(Value::String(s.trim().to_owned())).ok()
}

/// `PUT /objects/{object}/fields/{field}` body (web `FieldInput`); `field` repeats the path and may be left out.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldInput {
    #[serde(default)]
    pub field: Option<String>,
    pub data_type: String,
    #[serde(default)]
    pub pii: bool,
    #[serde(default)]
    pub enum_values: Vec<String>,
    #[serde(default, rename = "ref")]
    pub ref_to: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: String,
}

/// `PUT /objects/{object}/states/{status}` body (web `StateInput`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateInput {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub terminal: bool,
    #[serde(default)]
    pub canonical: bool,
}

/// `PUT /objects/{object}/transitions/{verb}/{from}` body (web `TransitionInput`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionInput {
    #[serde(default)]
    pub verb: Option<String>,
    #[serde(default)]
    pub from_status: Option<String>,
    pub to_status: String,
    #[serde(default)]
    pub guard_rule: Option<String>,
}

/// `PUT /objects/{object}/relations/{predicate}/{target}` body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelationInput {
    pub cardinality: String,
}

/// Add or change a field (fixture `saveObjectField`). 422 `invalid_field`, `enum_values_required`. A retired field
/// stays retired.
pub fn save_field(o: &mut ObjectFields, field: &str, input: FieldInput) -> ApiResult<ObjectField> {
    let name = field.trim();
    if input.field.as_deref().is_some_and(|b| b.trim() != name) {
        return Err(unprocessable(
            "invalid_field",
            "The body names a different field than the path.",
        ));
    }
    if !snake(name) {
        return Err(unprocessable(
            "invalid_field",
            "Field names are snake_case (letters, digits, _).",
        ));
    }
    let data_type: FieldType = parse_wire(&input.data_type).ok_or_else(|| {
        unprocessable(
            "invalid_field",
            format!("“{}” is not a field type.", input.data_type.trim()),
        )
    })?;
    let mut enum_values: Vec<String> = input
        .enum_values
        .iter()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .collect();
    if data_type == FieldType::Enum && enum_values.is_empty() {
        return Err(unprocessable(
            "enum_values_required",
            "An enum field needs at least one value.",
        ));
    }
    if data_type != FieldType::Enum {
        enum_values.clear();
    }
    let ref_to = input
        .ref_to
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned);
    if matches!(data_type, FieldType::Ref | FieldType::List)
        && ref_to.as_deref().is_some_and(|r| !snake(r))
    {
        return Err(unprocessable(
            "invalid_field",
            "A ref target is an object key.",
        ));
    }
    let at = o.fields.iter().position(|f| f.field == name);
    let f = ObjectField {
        field: name.to_owned(),
        data_type,
        pii: input.pii,
        enum_values,
        ref_to,
        required: input.required,
        description: input.description.trim().to_owned(),
        retired: at.is_some_and(|i| o.fields[i].retired),
    };
    match at {
        Some(i) => o.fields[i] = f.clone(),
        None => o.fields.push(f.clone()),
    }
    Ok(f)
}

/// Add or change a state (fixture `saveObjectState`). 422 `invalid_state_name`. A new state is initial when it is
/// the first; an existing one keeps its `initial`.
pub fn save_state(o: &mut ObjectFields, status: &str, input: StateInput) -> ApiResult<ObjectState> {
    let name = status.trim();
    if input.status.as_deref().is_some_and(|b| b.trim() != name) {
        return Err(unprocessable(
            "invalid_state_name",
            "The body names a different status than the path.",
        ));
    }
    if !snake(name) {
        return Err(unprocessable(
            "invalid_state_name",
            "Statuses are snake_case.",
        ));
    }
    let at = o.states.iter().position(|s| s.status == name);
    let label = match input.label.trim() {
        "" => name.to_owned(),
        l => l.to_owned(),
    };
    let row = ObjectState {
        status: name.to_owned(),
        label,
        terminal: input.terminal,
        canonical: input.canonical,
        initial: match at {
            Some(i) => o.states[i].initial,
            None => o.states.is_empty(),
        },
    };
    match at {
        Some(i) => o.states[i] = row.clone(),
        None => o.states.push(row.clone()),
    }
    Ok(row)
}

/// `<object>.<verb>` (an operation) or `backend:<event>`.
pub fn verb_ok(object: &str, verb: &str) -> bool {
    match verb.strip_prefix("backend:") {
        Some(event) => snake(event),
        None => verb
            .strip_prefix(object)
            .and_then(|r| r.strip_prefix('.'))
            .is_some_and(snake),
    }
}

/// Add or change the transition (verb, from) (fixture `saveObjectTransition`). 422 `invalid_verb`, `unknown_state`.
pub fn save_transition(
    o: &mut ObjectFields,
    object: &str,
    verb: &str,
    from: &str,
    input: TransitionInput,
) -> ApiResult<ObjectTransition> {
    let verb = verb.trim();
    let from = from.trim();
    if input.verb.as_deref().is_some_and(|b| b.trim() != verb) {
        return Err(unprocessable(
            "invalid_verb",
            "The body names a different verb than the path.",
        ));
    }
    if input
        .from_status
        .as_deref()
        .is_some_and(|b| b.trim() != from)
    {
        return Err(unprocessable(
            "unknown_state",
            "The body names a different from_status than the path.",
        ));
    }
    if !verb_ok(object, verb) {
        return Err(unprocessable(
            "invalid_verb",
            format!("A verb is “{object}.<verb>” (an operation) or “backend:<event>”."),
        ));
    }
    let to = input.to_status.trim();
    for s in [from, to] {
        if !o.states.iter().any(|x| x.status == s) {
            return Err(unprocessable(
                "unknown_state",
                format!("“{s}” is not a state of {object}."),
            ));
        }
    }
    let t = ObjectTransition {
        verb: verb.to_owned(),
        from_status: from.to_owned(),
        to_status: to.to_owned(),
        guard_rule: input
            .guard_rule
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(str::to_owned),
    };
    match o
        .transitions
        .iter()
        .position(|x| x.verb == t.verb && x.from_status == t.from_status)
    {
        Some(i) => o.transitions[i] = t.clone(),
        None => o.transitions.push(t.clone()),
    }
    Ok(t)
}

/// Remove the transition (verb, from). 404 `transition_not_found`.
pub fn delete_transition(o: &mut ObjectFields, verb: &str, from: &str) -> ApiResult<()> {
    match o
        .transitions
        .iter()
        .position(|x| x.verb == verb && x.from_status == from)
    {
        Some(i) => {
            o.transitions.remove(i);
            Ok(())
        }
        None => Err(missing(
            "transition_not_found",
            format!("{verb} has no transition from {from}."),
        )),
    }
}

/// 422 `invalid_predicate` unless the predicate reads as the seed's.
pub fn check_predicate(predicate: &str) -> ApiResult<()> {
    if predicate_ok(predicate) {
        Ok(())
    } else {
        Err(unprocessable(
            "invalid_predicate",
            "A predicate is lowercase words (letters and _), single spaces, at most 64 characters.",
        ))
    }
}

/// Add or change the relation `object —predicate→ target` (F-28). The route has checked the predicate and that the
/// target is a catalogue object. 422 `invalid_cardinality`.
pub fn save_relation(
    o: &mut ObjectFields,
    object: &str,
    predicate: &str,
    target: &str,
    input: RelationInput,
) -> ApiResult<ObjectRelation> {
    let cardinality: RelationCardinality = parse_wire(&input.cardinality).ok_or_else(|| {
        unprocessable(
            "invalid_cardinality",
            "A cardinality is one of 1:1, 1:0..1, 1:n, n:1, n:m.",
        )
    })?;
    let r = ObjectRelation {
        subject: object.to_owned(),
        predicate: predicate.to_owned(),
        object: target.to_owned(),
        cardinality,
    };
    match o
        .relations
        .iter()
        .position(|x| x.subject == object && x.predicate == predicate && x.object == target)
    {
        Some(i) => o.relations[i] = r.clone(),
        None => o.relations.push(r.clone()),
    }
    Ok(r)
}

/// Remove the relation `object —predicate→ target`. 404 `relation_not_found`.
pub fn delete_relation(
    o: &mut ObjectFields,
    object: &str,
    predicate: &str,
    target: &str,
) -> ApiResult<()> {
    match o
        .relations
        .iter()
        .position(|x| x.subject == object && x.predicate == predicate && x.object == target)
    {
        Some(i) => {
            o.relations.remove(i);
            Ok(())
        }
        None => Err(missing(
            "relation_not_found",
            format!("{object} has no relation “{predicate}” to {target}."),
        )),
    }
}

fn names<'a>(attrs: &'a Map<String, Value>, column: &str) -> Vec<&'a str> {
    attrs
        .get(column)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

/// The resolved operations whose `input_fields`, `output_fields` or discriminator name `field`.
pub fn operations_naming(ops: &[&OperationRow], field: &str) -> Vec<String> {
    ops.iter()
        .filter(|r| {
            names(&r.attrs, "input_fields").contains(&field)
                || names(&r.attrs, "output_fields").contains(&field)
                || r.discriminator.as_ref().is_some_and(|d| d.field == field)
        })
        .map(|r| r.key.clone())
        .collect()
}

/// Retire a field (F-29): 404 `field_not_found`; 409 `field_retired`, `field_in_use{operations}`.
pub fn retire_field(
    o: &mut ObjectFields,
    object: &str,
    field: &str,
    ops: &[&OperationRow],
) -> ApiResult<ObjectField> {
    let Some(i) = o.fields.iter().position(|f| f.field == field) else {
        return Err(missing(
            "field_not_found",
            format!("{object} has no field {field}."),
        ));
    };
    if o.fields[i].retired {
        return Err(conflict(
            "field_retired",
            format!("{object}.{field} is already retired."),
        ));
    }
    let using = operations_naming(ops, field);
    if !using.is_empty() {
        return Err(blocked(
            "field_in_use",
            format!(
                "{object}.{field} is used by {}; change those operations first.",
                using.join(", ")
            ),
            "operations",
            using,
        ));
    }
    o.fields[i].retired = true;
    Ok(o.fields[i].clone())
}

/// One line per change between two revisions of an object (P10 `pending_edits.what`).
pub fn describe_change(prev: Option<&ObjectFields>, cur: Option<&ObjectFields>) -> String {
    let (Some(prev), Some(cur)) = (prev, cur) else {
        return match (prev, cur) {
            (None, Some(_)) => "object added".to_owned(),
            (_, None) => "object retired".to_owned(),
            _ => "object changed".to_owned(),
        };
    };
    let mut out: Vec<String> = Vec::new();
    for f in &cur.fields {
        match prev.fields.iter().find(|p| p.field == f.field) {
            None => out.push(format!("field {} added", f.field)),
            Some(p) if !p.retired && f.retired => out.push(format!("field {} retired", f.field)),
            Some(p) if p != f => out.push(format!("field {} changed", f.field)),
            Some(_) => {}
        }
    }
    for p in &prev.fields {
        if !cur.fields.iter().any(|f| f.field == p.field) {
            out.push(format!("field {} removed", p.field));
        }
    }
    for s in &cur.states {
        match prev.states.iter().find(|p| p.status == s.status) {
            None => out.push(format!("state {} added", s.status)),
            Some(p) if p != s => out.push(format!("state {} changed", s.status)),
            Some(_) => {}
        }
    }
    for p in &prev.states {
        if !cur.states.iter().any(|s| s.status == p.status) {
            out.push(format!("state {} removed", p.status));
        }
    }
    let same_t = |a: &ObjectTransition, b: &ObjectTransition| {
        a.verb == b.verb && a.from_status == b.from_status
    };
    for t in &cur.transitions {
        match prev.transitions.iter().find(|p| same_t(p, t)) {
            None => out.push(format!(
                "transition {}: {} → {} added",
                t.verb, t.from_status, t.to_status
            )),
            Some(p) if p != t => out.push(format!(
                "transition {}: {} → {} changed",
                t.verb, t.from_status, t.to_status
            )),
            Some(_) => {}
        }
    }
    for p in &prev.transitions {
        if !cur.transitions.iter().any(|t| same_t(p, t)) {
            out.push(format!(
                "transition {} from {} removed",
                p.verb, p.from_status
            ));
        }
    }
    let same_r = |a: &ObjectRelation, b: &ObjectRelation| {
        a.subject == b.subject && a.predicate == b.predicate && a.object == b.object
    };
    for r in &cur.relations {
        let what = format!("relation {} {} {}", r.subject, r.predicate, r.object);
        match prev.relations.iter().find(|p| same_r(p, r)) {
            None => out.push(format!("{what} added")),
            Some(p) if p != r => out.push(format!("{what} changed")),
            Some(_) => {}
        }
    }
    for p in &prev.relations {
        if !cur.relations.iter().any(|r| same_r(p, r)) {
            out.push(format!(
                "relation {} {} {} removed",
                p.subject, p.predicate, p.object
            ));
        }
    }
    if prev.label != cur.label
        || prev.tier != cur.tier
        || prev.storage_shape_of != cur.storage_shape_of
        || prev.origin != cur.origin
        || prev.description != cur.description
        || prev.in_retail_job != cur.in_retail_job
    {
        out.push("details changed".to_owned());
    }
    if out.is_empty() {
        "no change".to_owned()
    } else {
        out.join("; ")
    }
}

/// `PUT /api/platform/operations/{object}/{verb}?variant=` body (spec 17 F-22 `OperationEdit`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationEdit {
    /// Required, but a missing field deserialises to "" so [`apply_operation_edit`] answers 422
    /// `identity_level_required` (a field error), not a generic 400 body error, and never a default level.
    #[serde(default)]
    pub identity_level: String,
    #[serde(default)]
    pub raises_identity_level: Option<String>,
    pub side_effect: String,
    pub confirm_back: bool,
    #[serde(default)]
    pub verify_read: Option<String>,
    pub idempotency: String,
    #[serde(default)]
    pub input_fields: Vec<String>,
    #[serde(default)]
    pub output_fields: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub extra_errors: Vec<String>,
}

/// What an operation edit is checked against.
pub struct OperationContext<'a> {
    /// The stored row's fields (as written: a variant row holds only what it overrides).
    pub row_fields: &'a Value,
    /// `None` for a `*` row; for a variant, the `*` row's stored attrs and extra errors.
    pub star: Option<(&'a Map<String, Value>, &'a [String])>,
    pub verb: &'a str,
    /// The operation as resolved now (its current input/output names are kept even when not object fields).
    pub current: &'a OperationRow,
    /// The object's fields.
    pub fields: &'a [ObjectField],
    /// Every resolved operation key whose side effect is R.
    pub read_ops: &'a BTreeSet<String>,
}

fn trimmed_unique(list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in list.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !out.iter().any(|x| x == s) {
            out.push(s.to_owned());
        }
    }
    out
}

/// Apply an operation edit (T12c): the new stored fields of the row. 422 `identity_level_required` (no level sent:
/// there is no default level), `invalid_identity_level`,
/// `invalid_operation` (incl. `raises_identity_level` on a non-verify op), `unknown_field{fields}`,
/// `unknown_verify_read`. The route refuses a built-in first (409).
///
/// A variant row keeps inheriting: a column whose edited value equals the `*` row's is left unset (inherit), any
/// other is set on the variant. Extra errors the `*` row already lists are not repeated on a variant.
pub fn apply_operation_edit(cx: &OperationContext<'_>, edit: OperationEdit) -> ApiResult<Value> {
    if edit.identity_level.trim().is_empty() {
        return Err(unprocessable(
            "identity_level_required",
            "identity_level is required: 0, 1, 2, 3 or S. An operation has no default level.",
        ));
    }
    let identity: IdentityLevel = parse_wire(&edit.identity_level).ok_or_else(|| {
        unprocessable(
            "invalid_identity_level",
            "An identity level is 0, 1, 2, 3 or S.",
        )
    })?;
    crate::runtime::catalog::require_contract_floor(&cx.current.key, identity)?;
    let raises: Option<IdentityLevel> = match edit.raises_identity_level.as_deref() {
        None => None,
        Some(raw) => {
            let level = parse_wire::<IdentityLevel>(raw).ok_or_else(|| {
                unprocessable(
                    "invalid_identity_level",
                    "raises_identity_level is 0, 1, 2, 3 or S.",
                )
            })?;
            if cx.verb != "verify" {
                return Err(unprocessable(
                    "invalid_operation",
                    "Only a verify operation raises the identity level.",
                ));
            }
            Some(level)
        }
    };
    let side: SideEffect = parse_wire(&edit.side_effect)
        .ok_or_else(|| unprocessable("invalid_operation", "A side effect is R, W, X or P."))?;
    let idem: Idempotency = parse_wire(&edit.idempotency)
        .ok_or_else(|| unprocessable("invalid_operation", "Idempotency is nat, key or key+dup."))?;
    let verify_read = edit
        .verify_read
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    if let Some(v) = &verify_read
        && !cx.read_ops.contains(v)
    {
        return Err(unprocessable(
            "unknown_verify_read",
            format!("“{v}” is not a read operation of the catalogue."),
        ));
    }
    let mut input = trimmed_unique(&edit.input_fields);
    let mut output = trimmed_unique(&edit.output_fields);
    // a contract operation: empty takes the contract's declared fields, given must be a subset of them
    crate::runtime::catalog::fill_contract_io(&cx.current.key, &mut input, &mut output)?;
    let contract = crate::runtime::catalog::contract_def(&cx.current.key);
    let live: BTreeSet<&str> = cx
        .fields
        .iter()
        .filter(|f| !f.retired)
        .map(|f| f.field.as_str())
        .collect();
    let mut kept: BTreeSet<&str> = names(&cx.current.attrs, "input_fields")
        .into_iter()
        .collect();
    kept.extend(names(&cx.current.attrs, "output_fields"));
    let mut unknown: Vec<String> = Vec::new();
    for n in input.iter().chain(output.iter()) {
        let declared =
            contract.is_some_and(|d| d.input_fields.contains(n) || d.output_fields.contains(n));
        if !declared
            && !live.contains(n.as_str())
            && !kept.contains(n.as_str())
            && !unknown.contains(n)
        {
            unknown.push(n.clone());
        }
    }
    if !unknown.is_empty() {
        return Err(ApiError::Unprocessable(
            "unknown_field",
            format!(
                "Not a field of {}: {}.",
                cx.current.object,
                unknown.join(", ")
            ),
        )
        .with("fields", unknown));
    }
    let mut extra = trimmed_unique(&edit.extra_errors);
    if let Some(bad) = extra.iter().find(|e| !snake(e)) {
        return Err(unprocessable(
            "invalid_operation",
            format!("“{bad}” is not an error code (snake_case)."),
        ));
    }
    let edited: [(&str, Value); 7] = [
        ("identity_level", json!(identity)),
        ("side_effect", json!(side)),
        ("confirm_back", json!(edit.confirm_back)),
        ("verify_read", json!(verify_read)),
        ("idempotency", json!(idem)),
        ("input_fields", json!(input)),
        ("output_fields", json!(output)),
    ];
    let mut attrs: Map<String, Value> = cx
        .row_fields
        .get("attrs")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for (column, value) in edited {
        match cx.star {
            Some((star, _)) if star.get(column) == Some(&value) => {
                attrs.remove(column);
            }
            _ => {
                attrs.insert(column.to_owned(), value);
            }
        }
    }
    // `raises_identity_level` is a verify op's column only (spec 17 F-22). It is nullable: unset on the `*` row clears
    // it; a variant inherits when it equals the `*` row's (both unset included), else sets it (null overrides a `*`
    // value).
    if cx.verb == "verify" {
        let column = "raises_identity_level";
        let value = json!(raises);
        let inherits = match cx.star {
            Some((star, _)) => star.get(column).map_or(value.is_null(), |s| s == &value),
            None => value.is_null(),
        };
        if inherits {
            attrs.remove(column);
        } else {
            attrs.insert(column.to_owned(), value);
        }
    }
    if let Some((_, star_extra)) = cx.star {
        extra.retain(|e| !star_extra.contains(e));
    }
    let mut fields = match cx.row_fields {
        Value::Object(m) => m.clone(),
        _ => Map::new(),
    };
    fields.insert("attrs".to_owned(), Value::Object(attrs));
    fields.insert(
        "description".to_owned(),
        Value::String(edit.description.trim().to_owned()),
    );
    fields.insert("extra_errors".to_owned(), json!(extra));
    Ok(Value::Object(fields))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_model::catalog::{OperationDefRow, resolve_operations};
    use agent_model::kinds::catalog::ObjectTier;

    fn order() -> ObjectFields {
        serde_json::from_value(json!({
            "label": "Order", "tier": "base",
            "fields": [
                {"field": "order_id", "data_type": "id"},
                {"field": "status", "data_type": "enum", "enum_values": ["processing", "cancelled"]},
                {"field": "gift_note", "data_type": "text"}
            ],
            "relations": [{"subject": "order", "predicate": "fulfilled by", "object": "delivery", "cardinality": "1:n"}],
            "states": [
                {"status": "processing", "label": "Processing", "initial": true},
                {"status": "cancelled", "label": "Cancelled", "terminal": true}
            ],
            "transitions": [{"verb": "order.cancel", "from_status": "processing", "to_status": "cancelled"}]
        }))
        .expect("order")
    }

    fn field_input(dt: &str, values: &[&str]) -> FieldInput {
        FieldInput {
            field: None,
            data_type: dt.into(),
            pii: false,
            enum_values: values.iter().map(|v| (*v).to_owned()).collect(),
            ref_to: None,
            required: false,
            description: " d ".into(),
        }
    }

    fn code<T: std::fmt::Debug>(r: ApiResult<T>) -> &'static str {
        r.expect_err("refusal").code()
    }

    #[test]
    fn fields_follow_the_fixture() {
        let mut o = order();
        let f = save_field(&mut o, "channel", field_input("enum", &[" web ", ""])).unwrap();
        assert_eq!(f.enum_values, vec!["web"]);
        assert_eq!(f.description, "d");
        assert_eq!(o.fields.len(), 4);
        let f = save_field(&mut o, "channel", field_input("string", &["x"])).unwrap();
        assert!(f.enum_values.is_empty(), "non-enum clears values");
        assert_eq!(o.fields.len(), 4, "upsert by name");
        assert_eq!(
            code(save_field(&mut o, "Bad", field_input("text", &[]))),
            "invalid_field"
        );
        assert_eq!(
            code(save_field(&mut o, "x", field_input("blob", &[]))),
            "invalid_field"
        );
        assert_eq!(
            code(save_field(&mut o, "x", field_input("enum", &[" "]))),
            "enum_values_required"
        );
        let mut r = field_input("ref", &[]);
        r.ref_to = Some("Order".into());
        assert_eq!(code(save_field(&mut o, "x", r)), "invalid_field");
        let mut other = field_input("text", &[]);
        other.field = Some("y".into());
        assert_eq!(code(save_field(&mut o, "x", other)), "invalid_field");
    }

    #[test]
    fn retired_field_stays_retired_on_save() {
        let mut o = order();
        o.fields[2].retired = true;
        let f = save_field(&mut o, "gift_note", field_input("text", &[])).unwrap();
        assert!(f.retired);
    }

    #[test]
    fn states_and_transitions() {
        let mut o = order();
        let s = save_state(
            &mut o,
            "returned",
            StateInput {
                status: None,
                label: " ".into(),
                terminal: true,
                canonical: true,
            },
        )
        .unwrap();
        assert_eq!(s.label, "returned");
        assert!(!s.initial);
        assert_eq!(
            code(save_state(
                &mut o,
                "Re turned",
                StateInput {
                    status: None,
                    label: String::new(),
                    terminal: false,
                    canonical: false
                }
            )),
            "invalid_state_name"
        );
        let t = TransitionInput {
            verb: None,
            from_status: None,
            to_status: "returned".into(),
            guard_rule: Some("  ".into()),
        };
        let saved =
            save_transition(&mut o, "order", "order.return", "cancelled", t.clone()).unwrap();
        assert_eq!(saved.guard_rule, None);
        assert_eq!(o.transitions.len(), 2);
        assert!(
            save_transition(&mut o, "order", "backend:shipped", "processing", t.clone()).is_ok()
        );
        assert_eq!(
            code(save_transition(
                &mut o,
                "order",
                "case.close",
                "processing",
                t.clone()
            )),
            "invalid_verb"
        );
        let mut bad = t.clone();
        bad.to_status = "lost".into();
        assert_eq!(
            code(save_transition(
                &mut o,
                "order",
                "order.lose",
                "processing",
                bad
            )),
            "unknown_state"
        );
        assert_eq!(
            code(delete_transition(&mut o, "order.cancel", "cancelled")),
            "transition_not_found"
        );
        delete_transition(&mut o, "order.cancel", "processing").unwrap();
        assert!(!o.transitions.iter().any(|x| x.verb == "order.cancel"));
    }

    #[test]
    fn relations() {
        let mut o = order();
        assert!(check_predicate("fulfilled by").is_ok());
        for bad in ["", "Places", "two  spaces", "trailing ", "a-b"] {
            assert_eq!(code(check_predicate(bad)), "invalid_predicate", "{bad}");
        }
        let r = save_relation(
            &mut o,
            "order",
            "paid by",
            "payment",
            RelationInput {
                cardinality: "1:n".into(),
            },
        )
        .unwrap();
        assert_eq!(r.cardinality, RelationCardinality::OneToMany);
        save_relation(
            &mut o,
            "order",
            "paid by",
            "payment",
            RelationInput {
                cardinality: "n:m".into(),
            },
        )
        .unwrap();
        assert_eq!(o.relations.len(), 2, "upsert by (predicate, target)");
        assert_eq!(
            code(save_relation(
                &mut o,
                "order",
                "paid by",
                "payment",
                RelationInput {
                    cardinality: "n:n".into()
                }
            )),
            "invalid_cardinality"
        );
        delete_relation(&mut o, "order", "paid by", "payment").unwrap();
        assert_eq!(
            code(delete_relation(&mut o, "order", "paid by", "payment")),
            "relation_not_found"
        );
    }

    fn ops() -> Vec<OperationRow> {
        let star = OperationDefRow::from_row(
            "order.read",
            &json!({"attrs": {"side_effect": "R", "identity_level": "2", "input_fields": ["order_id"],
                              "output_fields": ["status"], "builtin": false}}),
        )
        .unwrap();
        let disc = OperationDefRow::from_row(
            "order.read{by_status}",
            &json!({"discriminator": {"field": "status", "value": "x"}, "attrs": {}}),
        )
        .unwrap();
        resolve_operations(&[star, disc], &[])
    }

    #[test]
    fn retire_field_checks_use() {
        let mut o = order();
        let rows = ops();
        let refs: Vec<&OperationRow> = rows.iter().collect();
        let e = retire_field(&mut o, "order", "status", &refs).unwrap_err();
        assert_eq!(e.code(), "field_in_use");
        let ApiError::Detailed { extra, .. } = e else {
            panic!("detailed")
        };
        assert_eq!(
            extra.get("operations"),
            Some(&json!(["order.read", "order.read{by_status}"]))
        );
        assert_eq!(
            code(retire_field(&mut o, "order", "nope", &refs)),
            "field_not_found"
        );
        assert!(
            retire_field(&mut o, "order", "gift_note", &refs)
                .unwrap()
                .retired
        );
        assert_eq!(
            code(retire_field(&mut o, "order", "gift_note", &refs)),
            "field_retired"
        );
    }

    #[test]
    fn describes_changes() {
        let a = order();
        let mut b = order();
        b.fields[2].retired = true;
        b.transitions.clear();
        b.tier = ObjectTier::Industry;
        assert_eq!(
            describe_change(Some(&a), Some(&b)),
            "field gift_note retired; transition order.cancel from processing removed; details changed"
        );
        assert_eq!(describe_change(None, Some(&a)), "object added");
        assert_eq!(describe_change(Some(&a), None), "object retired");
        assert_eq!(describe_change(Some(&a), Some(&a)), "no change");
    }

    fn edit() -> OperationEdit {
        OperationEdit {
            identity_level: "2".into(),
            raises_identity_level: None,
            side_effect: "R".into(),
            confirm_back: false,
            verify_read: None,
            idempotency: "nat".into(),
            input_fields: vec!["order_id".into()],
            output_fields: vec!["status".into(), "status".into()],
            description: " One order. ".into(),
            extra_errors: vec![],
        }
    }

    #[test]
    fn operation_edit_star_and_variant() {
        let o = order();
        let rows = ops();
        let reads: BTreeSet<String> = ["order.read".to_owned()].into_iter().collect();
        let star_fields = json!({"attrs": {"side_effect": "R", "identity_level": "2", "input_fields": ["order_id"],
                                           "output_fields": ["status"], "builtin": false}, "description": "x"});
        let cx = OperationContext {
            row_fields: &star_fields,
            star: None,
            verb: "read",
            current: &rows[0],
            fields: &o.fields,
            read_ops: &reads,
        };
        let mut e = edit();
        e.confirm_back = true;
        let v = apply_operation_edit(&cx, e).unwrap();
        assert_eq!(v["attrs"]["confirm_back"], json!(true));
        assert_eq!(v["attrs"]["builtin"], json!(false), "other columns kept");
        assert_eq!(v["attrs"]["output_fields"], json!(["status"]));
        assert_eq!(v["description"], json!("One order."));

        let star_attrs = star_fields["attrs"].as_object().unwrap().clone();
        let variant_fields = json!({"discriminator": {"field": "status", "value": "x"}, "attrs": {"side_effect": "W"}});
        let no_extra: Vec<String> = vec![];
        let cx = OperationContext {
            row_fields: &variant_fields,
            star: Some((&star_attrs, &no_extra)),
            verb: "read",
            current: &rows[1],
            fields: &o.fields,
            read_ops: &reads,
        };
        let v = apply_operation_edit(&cx, edit()).unwrap();
        let attrs = v["attrs"].as_object().unwrap();
        assert!(
            !attrs.contains_key("side_effect"),
            "equal to * → inherit: {attrs:?}"
        );
        assert_eq!(attrs.get("confirm_back"), Some(&json!(false)), "* had none");
        assert_eq!(v["discriminator"]["field"], json!("status"));
    }

    #[test]
    fn operation_edit_refusals() {
        let o = order();
        let rows = ops();
        let reads: BTreeSet<String> = ["order.read".to_owned()].into_iter().collect();
        let f = json!({"attrs": {}});
        let cx = OperationContext {
            row_fields: &f,
            star: None,
            verb: "read",
            current: &rows[0],
            fields: &o.fields,
            read_ops: &reads,
        };
        let mut e = edit();
        e.input_fields = vec!["nope".into()];
        // order.read is a contract operation: the contract check answers before the object-field check
        assert_eq!(code(apply_operation_edit(&cx, e)), "field_not_in_contract");
        // a custom operation (no contract entry) still gets unknown_field
        let custom = resolve_operations(
            &[OperationDefRow::from_row(
                "order.peek",
                &json!({"attrs": {"side_effect": "R", "identity_level": "2", "input_fields": ["order_id"],
                                  "output_fields": ["status"], "builtin": false}}),
            )
            .unwrap()],
            &[],
        );
        let ccx = OperationContext {
            current: &custom[0],
            verb: "peek",
            ..cx
        };
        let mut e = edit();
        e.input_fields = vec!["nope".into()];
        assert_eq!(code(apply_operation_edit(&ccx, e)), "unknown_field");
        let mut e = edit();
        e.verify_read = Some("order.cancel".into());
        assert_eq!(code(apply_operation_edit(&cx, e)), "unknown_verify_read");
        let mut e = edit();
        e.identity_level = "4".into();
        assert_eq!(code(apply_operation_edit(&cx, e)), "invalid_identity_level");
        // no level sent (the field defaults to ""): a field error, never a default level
        let mut e = edit();
        e.identity_level = String::new();
        assert_eq!(
            code(apply_operation_edit(&cx, e)),
            "identity_level_required"
        );
        // raises_identity_level is a verify op's column only: a read op is refused, even with a valid level
        let mut e = edit();
        e.raises_identity_level = Some("3".into());
        assert_eq!(code(apply_operation_edit(&cx, e)), "invalid_operation");
        let mut e = edit();
        e.raises_identity_level = Some("7".into());
        assert_eq!(code(apply_operation_edit(&cx, e)), "invalid_identity_level");
        let mut e = edit();
        e.idempotency = "once".into();
        assert_eq!(code(apply_operation_edit(&cx, e)), "invalid_operation");
        let mut e = edit();
        e.extra_errors = vec!["Bad Code".into()];
        assert_eq!(code(apply_operation_edit(&cx, e)), "invalid_operation");
    }

    #[test]
    fn raises_identity_level_stored_on_verify_op() {
        use agent_model::kinds::catalog::OperationAttrs;
        let o = order();
        let rows = ops();
        let reads: BTreeSet<String> = ["order.read".to_owned()].into_iter().collect();
        let star_fields =
            json!({"attrs": {"side_effect": "R", "identity_level": "1", "builtin": false}});
        let cx = OperationContext {
            row_fields: &star_fields,
            star: None,
            verb: "verify",
            current: &rows[0],
            fields: &o.fields,
            read_ops: &reads,
        };
        let mut e = edit();
        e.raises_identity_level = Some("2".into());
        let v = apply_operation_edit(&cx, e).unwrap();
        assert_eq!(v["attrs"]["raises_identity_level"], json!("2"));
        // round trip: the stored attrs parse back (deny_unknown_fields) with the level
        let typed: OperationAttrs = serde_json::from_value(v["attrs"].clone()).unwrap();
        assert_eq!(typed.raises_identity_level, Some(IdentityLevel::Verified));

        // clearing: an edit without it removes the column from a `*` row that had it
        let cx = OperationContext {
            row_fields: &v,
            ..cx
        };
        let cleared = apply_operation_edit(&cx, edit()).unwrap();
        assert!(
            !cleared["attrs"]
                .as_object()
                .unwrap()
                .contains_key("raises_identity_level")
        );

        // a variant: equal to the `*` row's inherits; a different level is set; unset overrides a `*` value with null
        let star_attrs = v["attrs"].as_object().unwrap().clone();
        let no_extra: Vec<String> = vec![];
        let variant_fields = json!({"attrs": {"raises_identity_level": "3"}});
        let cx = OperationContext {
            row_fields: &variant_fields,
            star: Some((&star_attrs, &no_extra)),
            verb: "verify",
            current: &rows[1],
            fields: &o.fields,
            read_ops: &reads,
        };
        let mut e = edit();
        e.raises_identity_level = Some("2".into());
        let same = apply_operation_edit(&cx, e).unwrap();
        assert!(
            !same["attrs"]
                .as_object()
                .unwrap()
                .contains_key("raises_identity_level"),
            "equal to * → inherit"
        );
        let mut e = edit();
        e.raises_identity_level = Some("3".into());
        let own = apply_operation_edit(&cx, e).unwrap();
        assert_eq!(own["attrs"]["raises_identity_level"], json!("3"));
        let unset = apply_operation_edit(&cx, edit()).unwrap();
        assert_eq!(unset["attrs"]["raises_identity_level"], Value::Null);
    }
}
