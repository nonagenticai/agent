//! Journey kinds (`journey_theme`, `journey_stage`, `theme_stage_prior`, `journey_channel`, `stage_scope`,
//! `activity`, `activity_object`, `operation_expectation`, `use_case_card`; `11` §6.2; `stage_knowledge_kind` is
//! KH's, `stage_knowledge.rs`) and `job_setting`. B6 stored the first rows as summary rows (`{label, summary}`); B8
//! phase 2 (T1) gives `journey_theme`, `journey_stage`, `theme_stage_prior` and `activity` their own structs whose
//! added fields are all optional (the seeded `{label, summary}` rows still validate) and adds `journey_channel`,
//! `stage_scope` and `use_case_card`. The field names and shapes are the wire structs of agent-api's
//! `journey::model` (snake_case), so the grid maps a row to its struct 1:1. `activity_object` and
//! `operation_expectation` keep [`JourneyFields`].
//!
//! Job settings inherit (D108, plan KH T4): they live in every template layer, and a later layer's `patch` that
//! names no `value` keeps the nearest value above it (job → department → industry → base, the fold order). A setting
//! with a `floor` is a rule floor: its numeric value may only tighten down the fold — `min` never decreases, `max`
//! never increases, the direction never changes and the floor is never tombstoned ([`fold_setting`]; the compile
//! reports a violation as `floor_loosened`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{FieldError, done, require_text};

/// Fields of a journey-kind row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyFields {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Channel keys of a stage's example messages and of a `journey_channel` row (agent-api `ChannelKey`).
pub const CHANNEL_KEYS: [&str; 6] = ["chat", "email", "contact_form", "phone", "visit", "signing"];

fn is_unit_interval(x: f64) -> bool {
    (0.0..=1.0).contains(&x)
}

/// Fields of a `journey_theme` row: the customer state, compiled into gate-theme options.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyThemeFields {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
    /// 0–1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_weight: Option<f64>,
}

/// Fields of a `journey_stage` row: the capability column. `out_of_scope` is NOT here: it is the `stage_scope`
/// row of a department layer. `messages` maps a channel key to example customer utterances.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyStageFields {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub synonyms: Option<Vec<String>>,
    /// Theme keys the stage is reachable from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub themes: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages: Option<BTreeMap<String, Vec<String>>>,
}

/// Fields of a `theme_stage_prior` row (key `theme|stage`): the weight 0–1 (a weight of 0 is a tombstone).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeStagePriorFields {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weight: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityActor {
    Customer,
    Company,
    /// A known employee asking their own employer's desk (HR, IT service desk); the agent never asks them for an
    /// identifier (owner ruling "I KNOW MY CUSTOMER", which covers employees).
    Employee,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectRole {
    Subject,
    Context,
}

/// An object an activity reads or writes (agent-api `ActivityObject`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityObjectRef {
    /// Canonical object key, optionally with a variant (`case{return}`).
    pub object: String,
    pub role: ObjectRole,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub required_inputs: Vec<String>,
}

/// An operation an activity expects (agent-api `OperationExpectation`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationRef {
    /// `object.verb`.
    pub op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// Fields of an `activity` row (key `stage:activity`); objects and operations are embedded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityFields {
    pub label: String,
    /// The technical form (actor, operations); the object pages scan it for the operations an activity uses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// One plain line a customer or employee would recognise ("Change something about an order that has not shipped
    /// yet"); the description of the activity's generated option in the activity gate GA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub objects: Option<Vec<ActivityObjectRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operations: Option<Vec<OperationRef>>,
    /// D61: the department area.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<ActivityActor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub also_in_stages: Option<Vec<String>>,
    /// Owner 2026-10-26: the template marks the activity required - a tenant cannot hide it (`required_activity`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelStatusField {
    V1,
    Later,
    OutOfScope,
}

/// Fields of a `journey_channel` row (key = the channel key): an industry row is that industry's status (F-49).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JourneyChannelFields {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ChannelStatusField>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Fields of a `stage_scope` row (key = the stage): whether the stage is out of scope for the layer's department.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageScopeFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub out_of_scope: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScenarioStyle {
    Plain,
    Indirect,
    Nl,
}

