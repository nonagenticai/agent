//! The 30 kinds of a layer row (plan B6 §3, plus `knowledge_pack` of plan PK-1, `instruction` of plan KH T4,
//! D108, and `journey_channel`, `stage_scope`, `use_case_card` of plan B8 T1, and `rule_slot`, 0222): which scopes allow them, their key shapes, their field schemas (`deny_unknown_fields`, checked on every
//! write and on seed), a one-line summary and a label.
//!
//! The eight overlay kinds (entity … knowledge_store) are the configurator's compile kinds: a `patch` names
//! only the fields it changes. Every other kind is a catalogue item whose row holds the whole item, so `add`
//! and `patch` rows are both checked as complete items. A `tombstone` carries no fields but an optional
//! `reason`.

pub mod catalog;
pub mod gate;
pub mod instruction;
pub mod journey;
pub mod knowledge_pack;
pub mod overlay;
pub mod profile;
pub mod role;
pub mod rule_slot;
pub mod stage_knowledge;

use std::fmt;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::axis::Condition;
use crate::canonical::canonical_json;
use crate::layer::Scope;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Entity,
    Attribute,
    Relation,
    Job,
    ContextSpace,
    Procedure,
    ConnectorSlot,
    KnowledgeStore,
    Object,
    Operation,
    ErrorState,
    GateQuestion,
    AgentRole,
    Profile,
    AxisRule,
    DepartmentArchetype,
    JourneyTheme,
    JourneyStage,
    ThemeStagePrior,
    StageKnowledgeKind,
    Activity,
    ActivityObject,
    OperationExpectation,
    JobSetting,
    KnowledgePack,
    Instruction,
    JourneyChannel,
    StageScope,
    UseCaseCard,
    RuleSlot,
}

const EVERY_SCOPE: &[Scope] = &[
    Scope::Base,
    Scope::Profile,
    Scope::Industry,
    Scope::BaseDept,
    Scope::IndustryDept,
    Scope::Job,
];
/// Template layers: every scope but `profile` (a profile layer holds only the rows its axis rule adds).
const TEMPLATE_SCOPES: &[Scope] = &[
    Scope::Base,
    Scope::Industry,
    Scope::BaseDept,
    Scope::IndustryDept,
    Scope::Job,
];

impl Kind {
    pub const ALL: [Kind; 30] = [
        Kind::Entity,
        Kind::Attribute,
        Kind::Relation,
        Kind::Job,
        Kind::ContextSpace,
        Kind::Procedure,
        Kind::ConnectorSlot,
        Kind::KnowledgeStore,
        Kind::Object,
        Kind::Operation,
        Kind::ErrorState,
        Kind::GateQuestion,
        Kind::AgentRole,
        Kind::Profile,
        Kind::AxisRule,
        Kind::DepartmentArchetype,
        Kind::JourneyTheme,
        Kind::JourneyStage,
        Kind::ThemeStagePrior,
        Kind::StageKnowledgeKind,
        Kind::Activity,
        Kind::ActivityObject,
        Kind::OperationExpectation,
        Kind::JobSetting,
        Kind::KnowledgePack,
        Kind::Instruction,
        Kind::JourneyChannel,
        Kind::StageScope,
        Kind::UseCaseCard,
        Kind::RuleSlot,
    ];

    /// The name stored in `layer_row.kind` (and its CHECK constraint).
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Entity => "entity",
            Kind::Attribute => "attribute",
            Kind::Relation => "relation",
            Kind::Job => "job",
            Kind::ContextSpace => "context_space",
            Kind::Procedure => "procedure",
            Kind::ConnectorSlot => "connector_slot",
            Kind::KnowledgeStore => "knowledge_store",
            Kind::Object => "object",
            Kind::Operation => "operation",
            Kind::ErrorState => "error_state",
            Kind::GateQuestion => "gate_question",
            Kind::AgentRole => "agent_role",
            Kind::Profile => "profile",
            Kind::AxisRule => "axis_rule",
            Kind::DepartmentArchetype => "department_archetype",
            Kind::JourneyTheme => "journey_theme",
            Kind::JourneyStage => "journey_stage",
            Kind::ThemeStagePrior => "theme_stage_prior",
            Kind::StageKnowledgeKind => "stage_knowledge_kind",
            Kind::Activity => "activity",
            Kind::ActivityObject => "activity_object",
            Kind::OperationExpectation => "operation_expectation",
            Kind::JobSetting => "job_setting",
            Kind::KnowledgePack => "knowledge_pack",
            Kind::Instruction => "instruction",
            Kind::JourneyChannel => "journey_channel",
            Kind::StageScope => "stage_scope",
            Kind::UseCaseCard => "use_case_card",
            Kind::RuleSlot => "rule_slot",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// The configurator's eight compile kinds (`patch` is partial).
    pub fn is_overlay(self) -> bool {
        matches!(
            self,
            Kind::Entity
                | Kind::Attribute
                | Kind::Relation
                | Kind::Job
                | Kind::ContextSpace
                | Kind::Procedure
                | Kind::ConnectorSlot
                | Kind::KnowledgeStore
        )
    }

