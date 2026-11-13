//! Step and procedure conditions (B4 T6a, F-20, R13): the model B5 validates designs against, the evaluator the
//! runner (pre/postconditions) and the orchestrator (success criteria) run, and the validator.
//!
//! A condition reads one fact of the case ([`FACT_PATHS`]) and compares it with a value:
//!
//! | Fact | What |
//! |---|---|
//! | `slots.<name>` | a qualified slot (a blank slot is no value) |
//! | `identity.level` | the case's verified identity level (0..=3) |
//! | `customer.tier` | the customer's tier, once a `party.read` step answered one |
//! | `steps.<step>.outcome` | an earlier step's outcome (its output's `outcome`, else `ok`) |
//! | `steps.<step>.out.<field>` | a field of an earlier step's output (dotted for nested fields) |
//! | `steps.<step>.out.items[<field>=<value>].<field>` | the same, read from the FIRST element of a list whose `<field>` equals `<value>` (a `$slot` or a literal); no such element: no fact |
//! | `steps.<step>.out.rows[<field>=<value>].<field>` | `rows` is virtual: the step's equal-length ARRAY outputs zipped into one object per index (scalars excluded; unequal lengths: no rows) |
//! | `case.activity` | the activity the request was qualified into |
//!
//! A missing fact makes every condition false except `absent` (never an error). Facts are case data: they travel
//! on `op.call` (never on the console, K50) and never reach a log line, an event or an audit row; only condition
//! ids do.
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The fact paths a condition may read (`*` is one name; the last `*` of `steps.*.out.*` may be dotted).
pub const FACT_PATHS: &[&str] = &[
    "slots.*",
    "identity.level",
    "customer.tier",
    "steps.*.outcome",
    "steps.*.out.*",
    "case.activity",
];

/// The longest condition id (`^[a-z][a-z0-9_]{0,31}$`).
const MAX_ID: usize = 32;

/// One condition: `fact op value`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Condition {
    /// `^[a-z][a-z0-9_]{0,31}$`, unique in its list: what a failure reports.
    pub id: String,
    /// One of [`FACT_PATHS`].
    pub fact: String,
    pub op: CondOp,
    /// The value to compare with: a scalar for `eq`/`ne`, a number or text for `gte`/`lte`, a non-empty list of
    /// scalars for `in`, none for `exists`/`absent`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CondOp {
    Eq,
    Ne,
    In,
    Exists,
    Absent,
    Gte,
    Lte,
}

/// What one earlier step left on the case.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct StepFact {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub out: Option<Value>,
}

impl StepFact {
    /// A step that answered `out`: its outcome is the output's `outcome` text, else `ok`.
    pub fn answered(out: &Value) -> StepFact {
        StepFact {
            outcome: Some(
                out.get("outcome")
                    .and_then(Value::as_str)
                    .unwrap_or("ok")
                    .to_owned(),
            ),
            out: Some(out.clone()),
        }
    }
}

/// The facts of a case a condition reads ([`FACT_PATHS`]).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CaseFacts {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub slots: BTreeMap<String, Value>,
    #[serde(default)]
    pub identity_level: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_tier: Option<String>,
    /// By step name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub steps: BTreeMap<String, StepFact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
}

impl CaseFacts {
    /// No fact at all (what a call of a step without conditions carries).
    pub fn is_empty(&self) -> bool {
        *self == CaseFacts::default()
    }

    /// The fact at `path`; `None` when the case has none (or the path is not a fact path). A JSON `null` and a
    /// blank text are no value.
    pub fn get(&self, path: &str) -> Option<Value> {
        let (head, rest) = path.split_once('.')?;
        let v = match (head, rest) {
            ("slots", name) => self.slots.get(name).cloned(),
            ("identity", "level") => Some(Value::from(self.identity_level)),
            ("customer", "tier") => self.customer_tier.clone().map(Value::String),
            ("case", "activity") => self.activity.clone().map(Value::String),
            ("steps", rest) => {
                let (step, field) = rest.split_once('.')?;
                let s = self.steps.get(step)?;
                if field == "outcome" {
                    s.outcome.clone().map(Value::String)
                } else {
                    let mut cur = s.out.clone()?;
                    for p in field.strip_prefix("out.")?.split('.') {
                        cur = match parse_filter(p) {
                            Some(Ok(f)) => {
                                let want = f.resolve(|name| self.slots.get(name).cloned())?;
                                pick(&cur, &f, &want)?
                            }
                            Some(Err(())) => return None,
                            None => child(&cur, p)?,
                        };
                    }
                    Some(cur)
                }
            }
            _ => None,
        }?;
        match &v {
            Value::Null => None,
            Value::String(s) if s.trim().is_empty() => None,
            _ => Some(v),
        }
    }
}