/// One example customer phrasing of an activity with what a correct reply does (B8-4). `place` is the design place
/// (B2 ledger key) the outcome depends on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CardScenario {
    pub id: String,
    pub style: ScenarioStyle,
    pub phrasing: String,
    pub expected: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub place: Option<String>,
    #[serde(default)]
    pub cites: Vec<String>,
}

/// Fields of a `use_case_card` row (key `stage:activity`). A tenant refines the card's text fields, never
/// `scenarios`. `metric`, `sample_dialogue`, `success_example` and `error_example` are the shapes the web client
/// types (`ActivityDetail`), carried as JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UseCaseCardFields {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub examples: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metric: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_dialogue: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub success_example: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_example: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scenarios: Option<Vec<CardScenario>>,
}

/// Fields of a `job_setting` row (`stance`, `north_star`, `return_window_days` …); `value` is any JSON (B2 reads
/// it as the setting's default). `floor` makes the setting a rule floor (D108): `value` must then be a number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSettingFields {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<Floor>,
}

/// Which way a rule floor may move down the fold: `min` (e.g. a statutory return window: a lower layer may raise
/// it, never lower it) or `max` (a lower layer may lower it, never raise it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Floor {
    Min,
    Max,
}

impl Floor {
    fn as_str(self) -> &'static str {
        match self {
            Floor::Min => "min",
            Floor::Max => "max",
        }
    }
}

pub(crate) fn check_journey(f: &JourneyFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&f.label, "label", &mut errors);
    done(errors)
}

pub(crate) fn check_theme(f: &JourneyThemeFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&f.label, "label", &mut errors);
    if f.escalation_weight.is_some_and(|w| !is_unit_interval(w)) {
        errors.push(FieldError::new(
            "invalid_value",
            Some("escalation_weight"),
            "must be between 0 and 1",
        ));
    }
    done(errors)
}

pub(crate) fn check_stage(f: &JourneyStageFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&f.label, "label", &mut errors);
    for (channel, utterances) in f.messages.iter().flatten() {
        if !CHANNEL_KEYS.contains(&channel.as_str()) {
            errors.push(FieldError::at(
                "invalid_value",
                format!("messages.{channel}"),
                format!("not a channel; one of {}", CHANNEL_KEYS.join(", ")),
            ));
        }
        if utterances.iter().any(|u| u.trim().is_empty()) {
            errors.push(FieldError::at(
                "invalid_value",
                format!("messages.{channel}"),
                "utterances must be non-empty strings",
            ));
        }
    }
    done(errors)
}

pub(crate) fn check_prior(f: &ThemeStagePriorFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&f.label, "label", &mut errors);
    if f.weight.is_some_and(|w| !is_unit_interval(w)) {
        errors.push(FieldError::new(
            "invalid_value",
            Some("weight"),
            "must be between 0 and 1",
        ));
    }
    done(errors)
}

pub(crate) fn check_activity(f: &ActivityFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&f.label, "label", &mut errors);
    for (i, o) in f.objects.iter().flatten().enumerate() {
        if o.object.trim().is_empty() {
            errors.push(FieldError::at(
                "invalid_value",
                format!("objects[{i}].object"),
                "must be a non-empty string",
            ));
        }
    }
    for (i, o) in f.operations.iter().flatten().enumerate() {
        if o.op.trim().is_empty() {
            errors.push(FieldError::at(
                "invalid_value",
                format!("operations[{i}].op"),
                "must be a non-empty string",
            ));
        }
    }
    done(errors)
}

pub(crate) fn check_channel(f: &JourneyChannelFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&f.label, "label", &mut errors);
    done(errors)
}

pub(crate) fn check_stage_scope(f: &StageScopeFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    if f.label.as_deref().is_some_and(|l| l.trim().is_empty()) {
        errors.push(FieldError::new(
            "invalid_value",
            Some("label"),
            "must be a non-empty string",
        ));
    }
    done(errors)
}

