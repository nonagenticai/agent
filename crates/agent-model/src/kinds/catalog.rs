//! Catalogue kinds of the canonical object model (D19–D22, 11 §1–§3): `object` (fields, relations, states and
//! transitions folded in, B6-2), `operation`, `error_state` and `department_archetype` (D60). The structs are the
//! web types (`types/model.ts`, `types/platform.ts`) minus the parts the row key carries.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use super::instruction::{self, InstructionFields};
use super::{FieldError, done, is_snake_name, require_text};

fn is_false(b: &bool) -> bool {
    !*b
}

/// R read · W reversible · X irreversible/financial/identity · P payment. Ordered by rank: R < W < X < P.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum SideEffect {
    R,
    W,
    X,
    P,
}

impl SideEffect {
    /// The web `SIDE_EFFECT_RANK`: R 0, W 1, X 2, P 3.
    pub fn rank(self) -> u8 {
        match self {
            SideEffect::R => 0,
            SideEffect::W => 1,
            SideEffect::X => 2,
            SideEffect::P => 3,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            SideEffect::R => "R",
            SideEffect::W => "W",
            SideEffect::X => "X",
            SideEffect::P => "P",
        }
    }
}

impl fmt::Display for SideEffect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Modality {
    Observe,
    Do,
    Request,
}

/// 0 public · 1 identified · 2 verified · 3 step-up · S staff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum IdentityLevel {
    #[serde(rename = "0")]
    Public,
    #[serde(rename = "1")]
    Identified,
    #[serde(rename = "2")]
    Verified,
    #[serde(rename = "3")]
    StepUp,
    #[serde(rename = "S")]
    Staff,
}

impl IdentityLevel {
    /// The one ordering of levels: 0 < 1 < 2 < 3 < S (staff is above every customer level).
    pub const fn rank(self) -> u8 {
        match self {
            Self::Public => 0,
            Self::Identified => 1,
            Self::Verified => 2,
            Self::StepUp => 3,
            Self::Staff => 4,
        }
    }