    /// The four job-content kinds (context spaces, procedures, connector slots, knowledge stores).
    pub fn is_job_content(self) -> bool {
        matches!(
            self,
            Kind::ContextSpace | Kind::Procedure | Kind::ConnectorSlot | Kind::KnowledgeStore
        )
    }

    /// The journey kinds B8 owns (B6 stores `{label, summary}` only).
    pub fn is_journey(self) -> bool {
        matches!(
            self,
            Kind::JourneyTheme
                | Kind::JourneyStage
                | Kind::ThemeStagePrior
                | Kind::StageKnowledgeKind
                | Kind::Activity
                | Kind::ActivityObject
                | Kind::OperationExpectation
        )
    }

    /// The layer scopes a row of this kind may live in (`kind_not_allowed_in_layer` otherwise). Context spaces,
    /// knowledge stores, procedures and connector slots sit in department layers (base_dept and industry_dept,
    /// D116) as well as job layers (the fixtures put the retail areas there, D60/D61). Job settings and instructions live in every template layer and inherit down the fold
    /// (D108).
    pub fn scopes(self) -> &'static [Scope] {
        match self {
            Kind::Entity | Kind::Attribute | Kind::Relation => EVERY_SCOPE,
            Kind::Job | Kind::AgentRole => &[Scope::Base, Scope::Job],
            Kind::ContextSpace | Kind::KnowledgeStore | Kind::Procedure | Kind::ConnectorSlot => {
                &[Scope::BaseDept, Scope::IndustryDept, Scope::Job]
            }
            Kind::ErrorState | Kind::DepartmentArchetype => &[Scope::Base],
            Kind::JourneyChannel => &[Scope::Base, Scope::Industry],
            Kind::StageScope => &[Scope::Industry, Scope::BaseDept, Scope::IndustryDept],
            // a card has no base-level default: it belongs to a department's activity (or a job's refinement)
            Kind::UseCaseCard => &[
                Scope::Industry,
                Scope::BaseDept,
                Scope::IndustryDept,
                Scope::Job,
            ],
            Kind::Profile => &[Scope::Base, Scope::Industry],
            Kind::Object
            | Kind::Operation
            | Kind::GateQuestion
            | Kind::AxisRule
            | Kind::JourneyTheme
            | Kind::JourneyStage
            | Kind::ThemeStagePrior
            | Kind::StageKnowledgeKind
            | Kind::Activity
            | Kind::ActivityObject
            | Kind::OperationExpectation
            | Kind::JobSetting
            | Kind::KnowledgePack
            | Kind::Instruction
            | Kind::RuleSlot => TEMPLATE_SCOPES,
        }
    }

    pub fn allowed_in(self, scope: Scope) -> bool {
        self.scopes().contains(&scope)
    }

    /// D111: whether a row of this kind may live in a region sub-layer (`…/region/<r>`). Not the platform-wide
    /// definitions: `axis_rule` / `profile` (business-model rules, read from every tag member whatever its layer) and
    /// `department_archetype` (read from the base step only) — on a sub-layer they would apply to, or miss, every job
    /// regardless of its region. The schema refuses them too (0139 `layer_row_region_kind`).
    pub fn allowed_in_region(self) -> bool {
        !matches!(
            self,
            Kind::AxisRule | Kind::Profile | Kind::DepartmentArchetype
        )
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Add,
    Patch,
    Tombstone,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Add => "add",
            Op::Patch => "patch",
            Op::Tombstone => "tombstone",
        }
    }
}

/// One problem with a row's key or fields. `code` is one of `invalid_key`, `invalid_row`, `unknown_field`,
/// `missing_field`, `invalid_value`, `empty_patch`, `fields_on_tombstone`, or a kind's own code
/// (`invalid_rule`, `invalid_threshold`, `invalid_question`, `too_many_options`, `invalid_modalities`,
/// `invalid_label`, `empty_selection`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{code}: {message}")]
pub struct FieldError {
    pub code: &'static str,
    pub field: Option<String>,
    pub message: String,
}