pub(crate) fn check_card(f: &UseCaseCardFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    if f.label.as_deref().is_some_and(|l| l.trim().is_empty()) {
        errors.push(FieldError::new(
            "invalid_value",
            Some("label"),
            "must be a non-empty string",
        ));
    }
    if f.metric.as_ref().is_some_and(|m| !m.is_object()) {
        errors.push(FieldError::new(
            "invalid_value",
            Some("metric"),
            "must be an object",
        ));
    }
    for (name, v) in [
        ("success_example", &f.success_example),
        ("error_example", &f.error_example),
    ] {
        if v.as_ref().is_some_and(|v| !v.is_object()) {
            errors.push(FieldError::new(
                "invalid_value",
                Some(name),
                "must be an object",
            ));
        }
    }
    let mut seen = Vec::new();
    for (i, s) in f.scenarios.iter().flatten().enumerate() {
        require_text(&s.id, &format!("scenarios[{i}].id"), &mut errors);
        require_text(
            &s.phrasing,
            &format!("scenarios[{i}].phrasing"),
            &mut errors,
        );
        require_text(
            &s.expected,
            &format!("scenarios[{i}].expected"),
            &mut errors,
        );
        if seen.contains(&s.id.as_str()) {
            errors.push(FieldError::at(
                "invalid_value",
                format!("scenarios[{i}].id"),
                "scenario ids must be unique within a card",
            ));
        }
        seen.push(s.id.as_str());
    }
    done(errors)
}

pub(crate) fn check_job_setting(f: &JobSettingFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&f.label, "label", &mut errors);
    if f.floor.is_some() && f.value.as_ref().is_some_and(|v| !v.is_number()) {
        errors.push(FieldError::new(
            "invalid_value",
            Some("value"),
            "a rule floor's value must be a number",
        ));
    }
    done(errors)
}

fn floor_of(fields: &Map<String, Value>) -> Option<Floor> {
    fields.get("floor").and_then(|f| Floor::deserialize(f).ok())
}