/// One path segment of the form `<key>[<field>=<value>]`: the list at `key`, narrowed to its first element whose
/// `field` equals `value`. `value` is a literal, or `$<name>` (a slot / request value, resolved by the caller).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Filter<'a> {
    pub key: &'a str,
    pub field: &'a str,
    pub value: &'a str,
}

/// A name inside a filter: `[A-Za-z_][A-Za-z0-9_]*`.
fn filter_name(s: &str) -> bool {
    let mut c = s.chars();
    c.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && c.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A filter literal: non-empty, none of `. [ ] = $` and no blanks at the ends.
fn filter_literal(s: &str) -> bool {
    !s.is_empty() && s.trim() == s && !s.contains(['.', '[', ']', '=', '$'])
}

/// `None`: the segment holds no `[` or `]` (a plain name). `Some(Err)`: it does but is no valid filter. The
/// segment is split from the dotted path first, so a filter's value never holds a `.`.
pub(crate) fn parse_filter(seg: &str) -> Option<Result<Filter<'_>, ()>> {
    if !seg.contains(['[', ']']) {
        return None;
    }
    let parsed = (|| {
        let inner = seg.strip_suffix(']')?;
        let (key, cond) = inner.split_once('[')?;
        let (field, value) = cond.split_once('=')?;
        let value_ok = match value.strip_prefix('$') {
            Some(name) => filter_name(name),
            None => filter_literal(value),
        };
        (filter_name(key) && filter_name(field) && value_ok).then_some(Filter { key, field, value })
    })();
    Some(parsed.ok_or(()))
}

impl Filter<'_> {
    /// The value to match: a `$name` through `get` (a missing, null or blank value is no match), else the literal.
    pub(crate) fn resolve(&self, get: impl Fn(&str) -> Option<Value>) -> Option<Value> {
        match self.value.strip_prefix('$') {
            Some(name) => get(name).filter(|v| match v {
                Value::Null => false,
                Value::String(s) => !s.trim().is_empty(),
                _ => true,
            }),
            None => Some(Value::String(self.value.to_owned())),
        }
    }
}

/// The virtual list `rows` of a step output: every ARRAY-valued output of the same length zipped into one object
/// per index (`{field: out[field][i]}`), so `rows[order_id=$order_id].status` reads canonical fields whatever the
/// connector. A scalar or object output is no part of the rows; arrays of different lengths give no rows (fail
/// closed); no array output gives none. A real output named `rows` wins over this (see [`child`]).
pub(crate) fn rows_of(out: &Value) -> Option<Value> {
    let obj = out.as_object()?;
    let cols: Vec<(&String, &Vec<Value>)> = obj
        .iter()
        .filter_map(|(k, v)| v.as_array().map(|a| (k, a)))
        .collect();
    let len = cols.first()?.1.len();
    if cols.iter().any(|(_, a)| a.len() != len) {
        tracing::debug!("step output rows: array outputs differ in length; no rows");
        return None;
    }
    Some(Value::Array(
        (0..len)
            .map(|i| {
                Value::Object(
                    cols.iter()
                        .map(|(k, a)| ((*k).clone(), a[i].clone()))
                        .collect(),
                )
            })
            .collect(),
    ))
}

/// The member `key` of a step output: the real one, else (for `rows`) the virtual [`rows_of`] list.
pub(crate) fn child(cur: &Value, key: &str) -> Option<Value> {
    cur.get(key)
        .cloned()
        .or_else(|| (key == "rows").then(|| rows_of(cur)).flatten())
}

/// The first element of the list `child(cur, f.key)` whose `f.field` equals `want` (text, a number or a bool
/// compare by their text, so `"7"` and `7` match); `None` when there is no list or no such element.
pub(crate) fn pick(cur: &Value, f: &Filter<'_>, want: &Value) -> Option<Value> {
    let text = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    let want = text(want)?;
    child(cur, f.key)?
        .as_array()?
        .iter()
        .find(|it| it.get(f.field).and_then(text).as_deref() == Some(want.as_str()))
        .cloned()
}