impl FieldError {
    pub fn new(code: &'static str, field: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            code,
            field: field.map(str::to_owned),
            message: message.into(),
        }
    }

    /// An error on a computed field path (`fields[2].field`).
    pub fn at(code: &'static str, field: String, message: impl Into<String>) -> Self {
        Self {
            code,
            field: Some(field),
            message: message.into(),
        }
    }
}

/// A refused edit, as the UI's fixture client refuses it: HTTP status, error code and message. B6b routes map
/// it to `ApiError` one to one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, thiserror::Error)]
#[error("{status} {code}: {message}")]
pub struct Refusal {
    pub status: u16,
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    pub fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }
}

// --- key shapes (std only, no regex) -------------------------------------

/// `^[A-Z][A-Za-z0-9]{0,63}$`
pub(crate) fn is_entity_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('A'..='Z'))
        && s.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric())
}

/// `^[a-z][a-z0-9_]{0,63}$`
pub(crate) fn is_snake_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && s.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `object.verb` or `object.verb{variant}` (`order.cancel`, `case.create{return}`, `rule.evaluate{RET-01}`):
/// object and verb snake_case, variant `[A-Za-z0-9_-]{1,64}`.
pub fn split_operation_key(key: &str) -> Option<(&str, &str, Option<&str>)> {
    let (op, variant) = match key.strip_suffix('}') {
        Some(head) => {
            let (op, variant) = head.split_once('{')?;
            (op, Some(variant))
        }
        None => (key, None),
    };
    let (object, verb) = op.split_once('.')?;
    let variant_ok = variant.is_none_or(|v| {
        !v.is_empty()
            && v.len() <= 64
            && v.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    });
    (is_snake_name(object) && is_snake_name(verb) && variant_ok).then_some((object, verb, variant))
}

/// `^[A-Z][A-Z0-9]{1,7}$` (`GT`, `B1`, `DR1`), as the UI's gate key rule.
fn is_gate_key(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('A'..='Z'))
        && (2..=8).contains(&s.len())
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// The journey kinds' keys until B8 fixes their shapes (`support:start_return`, `issue|support`,
/// `support|tariff.sheet`, `delivery.redirect`): a lower-case letter, then at most 127 of `[a-z0-9_.:|{}-]`.
fn is_item_key(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('a'..='z'))
        && s.len() <= 128
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_.:|{}-".contains(c))
}

/// `stage|kind` of a `stage_knowledge_kind` row (`support|tariff.sheet`): an item key with exactly one `|` and no
/// `:`, the stage at most 64 characters; the stage may be `*` (every stage, D107). Knowledge kinds are materialised
/// into agent-api's `job_knowledge_kind`, whose CHECKs (0054) refuse `:`, `|` and whitespace in either part and a
/// stage over 64 characters; a key this accepts always passes them.
pub fn split_stage_knowledge_key(key: &str) -> Option<(&str, &str)> {
    // `*|kind` is checked as `s|kind`: the wildcard stage is the only `*` an item key may carry
    let probe = match key.strip_prefix("*|") {
        Some(rest) => std::borrow::Cow::Owned(format!("s|{rest}")),
        None => std::borrow::Cow::Borrowed(key),
    };
    if !is_item_key(&probe) || key.contains(':') {
        return None;
    }
    let (stage, kind) = key.split_once('|')?;
    (!stage.is_empty() && stage.len() <= 64 && !kind.is_empty() && !kind.contains('|'))
        .then_some((stage, kind))
}

fn key_matches(kind: Kind, key: &str) -> bool {
    match kind {
        Kind::Entity
        | Kind::Attribute
        | Kind::Relation
        | Kind::Job
        | Kind::ContextSpace
        | Kind::Procedure
        | Kind::ConnectorSlot
        | Kind::KnowledgeStore => overlay::key_matches(kind, key),
        Kind::Operation => split_operation_key(key).is_some(),
        Kind::GateQuestion => is_gate_key(key),
        Kind::Object
        | Kind::ErrorState
        | Kind::AgentRole
        | Kind::Profile
        | Kind::AxisRule
        | Kind::DepartmentArchetype
        | Kind::JourneyTheme
        | Kind::JourneyStage
        | Kind::JourneyChannel
        | Kind::StageScope
        | Kind::JobSetting => is_snake_name(key),
        Kind::StageKnowledgeKind => split_stage_knowledge_key(key).is_some(),
        Kind::KnowledgePack => knowledge_pack::split_key(key).is_some(),
        Kind::Instruction => instruction::is_slot(key),
        Kind::RuleSlot => rule_slot::is_rule_id(key),
        Kind::ThemeStagePrior
        | Kind::Activity
        | Kind::ActivityObject
        | Kind::OperationExpectation
        | Kind::UseCaseCard => is_item_key(key),
    }
}