    /// The stricter of two levels.
    pub fn stricter(self, other: Self) -> Self {
        if other.rank() > self.rank() {
            other
        } else {
            self
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Idempotency {
    #[serde(rename = "nat")]
    Natural,
    #[serde(rename = "key")]
    Key,
    #[serde(rename = "key+dup")]
    KeyDup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OpKind {
    Standard,
    HostedPaymentLink,
    Builtin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ObjectTier {
    Base,
    Industry,
    Optional,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    Id,
    String,
    Text,
    Enum,
    Money,
    Int,
    Decimal,
    Bool,
    Date,
    Datetime,
    Address,
    Ref,
    List,
    Json,
    Url,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelationCardinality {
    #[serde(rename = "1:1")]
    OneToOne,
    #[serde(rename = "1:0..1")]
    OneToOptional,
    #[serde(rename = "1:n")]
    OneToMany,
    #[serde(rename = "n:1")]
    ManyToOne,
    #[serde(rename = "n:m")]
    ManyToMany,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectField {
    pub field: String,
    pub data_type: FieldType,
    #[serde(default)]
    pub pii: bool,
    /// Allowed values for `enum` fields (or the enum ref name for axis-backed fields).
    #[serde(default)]
    pub enum_values: Vec<String>,
    /// `ref` / `list` target, e.g. `party` or `order_line`.
    #[serde(default, rename = "ref")]
    pub ref_to: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub description: String,
    /// F-29: a retired field stays in the row (older operations may name it); new operations cannot.
    #[serde(default, skip_serializing_if = "is_false")]
    pub retired: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectRelation {
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub cardinality: RelationCardinality,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectState {
    pub status: String,
    pub label: String,
    /// No way out.
    #[serde(default)]
    pub terminal: bool,
    /// Part of the canonical status set connectors map onto.
    #[serde(default)]
    pub canonical: bool,
    #[serde(default)]
    pub initial: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectTransition {
    /// `order.cancel`; backend events are written `backend:<event>` and are not operations.
    pub verb: String,
    pub from_status: String,
    pub to_status: String,
    #[serde(default)]
    pub guard_rule: Option<String>,
}

/// `object` row fields: the web `ObjectDef` minus `object` (the row key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectFields {
    pub label: String,
    pub tier: ObjectTier,
    /// D20: `arrangement` is stored as a Contract specialisation.
    #[serde(default)]
    pub storage_shape_of: Option<String>,
    #[serde(default)]
    pub origin: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub in_retail_job: bool,
    #[serde(default)]
    pub fields: Vec<ObjectField>,
    #[serde(default)]
    pub relations: Vec<ObjectRelation>,
    #[serde(default)]
    pub states: Vec<ObjectState>,
    #[serde(default)]
    pub transitions: Vec<ObjectTransition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Field names snake_case and unique, state names unique, every transition between declared states.
pub(crate) fn check_object(o: &ObjectFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&o.label, "label", &mut errors);
    let mut names = BTreeSet::new();
    for (i, f) in o.fields.iter().enumerate() {
        if !is_snake_name(&f.field) {
            errors.push(FieldError::at(
                "invalid_value",
                format!("fields[{i}].field"),
                format!("field name {:?} is not snake_case", f.field),
            ));
        }
        if !names.insert(f.field.as_str()) {
            errors.push(FieldError::at(
                "invalid_value",
                format!("fields[{i}].field"),
                format!("field {:?} is declared twice", f.field),
            ));
        }
    }
    let mut states = BTreeSet::new();
    for (i, s) in o.states.iter().enumerate() {
        if s.status.trim().is_empty() || !states.insert(s.status.as_str()) {
            errors.push(FieldError::at(
                "invalid_value",
                format!("states[{i}].status"),
                format!("state {:?} is empty or declared twice", s.status),
            ));
        }
    }
    for (i, t) in o.transitions.iter().enumerate() {
        for status in [&t.from_status, &t.to_status] {
            if !states.contains(status.as_str()) {
                errors.push(FieldError::at(
                    "invalid_value",
                    format!("transitions[{i}]"),
                    format!(
                        "{} names state {status:?}, which the object does not declare",
                        t.verb
                    ),
                ));
            }
        }
    }
    done(errors)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Discriminator {
    pub field: String,
    pub value: String,
}

/// The columns an operation row sets. The `*` row holds every column (but the nullable `verify_read`); a variant
/// row only what it overrides (absent = inherit from `*`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationAttrs {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modality: Option<Modality>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_derive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_level: Option<IdentityLevel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub side_effect: Option<SideEffect>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency: Option<Idempotency>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_fields: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_fields: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_back: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_read: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builtin: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op_kind: Option<OpKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard_rules: Option<Vec<String>>,
    /// The level a verify-type operation raises the case to (spec 17 F-22); nullable, so never missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raises_identity_level: Option<IdentityLevel>,
}

impl OperationAttrs {
    /// The columns a `*` row must set and does not (`verify_read` is nullable, so never missing).
    pub fn missing(&self) -> Vec<&'static str> {
        let set = [
            ("modality", self.modality.is_some()),
            ("is_derive", self.is_derive.is_some()),
            ("identity_level", self.identity_level.is_some()),
            ("side_effect", self.side_effect.is_some()),
            ("idempotency", self.idempotency.is_some()),
            ("input_fields", self.input_fields.is_some()),
            ("output_fields", self.output_fields.is_some()),
            ("confirm_back", self.confirm_back.is_some()),
            ("builtin", self.builtin.is_some()),
            ("op_kind", self.op_kind.is_some()),
            ("guard_rules", self.guard_rules.is_some()),
        ];
        set.into_iter()
            .filter(|(_, is_set)| !is_set)
            .map(|(name, _)| name)
            .collect()
    }
}

/// The retail customer-care job's expectation (11 §4.3.5): R required, O optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Expectation {
    R,
    O,
}

/// `operation` row fields: the web `OperationDefRow` minus `object`, `verb`, `variant` (the row key
/// `object.verb` / `object.verb{variant}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationFields {
    #[serde(default)]
    pub discriminator: Option<Discriminator>,
    #[serde(default)]
    pub attrs: OperationAttrs,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub retail: Option<Expectation>,
    #[serde(default)]
    pub stages: Vec<String>,
    /// Errors the generic rules cannot know (e.g. `outside_hours` on a handover).
    #[serde(default)]
    pub extra_errors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

pub(crate) fn check_operation(o: &OperationFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    if let Some(d) = &o.discriminator {
        require_text(&d.field, "discriminator.field", &mut errors);
        require_text(&d.value, "discriminator.value", &mut errors);
    }
    done(errors)
}

/// `error_state` row fields: the web `ErrorStateDef` minus `code` (the row key).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorStateFields {
    pub user_facing: bool,
    pub retryable: bool,
    pub escalate: bool,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

pub(crate) fn check_error_state(e: &ErrorStateFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&e.description, "description", &mut errors);
    done(errors)
}

/// `department_archetype` row fields (D60): the web `DepartmentArchetype` minus `key`, `layer_id` and
/// `specialised_by` (derived from the `industry_dept` layers that extend it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepartmentArchetypeFields {
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub activities: Vec<String>,
    #[serde(default)]
    pub request_taxonomy: Vec<String>,
    #[serde(default)]
    pub knowledge_kinds: Vec<String>,
    #[serde(default)]
    pub metrics: Vec<String>,
    #[serde(default)]
    pub approval_rule_slots: Vec<String>,
    /// Owner ruling 2026-10-23: the archetype's prompt slots (slot → an `instruction` row's fields), folded at the
    /// archetype's step into every job of a department that extends it ([`crate::compile::archetype_rows`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub instructions: BTreeMap<String, InstructionFields>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

pub(crate) fn check_department_archetype(
    a: &DepartmentArchetypeFields,
) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&a.label, "label", &mut errors);
    for (slot, f) in &a.instructions {
        if !instruction::is_slot(slot) {
            errors.push(FieldError::at(
                "invalid_value",
                format!("instructions.{slot}"),
                "must be a prompt slot: identity, policy, tone, safety, escalation or format",
            ));
        }
        if let Err(more) = instruction::check(f) {
            errors.extend(more.into_iter().map(|e| {
                let field = e.field.as_deref().unwrap_or_default();
                FieldError::at(e.code, format!("instructions.{slot}.{field}"), e.message)
            }));
        }
    }
    done(errors)
}

#[cfg(test)]
mod tests {
    use super::super::{Kind, validate_fields};
    use super::*;
    use serde_json::json;

    fn codes(r: Result<(), Vec<FieldError>>) -> Vec<&'static str> {
        r.err().unwrap_or_default().iter().map(|e| e.code).collect()
    }

    fn order() -> serde_json::Value {
        json!({
            "label": "Order",
            "tier": "base",
            "storage_shape_of": null,
            "origin": "owner files",
            "description": "One purchase.",
            "in_retail_job": true,
            "fields": [
                {"field": "order_id", "data_type": "id", "pii": false, "required": true, "enum_values": [], "ref": null, "description": ""},
                {"field": "party_id", "data_type": "ref", "pii": false, "required": true, "enum_values": [], "ref": "party", "description": ""}
            ],
            "relations": [{"subject": "contract", "predicate": "governs", "object": "order", "cardinality": "1:n"}],
            "states": [
                {"status": "processing", "label": "Processing", "terminal": false, "canonical": true, "initial": true},
                {"status": "cancelled", "label": "Cancelled", "terminal": true, "canonical": true, "initial": false}
            ],
            "transitions": [{"verb": "order.cancel", "from_status": "processing", "to_status": "cancelled", "guard_rule": null}]
        })
    }

    #[test]
    fn side_effect_rank_orders_r_w_x_p() {
        assert!(SideEffect::R < SideEffect::W);
        assert!(SideEffect::W < SideEffect::X);
        assert!(SideEffect::X < SideEffect::P);
        for (i, s) in [SideEffect::R, SideEffect::W, SideEffect::X, SideEffect::P]
            .into_iter()
            .enumerate()
        {
            assert_eq!(usize::from(s.rank()), i);
            assert_eq!(serde_json::to_value(s).expect("se"), json!(s.as_str()));
        }
    }

    #[test]
    fn object_rows() {
        assert_eq!(validate_fields(Kind::Object, &order()), Ok(()));
        let parsed: ObjectFields = serde_json::from_value(order()).expect("object");
        assert_eq!(parsed.fields[1].ref_to.as_deref(), Some("party"));
        let mut bad = order();
        bad["transitions"][0]["to_status"] = json!("returned");
        assert_eq!(
            codes(validate_fields(Kind::Object, &bad)),
            vec!["invalid_value"]
        );
        let mut dup = order();
        dup["fields"][1]["field"] = json!("order_id");
        assert_eq!(
            codes(validate_fields(Kind::Object, &dup)),
            vec!["invalid_value"]
        );
        let mut cardinality = order();
        cardinality["relations"][0]["cardinality"] = json!("n:n");
        assert_eq!(
            codes(validate_fields(Kind::Object, &cardinality)),
            vec!["invalid_value"]
        );
        let mut extra = order();
        extra["object"] = json!("order");
        assert_eq!(
            codes(validate_fields(Kind::Object, &extra)),
            vec!["unknown_field"]
        );
    }

    #[test]
    fn operation_rows() {
        let star = json!({
            "discriminator": null,
            "attrs": {
                "modality": "observe", "is_derive": false, "identity_level": "0", "side_effect": "R",
                "idempotency": "nat", "input_fields": ["query"], "output_fields": ["passages"],
                "confirm_back": false, "verify_read": null, "builtin": false, "op_kind": "standard",
                "guard_rules": []
            },
            "description": "Passages + citations.",
            "retail": "R",
            "stages": ["Orientation"],
            "extra_errors": []
        });
        assert_eq!(validate_fields(Kind::Operation, &star), Ok(()));
        let parsed: OperationFields = serde_json::from_value(star).expect("op");
        assert!(parsed.attrs.missing().is_empty());
        let variant = json!({
            "discriminator": {"field": "type", "value": "return"},
            "attrs": {"side_effect": "W", "idempotency": "key+dup", "identity_level": "S"},
            "description": "Open a return case.",
            "retail": null,
            "stages": [],
            "extra_errors": ["outside_hours"]
        });
        assert_eq!(validate_fields(Kind::Operation, &variant), Ok(()));
        let parsed: OperationFields = serde_json::from_value(variant).expect("variant");
        assert_eq!(parsed.attrs.side_effect, Some(SideEffect::W));
        assert!(parsed.attrs.missing().contains(&"modality"));
        let bad = json!({"attrs": {"side_effect": "Q"}});
        assert_eq!(
            codes(validate_fields(Kind::Operation, &bad)),
            vec!["invalid_value"]
        );
    }

    #[test]
    fn error_state_and_archetype_rows() {
        let e = json!({"user_facing": true, "retryable": false, "escalate": false, "description": "No record matches."});
        assert_eq!(validate_fields(Kind::ErrorState, &e), Ok(()));
        let a = json!({
            "label": "Customer service",
            "description": "Answers customers.",
            "activities": ["answer a question"],
            "request_taxonomy": ["question"],
            "knowledge_kinds": ["faq.general"],
            "metrics": ["CSAT"],
            "approval_rule_slots": ["goodwill gesture"]
        });
        assert_eq!(validate_fields(Kind::DepartmentArchetype, &a), Ok(()));
        let mut with_layer = a.clone();
        with_layer["layer_id"] = json!("L-base");
        assert_eq!(
            codes(validate_fields(Kind::DepartmentArchetype, &with_layer)),
            vec!["unknown_field"]
        );
    }

    #[test]
    fn archetype_instructions_are_checked_like_instruction_rows() {
        let mut a = json!({
            "label": "IT service desk",
            "instructions": {
                "escalation": {"text": "Hand security incidents to a person at once.", "mode": "append"},
                "policy": {"text": "Never ask for a password.", "language": "en"}
            }
        });
        assert_eq!(validate_fields(Kind::DepartmentArchetype, &a), Ok(()));
        let parsed: DepartmentArchetypeFields =
            serde_json::from_value(a.clone()).expect("archetype");
        assert_eq!(
            parsed.instructions["escalation"].mode,
            instruction::Mode::Append
        );
        a["instructions"]["greeting"] = json!({"text": "Hi."});
        a["instructions"]["tone"] = json!({"text": " ", "language": "english"});
        let r = validate_fields(Kind::DepartmentArchetype, &a);
        let fields: Vec<String> = r
            .err()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|e| e.field)
            .collect();
        assert_eq!(
            fields,
            vec![
                "instructions.greeting",
                "instructions.tone.text",
                "instructions.tone.language"
            ]
        );
        let mut unknown =
            json!({"label": "x", "instructions": {"tone": {"text": "Calm.", "weight": 2}}});
        assert!(validate_fields(Kind::DepartmentArchetype, &unknown).is_err());
        unknown["instructions"] = json!({});
        let empty: DepartmentArchetypeFields = serde_json::from_value(unknown).expect("archetype");
        assert!(
            serde_json::to_value(&empty)
                .expect("ser")
                .get("instructions")
                .is_none()
        );
    }
}