/// A scalar as a number, when it is one (a number, or text that parses as one).
fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// Two scalars are the same: numerically when both are numbers, else as JSON, else as text.
fn same(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (number(a), number(b)) {
        return x == y;
    }
    if a == b {
        return true;
    }
    match (a, b) {
        (Value::String(s), Value::Bool(v)) | (Value::Bool(v), Value::String(s)) => {
            s.trim() == v.to_string()
        }
        _ => false,
    }
}

/// The order of two scalars: numerically when both are numbers, else text against text (ISO dates compare).
fn order(a: &Value, b: &Value) -> Option<Ordering> {
    if let (Some(x), Some(y)) = (number(a), number(b)) {
        return x.partial_cmp(&y);
    }
    match (a, b) {
        (Value::String(x), Value::String(y)) => Some(x.as_str().cmp(y.as_str())),
        _ => None,
    }
}

/// Whether `c` holds on `f`. A missing fact is false (never an error), except for `absent`; a value the operator
/// cannot use is false too.
pub fn eval(c: &Condition, f: &CaseFacts) -> bool {
    let have = f.get(&c.fact);
    let want = c.value.as_ref();
    match (c.op, have, want) {
        (CondOp::Exists, have, _) => have.is_some(),
        (CondOp::Absent, have, _) => have.is_none(),
        (_, None, _) | (_, _, None) => false,
        (CondOp::Eq, Some(h), Some(w)) => same(&h, w),
        (CondOp::Ne, Some(h), Some(w)) => !same(&h, w),
        (CondOp::In, Some(h), Some(w)) => w
            .as_array()
            .is_some_and(|all| all.iter().any(|x| same(&h, x))),
        (CondOp::Gte, Some(h), Some(w)) => order(&h, w).is_some_and(Ordering::is_ge),
        (CondOp::Lte, Some(h), Some(w)) => order(&h, w).is_some_and(Ordering::is_le),
    }
}

/// The ids of the conditions of `cs` that do not hold on `f` (in list order); empty when all hold.
pub fn failed(cs: &[Condition], f: &CaseFacts) -> Vec<String> {
    cs.iter()
        .filter(|c| !eval(c, f))
        .map(|c| c.id.clone())
        .collect()
}

/// Why a condition list is not valid.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConditionError {
    /// The id is not `^[a-z][a-z0-9_]{0,31}$`.
    BadId(String),
    /// Two conditions share an id.
    DuplicateId(String),
    /// The fact is not one of [`FACT_PATHS`].
    UnknownPath { id: String, fact: String },
    /// The fact reads a step that does not run before this one (or does not exist).
    ForwardStepReference { id: String, step: String },
    /// The operator and the value do not go together (e.g. `in` without a list, `exists` with a value).
    BadValue { id: String },
}

impl fmt::Display for ConditionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConditionError::BadId(id) => write!(f, "condition id {id:?} is not a lowercase key"),
            ConditionError::DuplicateId(id) => write!(f, "condition id {id:?} is used twice"),
            ConditionError::UnknownPath { id, fact } => {
                write!(f, "condition {id}: {fact:?} is not a case fact")
            }
            ConditionError::ForwardStepReference { id, step } => {
                write!(f, "condition {id}: step {step:?} does not run before it")
            }
            ConditionError::BadValue { id } => {
                write!(f, "condition {id}: the value does not fit the operator")
            }
        }
    }
}

impl std::error::Error for ConditionError {}

pub(crate) fn valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    id.len() <= MAX_ID
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// A non-empty dotted name (`a`, `a.b`) with no blank segment; a segment holding `[` or `]` must be a valid
/// [`Filter`] (`items[order_id=$order_id]`).
fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.split('.')
            .all(|p| !p.trim().is_empty() && parse_filter(p).is_none_or(|f| f.is_ok()))
}

/// The step a fact path reads (`steps.<step>.…`), or `Err(())` for a path that is no fact path.
fn step_of(fact: &str) -> Result<Option<&str>, ()> {
    if matches!(fact, "identity.level" | "customer.tier" | "case.activity") {
        return Ok(None);
    }
    if let Some(name) = fact.strip_prefix("slots.") {
        return if !name.is_empty() && !name.contains('.') {
            Ok(None)
        } else {
            Err(())
        };
    }
    let rest = fact.strip_prefix("steps.").ok_or(())?;
    let (step, field) = rest.split_once('.').ok_or(())?;
    if step.is_empty() {
        return Err(());
    }
    if field == "outcome" {
        return Ok(Some(step));
    }
    match field.strip_prefix("out.") {
        Some(f) if valid_name(f) => Ok(Some(step)),
        _ => Err(()),
    }
}

