//! B8 T2a: the journey grid's wire shapes, 1:1 with `web/src/lib/api/types/journey.ts` (snake_case, as the API
//! sends them). Stages and themes are open sets (B8-10): a [`Slug`] validated `^[a-z][a-z0-9_]{1,40}$`, the
//! effective set comes from the journey rows, never from a closed enum.
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A stage or theme key (B8-10): `^[a-z][a-z0-9_]{1,40}$`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Slug(String);

/// The slug rule of B8-10 (also the fixture's `SLUG`).
pub fn is_slug(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=41).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

impl Slug {
    /// `None` when `s` is not a slug.
    pub fn new(s: &str) -> Option<Self> {
        is_slug(s).then(|| Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for Slug {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        if is_slug(&s) {
            Ok(Self(s))
        } else {
            Err(format!(
                "“{s}” is not a key: lower-case letters, digits and _ (2–41)"
            ))
        }
    }
}

impl From<Slug> for String {
    fn from(s: Slug) -> Self {
        s.0
    }
}

impl fmt::Display for Slug {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for Slug {
    fn eq(&self, o: &str) -> bool {
        self.0 == o
    }
}

pub type StageKey = Slug;
pub type ThemeKey = Slug;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Actor {
    Customer,
    Company,
    /// A known employee asking an internal desk (HR, IT service desk).
    Employee,
}

/// `journey_theme`: the customer state, compiled into GT options.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JourneyTheme {
    pub key: ThemeKey,
    pub label: String,
    pub order: i32,
    /// 0–1.
    pub escalation_weight: f64,
    /// The template layer whose row set this item last (B8 T2b: the fold's winning step; `None` on a hand-built
    /// grid). Not in the web fixture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

/// `journey_stage`: the capability column, compiled into GS options.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JourneyStage {
    pub key: StageKey,
    pub label: String,
    pub order: i32,
    pub description: String,
    pub synonyms: Vec<String>,
    /// Out of scope for this industry/job: rule 4 passes without an activity.
    pub out_of_scope: bool,
    pub themes: Vec<ThemeKey>,
    /// The template layer whose row set this item last (B8 T2b: the fold's winning step; `None` on a hand-built
    /// grid). Not in the web fixture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

/// `theme_stage_prior`: weight 0–1, the GS prior.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThemeStagePrior {
    pub theme: ThemeKey,
    pub stage: StageKey,
    pub weight: f64,
    /// The template layer whose row set this item last (B8 T2b: the fold's winning step; `None` on a hand-built
    /// grid). Not in the web fixture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKey {
    Chat,
    Email,
    ContactForm,
    Phone,
    Visit,
    Signing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelStatus {
    V1,
    Later,
    OutOfScope,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JourneyChannel {
    pub key: ChannelKey,
    pub label: String,
    pub status: ChannelStatus,
    pub note: String,
    /// The template layer whose row set this item last (B8 T2b: the fold's winning step; `None` on a hand-built
    /// grid). Not in the web fixture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectRole {
    Subject,
    Context,
}

/// `activity_object`: an object an activity reads or writes, its field subset and required slots.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActivityObject {
    /// Canonical object key, optionally with a variant (`case{return}`).
    pub object: String,
    pub role: ObjectRole,
    pub fields: Vec<String>,
    pub required_inputs: Vec<String>,
}

/// The canonical key of an activity object: `case{return}` → `case` (fixture `objectKey`).
pub fn object_key(o: &str) -> &str {
    match o.find('{') {
        Some(i) if o.ends_with('}') => &o[..i],
        _ => o,
    }
}

/// `operation_expectation`: `object.verb{variant}` with required / optional.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationExpectation {
    pub op: String,
    pub variant: Option<String>,
    pub required: bool,
}

/// `activity`: key `stage:activity`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JourneyActivity {
    pub key: String,
    pub stage: StageKey,
    pub actor: Actor,
    pub label: String,
    pub trigger: String,
    pub also_in_stages: Vec<StageKey>,
    pub objects: Vec<ActivityObject>,
    pub operations: Vec<OperationExpectation>,
    /// The technical form (actor, operations); kept as authored, the object pages scan it. Never shown as text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// The plain line a customer or employee would recognise; the description of the activity's generated option in
    /// the activity gate GA. Editable in the designer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// D61: the department area; absent on an activity no area claims yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub area: Option<String>,
    /// The template layer whose row set this item last (B8 T2b: the fold's winning step; `None` on a hand-built
    /// grid). Not in the web fixture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    /// Owner 2026-10-26: the template marks the activity required (a tenant cannot hide it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub required: bool,
}

/// Per stage × channel: example customer messages.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StageMessage {
    pub stage: StageKey,
    pub channel: ChannelKey,
    pub utterances: Vec<String>,
    /// The template layer whose row set this item last (B8 T2b: the fold's winning step; `None` on a hand-built
    /// grid). Not in the web fixture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

/// The `stage` of a knowledge kind that applies to every stage (D107).
pub const ANY_STAGE: &str = "*";

/// `stage_knowledge_kind`: what the tenant's store must hold for a stage. `stage` is a stage key or `*` (D107), so
/// it is a plain string, not a [`Slug`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StageKnowledgeKind {
    pub stage: String,
    pub kind: String,
    pub required: bool,
    pub description: String,
    /// The template layer whose row set this item last (B8 T2b: the fold's winning step; `None` on a hand-built
    /// grid). Not in the web fixture.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JourneyStatus {
    Tagged,
    Partial,
    Draft,
}