fn key_shape(kind: Kind) -> &'static str {
    match kind {
        Kind::Entity => "Entity (e.g. Order)",
        Kind::Attribute => "Entity.attribute_name (e.g. Order.order_id)",
        Kind::Relation => "Entity|predicate|Entity (e.g. Order|paidBy|Payment)",
        Kind::ContextSpace => "industry:area:slug (e.g. retail:order-support:returns)",
        Kind::Procedure => "name@version (e.g. return_request@2 or return@1.3.0)",
        Kind::ConnectorSlot => "kebab-case id (e.g. crm-core)",
        Kind::KnowledgeStore => "kebab-case key (e.g. faq-policies)",
        Kind::Operation => "object.verb or object.verb{variant} (e.g. case.create{return})",
        Kind::GateQuestion => "2-8 capital letters or digits, starting with a letter (e.g. GT)",
        Kind::StageKnowledgeKind => {
            "stage|kind, no colon, stage at most 64 characters (e.g. support|tariff.sheet)"
        }
        Kind::ThemeStagePrior
        | Kind::Activity
        | Kind::ActivityObject
        | Kind::OperationExpectation
        | Kind::UseCaseCard => "lower-case item key (e.g. support:start_return)",
        Kind::Instruction => "a prompt slot: identity, policy, tone, safety, escalation or format",
        Kind::RuleSlot => "rule id: upper-case letters, digits and - (e.g. RET-01)",
        Kind::KnowledgePack => {
            "stage|kind|pack_key, stage and kind `*` or without colon, bar or whitespace \
             (e.g. support|*|industry/retail)"
        }
        Kind::Job
        | Kind::Object
        | Kind::ErrorState
        | Kind::AgentRole
        | Kind::Profile
        | Kind::AxisRule
        | Kind::DepartmentArchetype
        | Kind::JourneyTheme
        | Kind::JourneyStage
        | Kind::JourneyChannel
        | Kind::StageScope
        | Kind::JobSetting => "snake_case name (e.g. customer_care_agent)",
    }
}

/// Check that `key` has the shape required for `kind`.
pub fn validate_key(kind: Kind, key: &str) -> Result<(), FieldError> {
    if key_matches(kind, key) {
        return Ok(());
    }
    Err(FieldError::new(
        "invalid_key",
        Some("key"),
        format!("invalid {kind} key {key:?}, expected {}", key_shape(kind)),
    ))
}

// --- fields --------------------------------------------------------------

/// Validate the fields of a complete item of `kind` (an `add` row, or a seeded row).
pub fn validate_fields(kind: Kind, fields: &Value) -> Result<(), Vec<FieldError>> {
    validate_row(kind, Op::Add, fields)
}