fn scalar(v: &Value) -> bool {
    matches!(v, Value::String(_) | Value::Number(_) | Value::Bool(_))
}

/// Check a condition list: ids, fact paths, step references (only `step_ids`: the steps that run before the
/// condition is evaluated) and operator/value pairs. The first problem found is the error.
pub fn validate(cs: &[Condition], step_ids: &[&str]) -> Result<(), ConditionError> {
    let mut seen = std::collections::BTreeSet::new();
    for c in cs {
        if !valid_id(&c.id) {
            return Err(ConditionError::BadId(c.id.clone()));
        }
        if !seen.insert(c.id.as_str()) {
            return Err(ConditionError::DuplicateId(c.id.clone()));
        }
        match step_of(&c.fact) {
            Err(()) => {
                return Err(ConditionError::UnknownPath {
                    id: c.id.clone(),
                    fact: c.fact.clone(),
                });
            }
            Ok(Some(step)) if !step_ids.contains(&step) => {
                return Err(ConditionError::ForwardStepReference {
                    id: c.id.clone(),
                    step: step.to_owned(),
                });
            }
            Ok(_) => {}
        }
        let ok = match (c.op, c.value.as_ref()) {
            (CondOp::Exists | CondOp::Absent, v) => v.is_none_or(Value::is_null),
            (CondOp::Eq | CondOp::Ne, Some(v)) => scalar(v),
            (CondOp::Gte | CondOp::Lte, Some(v)) => {
                matches!(v, Value::Number(_) | Value::String(_))
            }
            (CondOp::In, Some(Value::Array(all))) => !all.is_empty() && all.iter().all(scalar),
            _ => false,
        };
        if !ok {
            return Err(ConditionError::BadValue { id: c.id.clone() });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cond(id: &str, fact: &str, op: CondOp, value: Option<Value>) -> Condition {
        Condition {
            id: id.into(),
            fact: fact.into(),
            op,
            value,
        }
    }

    fn facts() -> CaseFacts {
        let mut f = CaseFacts {
            identity_level: 2,
            customer_tier: Some("gold".into()),
            activity: Some("returns".into()),
            ..CaseFacts::default()
        };
        f.slots.insert("order_id".into(), json!("ORD-1"));
        f.slots.insert("blank".into(), json!("  "));
        f.steps.insert(
            "order".into(),
            StepFact::answered(&json!({"status": "delivered", "total": 59.5, "ship": {"days": 3}})),
        );
        f
    }

    #[test]
    fn operators_compare_scalars() {
        let f = facts();
        let yes = [
            cond("a", "slots.order_id", CondOp::Eq, Some(json!("ORD-1"))),
            cond("b", "identity.level", CondOp::Gte, Some(json!(2))),
            cond("c", "identity.level", CondOp::Lte, Some(json!("2"))),
            cond(
                "d",
                "customer.tier",
                CondOp::In,
                Some(json!(["silver", "gold"])),
            ),
            cond("e", "steps.order.outcome", CondOp::Eq, Some(json!("ok"))),
            cond(
                "f",
                "steps.order.out.ship.days",
                CondOp::Lte,
                Some(json!(5)),
            ),
            cond(
                "g",
                "steps.order.out.status",
                CondOp::Ne,
                Some(json!("lost")),
            ),
            cond("h", "case.activity", CondOp::Exists, None),
            cond("i", "slots.blank", CondOp::Absent, None),
        ];
        assert!(failed(&yes, &f).is_empty(), "{:?}", failed(&yes, &f));
        let no = [
            cond("a", "steps.order.out.total", CondOp::Gte, Some(json!(60))),
            cond("b", "customer.tier", CondOp::In, Some(json!("gold"))),
            cond("c", "slots.order_id", CondOp::Eq, None),
        ];
        assert_eq!(failed(&no, &f), ["a", "b", "c"]);
    }

    /// The payments of a customer: `payment.list` answers every one of them, each naming its order.
    fn payment_facts(order_slot: Option<&str>) -> CaseFacts {
        let mut f = CaseFacts::default();
        if let Some(o) = order_slot {
            f.slots.insert("order_id".into(), json!(o));
        }
        f.steps.insert(
            "payments".into(),
            StepFact::answered(&json!({"items": [
                {"order_id": "ORD-1", "payment_id": "P-1", "status": "refunded", "n": 7, "ship": {"days": 3}},
                {"order_id": "ORD-2", "payment_id": "P-2", "status": "captured", "n": 8, "ship": {"days": 5}},
                {"order_id": "ORD-2", "payment_id": "P-3", "status": "pending"},
                {"order_id": "ORD-4", "payment_id": null}
            ]})),
        );
        f
    }

    #[test]
    fn a_filter_reads_the_first_element_of_the_order() {
        let f = payment_facts(Some("ORD-2"));
        // match: the first row of ORD-2, not the first row of the list
        assert_eq!(
            f.get("steps.payments.out.items[order_id=$order_id].status"),
            Some(json!("captured"))
        );
        // a literal value, a number compared by its text, and a nested path after the filter
        assert_eq!(
            f.get("steps.payments.out.items[order_id=ORD-1].payment_id"),
            Some(json!("P-1"))
        );
        assert_eq!(
            f.get("steps.payments.out.items[n=8].payment_id"),
            Some(json!("P-2"))
        );
        assert_eq!(
            f.get("steps.payments.out.items[order_id=$order_id].ship.days"),
            Some(json!(5))
        );
        // no match, a missing filter field, a null field, a slot that is not there, a key that is no list
        for none in [
            "steps.payments.out.items[order_id=ORD-9].status",
            "steps.payments.out.items[nope=ORD-2].status",
            "steps.payments.out.items[order_id=$missing].status",
            "steps.payments.out.rows[order_id=ORD-2].status",
            "steps.payments.out.items[order_id=ORD-2].nope",
            "steps.payments.out.items[order_id=$order_id].ship.nope",
            "steps.payments.out.items[payment_id=null].order_id",
        ] {
            assert_eq!(f.get(none), None, "{none}");
        }
        assert_eq!(
            payment_facts(None).get("steps.payments.out.items[order_id=$order_id].status"),
            None,
            "no order slot"
        );
        // plain dotted paths read as before
        let plain = facts();
        assert_eq!(plain.get("steps.order.out.ship.days"), Some(json!(3)));
        assert_eq!(
            plain.get("steps.order.out.status"),
            Some(json!("delivered"))
        );
    }

    /// A step output as a connector's columnar list answer: one parallel array per declared output.
    fn columnar(out: Value) -> CaseFacts {
        let mut f = CaseFacts::default();
        f.slots.insert("order_id".into(), json!("ORD-2"));
        f.steps.insert("payments".into(), StepFact::answered(&out));
        f
    }

    #[test]
    fn rows_zip_equal_length_array_outputs() {
        let f = columnar(json!({
            "order_id": ["ORD-1", "ORD-2", "ORD-2"],
            "payment_id": ["P-1", "P-2", "P-3"],
            "status": ["refunded", "captured", "pending"],
            "customer_id": "C-1"
        }));
        assert_eq!(
            f.get("steps.payments.out.rows[order_id=$order_id].status"),
            Some(json!("captured"))
        );
        assert_eq!(
            f.get("steps.payments.out.rows[order_id=ORD-1].payment_id"),
            Some(json!("P-1"))
        );
        // the whole row has the array outputs only: the scalar is no part of rows
        assert_eq!(
            f.get("steps.payments.out.rows[order_id=$order_id]"),
            Some(json!({"order_id": "ORD-2", "payment_id": "P-2", "status": "captured"}))
        );
        assert_eq!(
            f.get("steps.payments.out.rows[order_id=ORD-9].status"),
            None
        );
        assert_eq!(
            f.get("steps.payments.out.rows[customer_id=C-1].status"),
            None,
            "a scalar output is excluded from rows"
        );
        // the bare list
        assert_eq!(
            f.get("steps.payments.out.rows")
                .and_then(|v| v.as_array().map(Vec::len)),
            Some(3)
        );
    }

    #[test]
    fn rows_of_mismatched_lengths_are_none() {
        let f = columnar(json!({
            "order_id": ["ORD-1", "ORD-2"],
            "status": ["refunded"]
        }));
        assert_eq!(f.get("steps.payments.out.rows"), None);
        assert_eq!(
            f.get("steps.payments.out.rows[order_id=$order_id].status"),
            None
        );
        // no array output at all: none either
        let g = columnar(json!({"status": "captured"}));
        assert_eq!(
            g.get("steps.payments.out.rows[status=captured].status"),
            None
        );
        // a real output named rows wins over the virtual one
        let h =
            columnar(json!({"rows": [{"order_id": "ORD-2", "status": "own"}], "x": ["a", "b"]}));
        assert_eq!(
            h.get("steps.payments.out.rows[order_id=$order_id].status"),
            Some(json!("own"))
        );
    }

    #[test]
    fn a_refund_condition_on_rows_is_unmet_without_a_payment_of_the_order() {
        let met = cond(
            "payment_found",
            "steps.payments.out.rows[order_id=$order_id].payment_id",
            CondOp::Exists,
            None,
        );
        let f = columnar(json!({"order_id": ["ORD-1"], "payment_id": ["P-1"]}));
        assert_eq!(
            failed(std::slice::from_ref(&met), &f),
            vec!["payment_found"]
        );
        let g = columnar(json!({"order_id": ["ORD-1", "ORD-2"], "payment_id": ["P-1", "P-2"]}));
        assert!(failed(std::slice::from_ref(&met), &g).is_empty());
    }

    #[test]
    fn a_condition_on_another_orders_payment_is_unmet() {
        let not_refunded = cond(
            "not_refunded",
            "steps.payments.out.items[order_id=$order_id].status",
            CondOp::Ne,
            Some(json!("refunded")),
        );
        let refundable = cond(
            "refundable",
            "steps.payments.out.items[order_id=$order_id].payment_id",
            CondOp::Exists,
            None,
        );
        let both = [not_refunded, refundable];
        // ORD-1's payment is refunded: unmet by its own row (the first row of the list is ORD-1's, too)
        assert_eq!(
            failed(&both, &payment_facts(Some("ORD-1"))),
            vec!["not_refunded"]
        );
        // ORD-2 has a captured payment: met, though the first row of the whole list is a refunded one
        assert!(failed(&both, &payment_facts(Some("ORD-2"))).is_empty());
        // ORD-9 has no payment at all: both unmet, `ne` included (a missing fact never holds)
        assert_eq!(
            failed(&both, &payment_facts(Some("ORD-9"))),
            vec!["not_refunded", "refundable"]
        );
        // ORD-4's row has a null payment_id: no refundable payment
        assert_eq!(
            failed(&both[1..], &payment_facts(Some("ORD-4"))),
            vec!["refundable"]
        );
        // no order slot, no payments step output: unmet
        assert_eq!(
            failed(&both, &payment_facts(None)),
            vec!["not_refunded", "refundable"]
        );
        assert_eq!(failed(&both, &facts()), vec!["not_refunded", "refundable"]);
    }

    #[test]
    fn the_validator_takes_one_filter_form() {
        let ok =
            |fact: &str| validate(&[cond("a", fact, CondOp::Exists, None)], &["payments"]).is_ok();
        assert!(ok("steps.payments.out.items[order_id=$order_id].status"));
        assert!(ok("steps.payments.out.items[order_id=ORD-1].ship.days"));
        assert!(ok("steps.payments.out.items[n=8].payment_id"));
        assert!(ok("steps.payments.out.items.status"));
        for bad in [
            "steps.payments.out.items[order_id].status",
            "steps.payments.out.items[order_id=].status",
            "steps.payments.out.items[=x].status",
            "steps.payments.out.items[order_id=$].status",
            "steps.payments.out.items[order_id=$a.b].status",
            "steps.payments.out.items[order_id=a.b].status",
            "steps.payments.out.items[order_id=$order_id.status",
            "steps.payments.out.items[order_id=$order_id]x.status",
            "steps.payments.out.items[[order_id=a]].status",
            "steps.payments.out.[order_id=a].status",
            "steps.payments.out.items[order id=a].status",
            "steps.payments.out.items[order_id= a].status",
            "steps.payments.out.items[a=b][c=d].status",
            "steps.payments.out.items]order_id=a[.status",
        ] {
            assert!(!ok(bad), "{bad}");
        }
        // a step that does not run before it is still refused
        let fwd = [cond(
            "a",
            "steps.later.out.items[order_id=$order_id].status",
            CondOp::Exists,
            None,
        )];
        assert!(validate(&fwd, &["payments"]).is_err());
    }

    #[test]
    fn steps_without_conditions_carry_no_facts() {
        assert!(CaseFacts::default().is_empty());
        assert_eq!(
            serde_json::to_value(CaseFacts::default()).unwrap(),
            json!({"identity_level": 0})
        );
        assert!(!facts().is_empty());
    }
}