fn number(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// The fields a `job_setting` patch leaves in force over the inherited item `prev` (D108): `next`, plus `prev`'s
/// `value` and `floor` when `next` names none. `Err(problem)` when the patch loosens an inherited floor (a lower
/// value under `min`, a higher one under `max`, or another direction); the caller reports it as `floor_loosened`.
pub fn fold_setting(
    prev: &Map<String, Value>,
    next: &Map<String, Value>,
) -> Result<Map<String, Value>, String> {
    let mut out = next.clone();
    for inherited in ["value", "floor"] {
        if out.get(inherited).is_none_or(Value::is_null)
            && let Some(v) = prev.get(inherited)
        {
            out.insert(inherited.to_owned(), v.clone());
        }
    }
    let Some(floor) = floor_of(prev) else {
        return Ok(out);
    };
    match floor_of(&out) {
        Some(f) if f == floor => {}
        other => {
            return Err(format!(
                "the floor is {} and stays {}; this layer makes it {}",
                floor.as_str(),
                floor.as_str(),
                other.map_or("none", Floor::as_str)
            ));
        }
    }
    if let (Some(was), Some(now)) = (number(prev.get("value")), number(out.get("value"))) {
        let looser = match floor {
            Floor::Min => now < was,
            Floor::Max => now > was,
        };
        if looser {
            return Err(format!(
                "{now} is looser than the inherited {} floor {was}",
                floor.as_str()
            ));
        }
    }
    Ok(out)
}

/// `Err(problem)` when tombstoning the inherited item `prev` would drop a rule floor (D108).
pub fn tombstone_setting(prev: &Map<String, Value>) -> Result<(), String> {
    match floor_of(prev) {
        Some(f) => Err(format!(
            "it is a {} rule floor; a lower layer may tighten it, never remove it",
            f.as_str()
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Kind, validate_fields};
    use super::*;
    use serde_json::json;

    #[test]
    fn summary_rows() {
        for kind in Kind::ALL.into_iter().filter(|k| k.is_journey()) {
            let row = json!({"label": "Issue", "summary": "escalation_weight 0.6"});
            assert_eq!(validate_fields(kind, &row), Ok(()), "{kind}");
            assert!(validate_fields(kind, &json!({"label": "Issue", "bogus_field": 0.6})).is_err());
            assert!(validate_fields(kind, &json!({"label": ""})).is_err());
        }
        let setting = json!({"label": "Preamble", "summary": "You help customers.", "value": "You help customers."});
        assert_eq!(validate_fields(Kind::JobSetting, &setting), Ok(()));
        assert_eq!(
            validate_fields(Kind::JobSetting, &json!({"label": "North star"})),
            Ok(())
        );
        assert!(validate_fields(Kind::JobSetting, &json!({"summary": "x"})).is_err());
        let floor = json!({"label": "Return window", "value": 14, "floor": "min"});
        assert_eq!(validate_fields(Kind::JobSetting, &floor), Ok(()));
        assert!(
            validate_fields(
                Kind::JobSetting,
                &json!({"label": "W", "value": "14", "floor": "min"})
            )
            .is_err()
        );
        assert!(
            validate_fields(
                Kind::JobSetting,
                &json!({"label": "W", "value": 1, "floor": "up"})
            )
            .is_err()
        );
    }

    fn codes(r: Result<(), Vec<FieldError>>) -> Vec<&'static str> {
        r.err().unwrap_or_default().iter().map(|e| e.code).collect()
    }

    #[test]
    fn theme_and_prior_carry_their_numbers() {
        let full = json!({"label": "Issue", "summary": "s", "order": 3, "escalation_weight": 0.6});
        assert_eq!(validate_fields(Kind::JourneyTheme, &full), Ok(()));
        assert_eq!(
            validate_fields(Kind::JourneyTheme, &json!({"label": "Issue"})),
            Ok(())
        );
        assert_eq!(
            codes(validate_fields(
                Kind::JourneyTheme,
                &json!({"label": "Issue", "escalation_weight": 1.5})
            )),
            ["invalid_value"]
        );
        assert_eq!(
            codes(validate_fields(
                Kind::JourneyTheme,
                &json!({"label": "I", "nope": 1})
            )),
            ["unknown_field"]
        );
        let prior = json!({"label": "issue|support", "summary": "w", "weight": 0.4});
        assert_eq!(validate_fields(Kind::ThemeStagePrior, &prior), Ok(()));
        assert_eq!(
            codes(validate_fields(
                Kind::ThemeStagePrior,
                &json!({"label": "p", "weight": -0.1})
            )),
            ["invalid_value"]
        );
        assert_eq!(
            codes(validate_fields(
                Kind::ThemeStagePrior,
                &json!({"label": "p", "stage": "x"})
            )),
            ["unknown_field"]
        );
    }

    #[test]
    fn stage_carries_description_themes_and_messages_but_not_out_of_scope() {
        assert_eq!(
            validate_fields(Kind::JourneyStage, &json!({"label": "Support"})),
            Ok(())
        );
        let full = json!({
            "label": "Support", "summary": "s", "order": 2, "description": "d",
            "synonyms": ["help"], "themes": ["issue"],
            "messages": {"chat": ["Where is my order?"], "email": []}
        });
        assert_eq!(validate_fields(Kind::JourneyStage, &full), Ok(()));
        for bad in [
            json!({"label": "S", "out_of_scope": true}),
            json!({"label": "S", "messages": {"fax": ["x"]}}),
            json!({"label": "S", "messages": {"chat": [" "]}}),
            json!({"label": "S", "messages": {"chat": "x"}}),
        ] {
            assert!(validate_fields(Kind::JourneyStage, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn activity_embeds_objects_and_operations() {
        assert_eq!(
            validate_fields(Kind::Activity, &json!({"label": "Close"})),
            Ok(())
        );
        let full = json!({
            "label": "Start a return", "summary": "s", "area": "returns", "trigger": "customer asks",
            "actor": "customer", "also_in_stages": ["support"],
            "objects": [{"object": "case{return}", "role": "subject", "fields": ["id"], "required_inputs": ["order_id"]},
                        {"object": "order", "role": "context"}],
            "operations": [{"op": "order.read", "required": true},
                           {"op": "case.create", "variant": "return", "required": false},
                           {"op": "interaction.create"}]
        });
        assert_eq!(validate_fields(Kind::Activity, &full), Ok(()));
        for bad in [
            json!({"label": "A", "actor": "robot"}),
            json!({"label": "A", "objects": [{"object": "o", "role": "owner"}]}),
            json!({"label": "A", "objects": [{"object": " ", "role": "subject"}]}),
            json!({"label": "A", "objects": [{"object": "o", "role": "subject", "x": 1}]}),
            json!({"label": "A", "operations": [{"op": ""}]}),
            json!({"label": "A", "operations": [{"op": "a.b", "extra": 1}]}),
            json!({"label": "A", "unknown": 1}),
        ] {
            assert!(validate_fields(Kind::Activity, &bad).is_err(), "{bad}");
        }
        // activity_object and operation_expectation keep the summary shape
        for kind in [Kind::ActivityObject, Kind::OperationExpectation] {
            assert_eq!(
                validate_fields(kind, &json!({"label": "L", "summary": "s"})),
                Ok(())
            );
            assert!(validate_fields(kind, &json!({"label": "L", "objects": []})).is_err());
        }
    }

    #[test]
    fn activity_actor_employee_is_snake_case_and_distinct() {
        for (text, actor) in [
            ("customer", ActivityActor::Customer),
            ("company", ActivityActor::Company),
            ("employee", ActivityActor::Employee),
        ] {
            let row = json!({"label": "A", "actor": text});
            assert_eq!(validate_fields(Kind::Activity, &row), Ok(()), "{text}");
            let parsed: ActivityFields = serde_json::from_value(row).unwrap();
            assert_eq!(parsed.actor, Some(actor), "{text}");
            assert_eq!(serde_json::to_value(actor).unwrap(), json!(text));
        }
        assert!(
            validate_fields(Kind::Activity, &json!({"label": "A", "actor": "Employee"})).is_err()
        );
    }

    #[test]
    fn channel_stage_scope_and_card_parse_minimal_and_full() {
        let channel = json!({"label": "Chat", "summary": "s", "status": "v1", "note": "web chat"});
        assert_eq!(validate_fields(Kind::JourneyChannel, &channel), Ok(()));
        assert_eq!(
            validate_fields(Kind::JourneyChannel, &json!({"label": "Chat"})),
            Ok(())
        );
        for bad in [
            json!({"label": "C", "status": "soon"}),
            json!({"label": "C", "key": "chat"}),
            json!({"label": ""}),
        ] {
            assert!(
                validate_fields(Kind::JourneyChannel, &bad).is_err(),
                "{bad}"
            );
        }

        assert_eq!(
            validate_fields(Kind::StageScope, &json!({"out_of_scope": true})),
            Ok(())
        );
        let scope = json!({"label": "Billing", "summary": "s", "out_of_scope": false});
        assert_eq!(validate_fields(Kind::StageScope, &scope), Ok(()));
        for bad in [
            json!({}),
            json!({"label": "x"}),
            json!({"out_of_scope": "yes"}),
            json!({"out_of_scope": true, "stage": "support"}),
            json!({"label": " ", "out_of_scope": true}),
        ] {
            assert!(validate_fields(Kind::StageScope, &bad).is_err(), "{bad}");
        }

        assert_eq!(validate_fields(Kind::UseCaseCard, &json!({})), Ok(()));
        let card = json!({
            "label": "Start a return", "summary": "s", "examples": ["I want to return my shoes"],
            "metric": {"key": "returns_started", "label": "Returns started", "target": "90%", "current": null},
            "sample_dialogue": [{"who": "customer", "text": "Hi"}],
            "success_example": {"outcome": "ok"}, "error_example": {"outcome": "error"},
            "scenarios": [
                {"id": "s1", "style": "plain", "phrasing": "Return my order", "expected": "starts a return",
                 "place": "rule:RET-01", "cites": ["policy/returns"]},
                {"id": "s2", "style": "nl", "phrasing": "Ik wil retourneren", "expected": "starts a return"}
            ]
        });
        assert_eq!(validate_fields(Kind::UseCaseCard, &card), Ok(()));
        for bad in [
            json!({"unknown": 1}),
            json!({"metric": "90%"}),
            json!({"success_example": []}),
            json!({"scenarios": [{"id": "s", "style": "loud", "phrasing": "p", "expected": "e"}]}),
            json!({"scenarios": [{"id": "s", "style": "plain", "phrasing": "p", "expected": "e", "probe": {}}]}),
            json!({"scenarios": [{"id": "", "style": "plain", "phrasing": "p", "expected": "e"}]}),
            json!({"scenarios": [
                {"id": "s", "style": "plain", "phrasing": "p", "expected": "e"},
                {"id": "s", "style": "nl", "phrasing": "p", "expected": "e"}]}),
        ] {
            assert!(validate_fields(Kind::UseCaseCard, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn new_kinds_keys_and_scopes() {
        use crate::kinds::validate_key;
        use crate::layer::Scope;
        assert_eq!(validate_key(Kind::JourneyChannel, "contact_form"), Ok(()));
        assert!(validate_key(Kind::JourneyChannel, "Chat").is_err());
        assert_eq!(validate_key(Kind::StageScope, "support"), Ok(()));
        assert!(validate_key(Kind::StageScope, "support:x").is_err());
        assert_eq!(
            validate_key(Kind::UseCaseCard, "support:start_return"),
            Ok(())
        );
        assert!(validate_key(Kind::UseCaseCard, "Support:x").is_err());

        assert!(Kind::JourneyChannel.allowed_in(Scope::Base));
        assert!(Kind::JourneyChannel.allowed_in(Scope::Industry));
        assert!(!Kind::JourneyChannel.allowed_in(Scope::IndustryDept));
        assert!(!Kind::JourneyChannel.allowed_in(Scope::Profile));
        for scope in [Scope::Industry, Scope::BaseDept, Scope::IndustryDept] {
            assert!(Kind::StageScope.allowed_in(scope), "{scope}");
        }
        for scope in [Scope::Base, Scope::Profile, Scope::Job] {
            assert!(!Kind::StageScope.allowed_in(scope), "{scope}");
        }
        for scope in [
            Scope::Industry,
            Scope::BaseDept,
            Scope::IndustryDept,
            Scope::Job,
        ] {
            assert!(Kind::UseCaseCard.allowed_in(scope), "{scope}");
        }
        for scope in [Scope::Base, Scope::Profile] {
            assert!(!Kind::UseCaseCard.allowed_in(scope), "{scope}");
        }
    }

    fn obj(v: serde_json::Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn settings_inherit_and_floors_only_tighten() {
        // no value: the nearest value above stays
        let base = obj(json!({"label": "Stance", "value": "warm"}));
        let job = fold_setting(&base, &obj(json!({"label": "Stance (job)"}))).unwrap();
        assert_eq!(job["value"], json!("warm"));
        assert_eq!(job["label"], json!("Stance (job)"));
        let job = fold_setting(&base, &obj(json!({"label": "S", "value": "brisk"}))).unwrap();
        assert_eq!(job["value"], json!("brisk"));

        let min = obj(json!({"label": "Return window", "value": 14, "floor": "min"}));
        // tighter, equal and value-less patches pass; the floor is inherited
        for ok in [
            json!({"label": "W", "value": 30}),
            json!({"label": "W", "value": 14}),
            json!({"label": "W"}),
        ] {
            let got = fold_setting(&min, &obj(ok.clone())).unwrap_or_else(|e| panic!("{ok}: {e}"));
            assert_eq!(got["floor"], json!("min"), "{ok}");
        }
        assert!(fold_setting(&min, &obj(json!({"label": "W", "value": 7}))).is_err());
        assert!(
            fold_setting(
                &min,
                &obj(json!({"label": "W", "value": 30, "floor": "max"}))
            )
            .is_err()
        );
        assert!(tombstone_setting(&min).is_err());
        assert!(tombstone_setting(&base).is_ok());

        let max = obj(json!({"label": "Refund limit", "value": 100, "floor": "max"}));
        assert!(fold_setting(&max, &obj(json!({"label": "L", "value": 50}))).is_ok());
        assert!(fold_setting(&max, &obj(json!({"label": "L", "value": 150}))).is_err());
    }
}