/// Validate the fields of a row of `kind` written with `op`.
pub fn validate_row(kind: Kind, op: Op, fields: &Value) -> Result<(), Vec<FieldError>> {
    let Some(map) = fields.as_object() else {
        return Err(vec![FieldError::new(
            "invalid_row",
            None,
            "fields must be a JSON object",
        )]);
    };
    match op {
        Op::Tombstone => check_tombstone(map),
        Op::Add | Op::Patch if kind.is_overlay() => overlay::validate(kind, op, map),
        Op::Add | Op::Patch => {
            match kind {
                Kind::Object => {
                    parse::<catalog::ObjectFields>(fields).and_then(|f| catalog::check_object(&f))
                }
                Kind::Operation => parse::<catalog::OperationFields>(fields)
                    .and_then(|f| catalog::check_operation(&f)),
                Kind::ErrorState => parse::<catalog::ErrorStateFields>(fields)
                    .and_then(|f| catalog::check_error_state(&f)),
                Kind::DepartmentArchetype => parse::<catalog::DepartmentArchetypeFields>(fields)
                    .and_then(|f| catalog::check_department_archetype(&f)),
                Kind::GateQuestion => {
                    parse::<gate::GateQuestionFields>(fields).and_then(|f| gate::check(&f))
                }
                Kind::AgentRole => {
                    parse::<role::AgentRoleFields>(fields).and_then(|f| role::check(&f))
                }
                Kind::Profile => {
                    parse::<profile::ProfileFields>(fields).and_then(|f| profile::check_profile(&f))
                }
                Kind::AxisRule => parse::<profile::AxisRuleFields>(fields)
                    .and_then(|f| profile::check_axis_rule(&f)),
                Kind::JobSetting => parse::<journey::JobSettingFields>(fields)
                    .and_then(|f| journey::check_job_setting(&f)),
                Kind::KnowledgePack => parse::<knowledge_pack::KnowledgePackFields>(fields)
                    .and_then(|f| knowledge_pack::check(&f)),
                Kind::StageKnowledgeKind => {
                    parse::<stage_knowledge::StageKnowledgeKindFields>(fields)
                        .and_then(|f| stage_knowledge::check(&f))
                }
                Kind::Instruction => parse::<instruction::InstructionFields>(fields)
                    .and_then(|f| instruction::check(&f)),
                Kind::JourneyTheme => parse::<journey::JourneyThemeFields>(fields)
                    .and_then(|f| journey::check_theme(&f)),
                Kind::JourneyStage => parse::<journey::JourneyStageFields>(fields)
                    .and_then(|f| journey::check_stage(&f)),
                Kind::ThemeStagePrior => parse::<journey::ThemeStagePriorFields>(fields)
                    .and_then(|f| journey::check_prior(&f)),
                Kind::Activity => parse::<journey::ActivityFields>(fields)
                    .and_then(|f| journey::check_activity(&f)),
                Kind::JourneyChannel => parse::<journey::JourneyChannelFields>(fields)
                    .and_then(|f| journey::check_channel(&f)),
                Kind::StageScope => parse::<journey::StageScopeFields>(fields)
                    .and_then(|f| journey::check_stage_scope(&f)),
                Kind::RuleSlot => {
                    parse::<rule_slot::RuleSlotFields>(fields).and_then(|f| rule_slot::check(&f))
                }
                Kind::UseCaseCard => parse::<journey::UseCaseCardFields>(fields)
                    .and_then(|f| journey::check_card(&f)),
                Kind::ActivityObject | Kind::OperationExpectation => {
                    parse::<journey::JourneyFields>(fields).and_then(|f| journey::check_journey(&f))
                }
                Kind::Entity
                | Kind::Attribute
                | Kind::Relation
                | Kind::Job
                | Kind::ContextSpace
                | Kind::Procedure
                | Kind::ConnectorSlot
                | Kind::KnowledgeStore => overlay::validate(kind, op, map),
            }
        }
    }
}