/// `tagged` (both layers tagged, nothing untagged since) | `partial` (one layer tagged, or untagged revisions after
/// a tag) | `draft` (never tagged).
pub fn journey_status(tag: Option<&str>, dept_tag: Option<&str>, draft_rev: u32) -> JourneyStatus {
    match (tag, dept_tag) {
        (None, None) => JourneyStatus::Draft,
        (Some(_), Some(_)) if draft_rev == 0 => JourneyStatus::Tagged,
        _ => JourneyStatus::Partial,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Red,
    Amber,
    /// Information only (rule 7): never blocks, never counts as a warning.
    Info,
}

/// A validation finding (11§7 rules 1–7). `red` blocks tagging.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationFinding {
    pub rule: u8,
    pub severity: Severity,
    pub message: String,
    pub stage: Option<String>,
    pub activity: Option<String>,
}

/// `GET /api/platform/journeys/{industry}`: the owner's grid for one industry + department.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndustryJourney {
    pub industry: String,
    pub label: String,
    pub department: String,
    pub status: JourneyStatus,
    pub tag: Option<String>,
    pub dept_tag: Option<String>,
    pub draft_rev: u32,
    pub updated_at: DateTime<Utc>,
    pub updated_by: String,
    pub themes: Vec<JourneyTheme>,
    pub stages: Vec<JourneyStage>,
    pub priors: Vec<ThemeStagePrior>,
    pub channels: Vec<JourneyChannel>,
    pub activities: Vec<JourneyActivity>,
    pub messages: Vec<StageMessage>,
    pub knowledge_kinds: Vec<StageKnowledgeKind>,
    /// Canonical objects of the industry's business model (rule 3).
    pub model_objects: Vec<String>,
    /// Computed on read.
    pub findings: Vec<ValidationFinding>,
    pub source_run: Option<i64>,
}

/// The generator's steps, in order (`GeneratorStepKey`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeneratorStepKey {
    Input,
    BusinessModel,
    Journey,
    Objects,
    Operations,
    Harmonise,
    Validate,
    UseCases,
    Package,
}

impl GeneratorStepKey {
    pub const ALL: [Self; 9] = [
        Self::Input,
        Self::BusinessModel,
        Self::Journey,
        Self::Objects,
        Self::Operations,
        Self::Harmonise,
        Self::Validate,
        Self::UseCases,
        Self::Package,
    ];

    /// The steps where the LLM drafts (fixture `DRAFT_STEPS`).
    pub const DRAFT: [Self; 6] = [
        Self::BusinessModel,
        Self::Journey,
        Self::Objects,
        Self::Operations,
        Self::Harmonise,
        Self::UseCases,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::BusinessModel => "business_model",
            Self::Journey => "journey",
            Self::Objects => "objects",
            Self::Operations => "operations",
            Self::Harmonise => "harmonise",
            Self::Validate => "validate",
            Self::UseCases => "use_cases",
            Self::Package => "package",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    pub fn drafts(self) -> bool {
        Self::DRAFT.contains(&self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_open_but_validated() {
        assert!(Slug::new("orientation").is_some());
        assert!(Slug::new("after_sales_2").is_some());
        let long = "x".repeat(42);
        for bad in ["", "a", "Order", "1st", "has-dash", "*", long.as_str()] {
            assert!(Slug::new(bad).is_none(), "{bad}");
        }
        let s: Result<JourneyStage, _> = serde_json::from_value(serde_json::json!({
            "key": "Bad Key", "label": "x", "order": 1, "description": "", "synonyms": [],
            "out_of_scope": false, "themes": []
        }));
        assert!(s.is_err());
    }

    #[test]
    fn object_key_strips_the_variant() {
        assert_eq!(object_key("case{return}"), "case");
        assert_eq!(object_key("order"), "order");
    }

    #[test]
    fn status_partial_after_untagged_revision() {
        assert_eq!(journey_status(None, None, 3), JourneyStatus::Draft);
        assert_eq!(
            journey_status(Some("industry/retail@v1"), Some("d@v1"), 0),
            JourneyStatus::Tagged
        );
        assert_eq!(
            journey_status(Some("industry/retail@v1"), Some("d@v1"), 1),
            JourneyStatus::Partial
        );
        assert_eq!(
            journey_status(Some("industry/retail@v1"), None, 0),
            JourneyStatus::Partial
        );
    }

    #[test]
    fn step_keys_round_trip() {
        for k in GeneratorStepKey::ALL {
            assert_eq!(GeneratorStepKey::parse(k.as_str()), Some(k));
            assert_eq!(
                serde_json::to_value(k).unwrap(),
                serde_json::json!(k.as_str())
            );
        }
        assert!(!GeneratorStepKey::Validate.drafts());
        assert!(GeneratorStepKey::UseCases.drafts());
    }
}