/// A tombstone carries nothing but an optional `reason` (the fixtures record why an item was retired).
fn check_tombstone(fields: &Map<String, Value>) -> Result<(), Vec<FieldError>> {
    let errors: Vec<FieldError> = fields
        .iter()
        .filter_map(|(name, value)| match (name.as_str(), value.as_str()) {
            ("reason", Some(s)) if !s.trim().is_empty() => None,
            ("reason", _) => Some(FieldError::new(
                "invalid_value",
                Some("reason"),
                "must be a non-empty string",
            )),
            _ => Some(FieldError::new(
                "fields_on_tombstone",
                Some(name.as_str()),
                "a tombstone carries no fields but an optional reason",
            )),
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Parse `fields` as the kind's struct; a serde error becomes one `FieldError`.
fn parse<T: DeserializeOwned>(fields: &Value) -> Result<T, Vec<FieldError>> {
    T::deserialize(fields).map_err(|e| {
        let message = e.to_string();
        let code = if message.starts_with("unknown field") {
            "unknown_field"
        } else if message.starts_with("missing field") {
            "missing_field"
        } else {
            "invalid_value"
        };
        vec![FieldError::new(code, None, message)]
    })
}

/// `Ok(())` when `errors` is empty.
pub(crate) fn done(errors: Vec<FieldError>) -> Result<(), Vec<FieldError>> {
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// A non-empty (after trim) string field, or an `invalid_value` error on `field`.
pub(crate) fn require_text(value: &str, field: &str, errors: &mut Vec<FieldError>) {
    if value.trim().is_empty() {
        errors.push(FieldError::new(
            "invalid_value",
            Some(field),
            "must be a non-empty string",
        ));
    }
}

// --- summary + label ------------------------------------------------------

fn text<'a>(fields: &'a Value, name: &str) -> &'a str {
    fields.get(name).and_then(Value::as_str).unwrap_or_default()
}

fn texts<'a>(fields: &'a Value, name: &str) -> Vec<&'a str> {
    fields
        .get(name)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

fn count(fields: &Value, name: &str) -> usize {
    fields
        .get(name)
        .and_then(Value::as_array)
        .map_or(0, Vec::len)
}

/// The item's authored content in one line (a TagItem `detail`). API writes store it as `summary`; seeded rows
/// keep the fixture text. Journey kinds and job settings have no structure yet (B8, B2): their stored
/// `summary` is the content.
pub fn summary(kind: Kind, fields: &Value) -> String {
    let label = text(fields, "label").to_owned();
    let parts: Vec<String> = match kind {
        Kind::Entity => vec![label, texts(fields, "synonyms").join(", ")],
        Kind::Attribute => {
            let pii = fields.get("pii").and_then(Value::as_bool) == Some(true);
            vec![
                text(fields, "data_type").to_owned(),
                if pii { "pii".to_owned() } else { String::new() },
            ]
        }
        Kind::Relation => vec![text(fields, "cardinality").to_owned()],
        Kind::Job => vec![label, text(fields, "actor").to_owned()],
        Kind::ContextSpace => vec![label, texts(fields, "slots").join(", ")],
        Kind::Procedure => vec![label, texts(fields, "uses_slots").join(", ")],
        Kind::ConnectorSlot => vec![label, texts(fields, "implements").join(", ")],
        Kind::KnowledgeStore => vec![label, texts(fields, "kinds").join(", ")],
        Kind::Object => {
            let n_fields = count(fields, "fields");
            let n_states = count(fields, "states");
            vec![
                label,
                format!("{n_fields} fields"),
                format!("{n_states} states"),
            ]
        }
        Kind::Operation | Kind::ErrorState => vec![text(fields, "description").to_owned()],
        Kind::GateQuestion => vec![
            format!("{} {}", text(fields, "point"), text(fields, "type")),
            text(fields, "question").to_owned(),
        ],
        Kind::AgentRole => {
            let enabled = fields.get("enabled").and_then(Value::as_bool) != Some(false);
            let n_grants = count(fields, "grants");
            vec![
                label,
                format!("max {}", text(fields, "max_side_effect")),
                format!("{n_grants} grants"),
                if enabled {
                    String::new()
                } else {
                    "disabled".to_owned()
                },
            ]
        }
        Kind::Profile | Kind::DepartmentArchetype => {
            vec![label, text(fields, "description").to_owned()]
        }
        Kind::AxisRule => {
            let condition = fields
                .get("condition")
                .and_then(|c| Condition::deserialize(c).ok())
                .map(|c| c.describe())
                .unwrap_or_default();
            let priority = fields
                .get("priority")
                .and_then(Value::as_i64)
                .map(|p| format!("priority {p}"))
                .unwrap_or_default();
            vec![priority, condition]
        }
        Kind::JobSetting => match fields.get("summary").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => vec![s.to_owned()],
            _ => {
                let value = match fields.get("value") {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(s)) => s.clone(),
                    Some(other) => canonical_json(other),
                };
                vec![label, value]
            }
        },
        Kind::StageKnowledgeKind => {
            match stage_knowledge::StageKnowledgeKindFields::deserialize(fields) {
                Ok(f) => vec![stage_knowledge::summary(&f)],
                Err(_) => vec![label],
            }
        }
        Kind::JourneyTheme
        | Kind::JourneyStage
        | Kind::ThemeStagePrior
        | Kind::Activity
        | Kind::ActivityObject
        | Kind::OperationExpectation
        | Kind::JourneyChannel
        | Kind::UseCaseCard => match fields.get("summary").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => vec![s.to_owned()],
            _ => vec![label],
        },
        Kind::StageScope => match fields.get("out_of_scope").and_then(Value::as_bool) {
            Some(true) => vec!["out of scope".to_owned()],
            _ => vec!["in scope".to_owned()],
        },
        // `{version}` only: no summary is stored (write_revision drops an empty one); the label says it all.
        Kind::KnowledgePack => Vec::new(),
        Kind::RuleSlot => match rule_slot::RuleSlotFields::deserialize(fields) {
            Ok(f) => vec![rule_slot::summary(&f)],
            Err(_) => vec![label],
        },
        Kind::Instruction => match instruction::InstructionFields::deserialize(fields) {
            Ok(f) => vec![instruction::summary(&f)],
            Err(_) => vec![text(fields, "text").to_owned()],
        },
    };
    parts
        .into_iter()
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The item's display label: its `label` field, else its key (operations, error states, gates have none). A
/// `knowledge_pack` row has no label field: `<pack_key>@v<version> → <stage>/<kind>`.
pub fn label(kind: Kind, key: &str, fields: &Value) -> String {
    if kind == Kind::KnowledgePack {
        return knowledge_pack::label(key, fields.get("version").and_then(Value::as_i64));
    }
    match text(fields, "label").trim() {
        "" => key.to_owned(),
        l => l.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn codes(r: Result<(), Vec<FieldError>>) -> Vec<&'static str> {
        r.err().unwrap_or_default().iter().map(|e| e.code).collect()
    }

    #[test]
    fn kinds_round_trip() {
        let mut names: Vec<&str> = Kind::ALL.iter().map(|k| k.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 30);
        for k in Kind::ALL {
            assert_eq!(Kind::parse(k.as_str()), Some(k));
            assert_eq!(serde_json::to_value(k).expect("kind"), json!(k.as_str()));
            assert_eq!(k.to_string(), k.as_str());
            assert!(!k.scopes().is_empty(), "{k}");
        }
        assert_eq!(Kind::ALL.iter().filter(|k| k.is_overlay()).count(), 8);
        assert_eq!(Kind::ALL.iter().filter(|k| k.is_journey()).count(), 7);
        assert_eq!(Kind::parse("tenant"), None);
    }

    #[test]
    fn profile_layers_hold_only_ontology_rows() {
        let in_profile: Vec<Kind> = Kind::ALL
            .into_iter()
            .filter(|k| k.allowed_in(Scope::Profile))
            .collect();
        assert_eq!(in_profile, [Kind::Entity, Kind::Attribute, Kind::Relation]);
        assert!(Kind::ContextSpace.allowed_in(Scope::IndustryDept));
        // D116: department-level content lives in base_dept and industry_dept as well as job
        for kind in [
            Kind::Procedure,
            Kind::ConnectorSlot,
            Kind::ContextSpace,
            Kind::KnowledgeStore,
        ] {
            for scope in [Scope::BaseDept, Scope::IndustryDept, Scope::Job] {
                assert!(kind.allowed_in(scope), "{kind} {scope}");
            }
            for scope in [Scope::Base, Scope::Profile, Scope::Industry] {
                assert!(!kind.allowed_in(scope), "{kind} {scope}");
            }
        }
        assert!(!Kind::Job.allowed_in(Scope::BaseDept));
        assert!(!Kind::AgentRole.allowed_in(Scope::BaseDept));
        assert!(!Kind::DepartmentArchetype.allowed_in(Scope::BaseDept));
        assert!(!Kind::Profile.allowed_in(Scope::BaseDept));
        assert!(Kind::GateQuestion.allowed_in(Scope::BaseDept));
        assert!(Kind::ErrorState.allowed_in(Scope::Base));
        assert!(!Kind::ErrorState.allowed_in(Scope::Job));
        assert!(Kind::AgentRole.allowed_in(Scope::Job));
        assert!(!Kind::Profile.allowed_in(Scope::Job));
        // D108: settings and instructions live in every template layer, never a profile layer
        for kind in [Kind::JobSetting, Kind::Instruction] {
            for scope in [
                Scope::Base,
                Scope::Industry,
                Scope::BaseDept,
                Scope::IndustryDept,
                Scope::Job,
            ] {
                assert!(kind.allowed_in(scope), "{kind} {scope}");
            }
            assert!(!kind.allowed_in(Scope::Profile), "{kind}");
        }
    }

    #[test]
    fn catalogue_key_shapes() {
        let ok = [
            (Kind::Object, "access_grant"),
            (Kind::Operation, "order.cancel"),
            (Kind::Operation, "case.create{return}"),
            (Kind::Operation, "rule.evaluate{RET-01}"),
            (Kind::ErrorState, "connector_misconfigured"),
            (Kind::GateQuestion, "GT"),
            (Kind::GateQuestion, "DR1"),
            (Kind::AgentRole, "staff_console"),
            (Kind::Profile, "retail_ecommerce"),
            (Kind::AxisRule, "metered_supply"),
            (Kind::DepartmentArchetype, "customer_service"),
            (Kind::JourneyStage, "orientation"),
            (Kind::ThemeStagePrior, "issue|support"),
            (Kind::StageKnowledgeKind, "support|tariff.sheet"),
            (Kind::Activity, "support:start_return"),
            (Kind::OperationExpectation, "delivery.redirect"),
            (Kind::JobSetting, "north_star"),
        ];
        for (kind, key) in ok {
            assert_eq!(validate_key(kind, key), Ok(()), "{kind} {key}");
        }
        let bad = [
            (Kind::Object, "Order"),
            (Kind::Operation, "order"),
            (Kind::Operation, "order.cancel{}"),
            (Kind::Operation, "Order.cancel"),
            (Kind::Operation, "order.cancel{a b}"),
            (Kind::GateQuestion, "G"),
            (Kind::GateQuestion, "GS.exit"),
            (Kind::GateQuestion, "gt"),
            (Kind::GateQuestion, "ABCDEFGHI"),
            (Kind::Activity, "Support:x"),
            (Kind::JobSetting, ""),
            // the agent-api job_knowledge_kind CHECKs (0054): one `|`, no `:`, stage ≤ 64
            (Kind::StageKnowledgeKind, "support:returns|faq"),
            (Kind::StageKnowledgeKind, "a|b|c"),
            (Kind::StageKnowledgeKind, "support"),
            (Kind::StageKnowledgeKind, "support|"),
            (Kind::StageKnowledgeKind, "support|faq:x"),
        ];
        for (kind, key) in bad {
            assert_eq!(
                validate_key(kind, key).err().map(|e| e.code),
                Some("invalid_key"),
                "{kind} {key:?}"
            );
        }
        let long_stage = format!("{}|faq", "s".repeat(65));
        assert_eq!(
            validate_key(Kind::StageKnowledgeKind, &long_stage)
                .err()
                .map(|e| e.code),
            Some("invalid_key")
        );
        assert_eq!(
            split_stage_knowledge_key(&format!("{}|faq", "s".repeat(64))).map(|(_, k)| k),
            Some("faq")
        );
        assert_eq!(
            split_operation_key("case.create{return}"),
            Some(("case", "create", Some("return")))
        );
        assert_eq!(
            split_operation_key("order.read"),
            Some(("order", "read", None))
        );
    }

    #[test]
    fn tombstone_takes_only_a_reason() {
        assert!(validate_row(Kind::Entity, Op::Tombstone, &json!({})).is_ok());
        assert!(
            validate_row(
                Kind::ContextSpace,
                Op::Tombstone,
                &json!({"reason": "Gift cards removed from retail@v2"})
            )
            .is_ok()
        );
        assert_eq!(
            codes(validate_row(
                Kind::Entity,
                Op::Tombstone,
                &json!({"label": "x"})
            )),
            vec!["fields_on_tombstone"]
        );
        assert_eq!(
            codes(validate_row(
                Kind::Object,
                Op::Tombstone,
                &json!({"reason": ""})
            )),
            vec!["invalid_value"]
        );
        assert_eq!(
            codes(validate_fields(
                Kind::Entity,
                &json!(["not", "an", "object"])
            )),
            vec!["invalid_row"]
        );
    }

    #[test]
    fn catalogue_rows_are_whole_items() {
        // deny_unknown_fields, missing required fields, bad enum values.
        let extra = json!({"user_facing": true, "retryable": false, "escalate": false, "description": "x", "colour": 1});
        assert_eq!(
            codes(validate_fields(Kind::ErrorState, &extra)),
            vec!["unknown_field"]
        );
        assert_eq!(
            codes(validate_fields(
                Kind::ErrorState,
                &json!({"description": "x"})
            )),
            vec!["missing_field"]
        );
        let bad_enum = json!({"label": "Q", "used_by": "edge", "max_side_effect": "Z", "enabled": true, "grants": [], "note": ""});
        assert_eq!(
            codes(validate_fields(Kind::AgentRole, &bad_enum)),
            vec!["invalid_value"]
        );
        // A catalogue patch is the whole item too.
        assert_eq!(
            codes(validate_row(
                Kind::ErrorState,
                Op::Patch,
                &json!({"description": "x"})
            )),
            vec!["missing_field"]
        );
    }

    #[test]
    fn summaries_and_labels() {
        let entity =
            json!({"label": "Customer", "synonyms": ["customer", "klant"], "object": "party"});
        assert_eq!(summary(Kind::Entity, &entity), "Customer · customer, klant");
        assert_eq!(label(Kind::Entity, "Party", &entity), "Customer");
        assert_eq!(
            summary(
                Kind::Attribute,
                &json!({"data_type": "string", "pii": true})
            ),
            "string · pii"
        );
        assert_eq!(
            label(Kind::Operation, "order.cancel", &json!({})),
            "order.cancel"
        );
        let rule = json!({"priority": 100, "condition": {"and": [{"eq": ["order_type", "continuous"]}, {"eq": ["payment_type", "balancing"]}]}, "layer": "profile/metered_supply"});
        assert_eq!(
            summary(Kind::AxisRule, &rule),
            "priority 100 · order_type = continuous ∧ payment_type = balancing"
        );
        assert_eq!(
            summary(
                Kind::JourneyTheme,
                &json!({"label": "Issue", "summary": "escalation_weight 0.6"})
            ),
            "escalation_weight 0.6"
        );
        assert_eq!(
            summary(
                Kind::JobSetting,
                &json!({"label": "Stance", "value": "warm"})
            ),
            "Stance · warm"
        );
        let role =
            json!({"label": "Assistant", "max_side_effect": "W", "enabled": false, "grants": []});
        assert_eq!(
            summary(Kind::AgentRole, &role),
            "Assistant · max W · 0 grants · disabled"
        );
    }
}
