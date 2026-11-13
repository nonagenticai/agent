//! J7 gates (B5 T8, T8a): the questions a job asks and the tenant's refinements of them, as the routes show them and
//! the runtime (T12) asks them — one fold for both, so Production and Test cannot drift.
//!
//! - Base questions are READ (plan R1): the library at the design's pins (`Bases::gates`, B6's options already the
//!   job's languages, D59), then the tenant's own questions (`gate_question` places), by key. A tenant question whose
//!   key the library now has is shadowed by the library row (B5 T4 reports it as a `base_shadowed` conflict).
//! - Refinements: a LIBRARY option's label / description / hidden is B5's `gate` place `{gate, option}`; a GENERATED
//!   option's (GT/GS/GA/GO, `generated: true`) is B2's `overlay` place of object `gate_option`, key
//!   `<GATE> · <option>` (an override item or a tombstone = hidden, plan R2). A question's threshold OR rule is the
//!   `gate` place `{gate, option: null}` (the later edit clears the other, fixture `upsertRef`).
//! - T8a: `gate_setting` places (`{enabled, customer_reply}`): a disabled question is never asked (a safety question
//!   is always on); the customer text is the setting, else the question's own (`customer_reply` field of the library
//!   row / the tenant question), else the care-bus default ([`customer_text`]).
//! - The rule helpers are `lib/ontology/gateRules.ts` (`ruleError`, `normaliseRule`) and the fixture's
//!   `checkThreshold` / `tunedRule` / `ruleShort`, ported 1:1.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use agent_model::compile::CatalogueItem;
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Map, Value};
use sqlx::PgConnection;

use super::stats::Stats;
use crate::design::{self, DesignOf, DraftRef, JobRef, Registry};
use crate::error::ApiResult;
use crate::job_design::base::{self, Bases};
use crate::job_design::targets::gate_settings::GateSetting;
use crate::job_design::types::{
    ApprovedRelease, EffGateOption, EffGateQuestion, GateAction, GateChange, GateChangeField,
    GateChangeShips, GateDecision, GateFail, GateFireWhen, GateGroup, GatePoint, GateRule,
    GateType, JobGateReleases, LiveRelease, QuestionOrigin,
};
use crate::release::compile::TemplateCompiler;
use crate::release::overlay::GATE_OPTION;

/// F-16: the customer text of a handover without a question text of its own (care-bus default).
pub const DEFAULT_HANDOVER_REPLY: &str =
    "I'm handing this over to a colleague, who will get back to you shortly.";
/// F-16: the customer text of a block without a question text of its own (care-bus default).
pub const DEFAULT_BLOCK_REPLY: &str =
    "I can't continue with this request here. A colleague will follow up with you by email.";
/// F-17: why a disabled question is not asked (`SkippedQuestion.reason`).
pub const DISABLED_REASON: &str = "Disabled for this job";
/// A customer reply is 1–500 characters (`job_gate_question_setting` / `job_gate_question` CHECKs).
pub const MAX_REPLY_CHARS: usize = 500;

const TOMBSTONE: &str = "tombstone";

// ---- base questions -------------------------------------------------------------------------------------------------

/// One option of a question as its base has it.
#[derive(Clone, Debug, PartialEq)]
pub struct BaseOption {
    pub key: String,
    pub label: String,
    pub description: String,
    pub generated: bool,
}

/// One question a job asks, before the tenant's refinements: a library row or a tenant question.
#[derive(Clone, Debug, PartialEq)]
pub struct Question {
    pub key: String,
    pub origin: QuestionOrigin,
    pub group: GateGroup,
    pub layer: String,
    pub point: GatePoint,
    pub kind: GateType,
    pub question: String,
    pub options: Vec<BaseOption>,
    pub generated_from: Option<String>,
    pub threshold: String,
    pub threshold_value: Option<f64>,
    pub fail: GateFail,
    pub safety: bool,
    pub effect: String,
    pub rule: GateRule,
    pub action: GateAction,
    pub tools: Option<Vec<String>>,
    pub updated_by: Option<String>,
    pub updated_at: DateTime<Utc>,
    pub customer_reply: Option<String>,
}

impl Question {
    pub fn option(&self, key: &str) -> Option<&BaseOption> {
        self.options.iter().find(|o| o.key == key)
    }
}

#[derive(Debug, Deserialize)]
struct RawOption {
    key: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    generated: bool,
}

impl RawOption {
    fn base(self) -> BaseOption {
        BaseOption {
            label: self.label.unwrap_or_else(|| self.key.clone()),
            description: self.description.unwrap_or_default(),
            generated: self.generated,
            key: self.key,
        }
    }
}

/// The fields of a library `gate_question` row (seed `rows.json`); unknown fields (`summary`, `origin`) ignored.
#[derive(Debug, Deserialize)]
struct LibraryFields {
    #[serde(default = "base_group")]
    group: GateGroup,
    point: GatePoint,
    #[serde(rename = "type")]
    kind: GateType,
    question: String,
    #[serde(default)]
    options: Vec<RawOption>,
    #[serde(default)]
    generated_from: Option<String>,
    #[serde(default)]
    threshold: Option<String>,
    #[serde(default)]
    threshold_value: Option<f64>,
    fail: GateFail,
    #[serde(default)]
    safety: bool,
    #[serde(default)]
    effect: Option<String>,
    rule: GateRule,
    action: GateAction,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    updated_by: Option<String>,
    #[serde(default)]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    customer_reply: Option<String>,
}

fn base_group() -> GateGroup {
    GateGroup::Base
}

/// The value of a `gate_question` place (`targets::gates::GateQuestion`).
#[derive(Debug, Deserialize)]
struct TenantValue {
    point: GatePoint,
    #[serde(rename = "type")]
    kind: GateType,
    question: String,
    options: Vec<RawOption>,
    #[serde(default)]
    threshold: Option<String>,
    #[serde(default)]
    threshold_value: Option<f64>,
    fail: GateFail,
    #[serde(default)]
    effect: Option<String>,
    rule: GateRule,
    action: GateAction,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    customer_reply: Option<String>,
}

fn epoch() -> DateTime<Utc> {
    DateTime::<Utc>::from_timestamp(0, 0).unwrap_or_default()
}

/// A library question from its catalogue item; `None` (logged) when the row does not have a question's shape.
pub fn library_question(g: &CatalogueItem) -> Option<Question> {
    let f: LibraryFields = match serde_json::from_value(Value::Object(g.fields.clone())) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(gate = %g.key, error = %e, "gate_question_unreadable");
            return None;
        }
    };
    Some(Question {
        key: g.key.clone(),
        origin: QuestionOrigin::Platform,
        group: f.group,
        layer: g.layer.clone(),
        point: f.point,
        kind: f.kind,
        question: f.question,
        options: f.options.into_iter().map(RawOption::base).collect(),
        generated_from: f.generated_from,
        threshold: f.threshold.unwrap_or_default(),
        threshold_value: f.threshold_value,
        fail: f.fail,
        safety: f.safety,
        effect: f.effect.unwrap_or_default(),
        rule: f.rule,
        action: f.action,
        tools: f.tools,
        updated_by: f.updated_by,
        updated_at: f.updated_at.unwrap_or_else(epoch),
        customer_reply: f.customer_reply.filter(|t| !t.trim().is_empty()),
    })
}

// ---- the tenant's rows of one design --------------------------------------------------------------------------------

/// A refinement of a library option (`gate` place `{gate, option}`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct OptionRef {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub base_hash: String,
}

/// A refinement of a question's threshold or rule (`gate` place `{gate, option: null}`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub struct RuleRef {
    #[serde(default)]
    pub threshold_value: Option<f64>,
    #[serde(default)]
    pub rule: Option<GateRule>,
    #[serde(default)]
    pub base_hash: Option<String>,
}

/// The tenant's overlay row of a generated option.
#[derive(Clone, Debug, PartialEq)]
pub enum OverlayOption {
    Hidden,
    Item(Map<String, Value>),
}

/// The tenant's gate rows of one design (`gate`, `gate_question`, `gate_setting` and the `gate_option` overlay places).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GateRows {
    /// (gate, option) → refinement of a library option.
    pub options: BTreeMap<(String, String), OptionRef>,
    /// gate → threshold / rule refinement.
    pub rules: BTreeMap<String, RuleRef>,
    /// The tenant's own questions, by key.
    pub questions: BTreeMap<String, Question>,
    pub settings: BTreeMap<String, GateSetting>,
    /// `<GATE> · <option>` → the overlay row of a generated option.
    pub overlay: BTreeMap<String, OverlayOption>,
}

/// The overlay key of a generated option (`release/compile.rs` `gate_options`).
pub fn overlay_key(gate: &str, option: &str) -> String {
    format!("{gate} · {option}")
}

fn ref_str<'r>(r: &'r DraftRef, f: &str) -> Option<&'r str> {
    r.get(f).and_then(Value::as_str)
}

/// A stored value of `kind` at `at`, or `None` (logged) when it has another shape: a value the reader cannot decode
/// (written by an older shape, or straight through the design target) is skipped, never a 500 — every reader of the
/// gate rows (the routes, readiness on approve/apply, `RuntimeDesign::read`) would otherwise fail on it, as
/// `toolset::role_settings_of` already skips an unreadable `role_setting`.
fn decoded<T: serde::de::DeserializeOwned>(kind: &str, at: &str, v: Value) -> Option<T> {
    match serde_json::from_value(v) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::warn!(kind, at, error = %e, "gate design value unreadable; skipped");
            None
        }
    }
}

/// The tenant's gate rows of design `of` (B2's read API: the working rows for `Draft`, the snapshot for `Live`).
pub async fn rows(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<GateRows> {
    let mut out = GateRows::default();
    for r in design::values_of_kind(&mut *c, reg, job, of, "gate").await? {
        let (Some(gate), Some(v)) = (ref_str(&r.r#ref, "gate"), r.snap.filter(|v| !v.is_null()))
        else {
            continue;
        };
        match ref_str(&r.r#ref, "option") {
            Some(option) => {
                if let Some(o) = decoded("gate", &overlay_key(gate, option), v) {
                    out.options.insert((gate.to_owned(), option.to_owned()), o);
                }
            }
            None => {
                if let Some(rule) = decoded("gate", gate, v) {
                    out.rules.insert(gate.to_owned(), rule);
                }
            }
        }
    }
    let meta: BTreeMap<String, (String, DateTime<Utc>)> =
        sqlx::query_as::<_, (String, String, DateTime<Utc>)>(
            "SELECT key, updated_by, updated_at FROM job_gate_question WHERE job_id = $1",
        )
        .bind(job.id)
        .fetch_all(&mut *c)
        .await?
        .into_iter()
        .map(|(k, by, at)| (k, (by, at)))
        .collect();
    for r in design::values_of_kind(&mut *c, reg, job, of, "gate_question").await? {
        let (Some(key), Some(v)) = (ref_str(&r.r#ref, "gate"), r.snap.filter(|v| !v.is_null()))
        else {
            continue;
        };
        let Some(t) = decoded::<TenantValue>("gate_question", key, v) else {
            continue;
        };
        let (by, at) = meta
            .get(key)
            .map(|(by, at)| (Some(by.clone()), *at))
            .unwrap_or((None, epoch()));
        out.questions.insert(
            key.to_owned(),
            Question {
                key: key.to_owned(),
                origin: QuestionOrigin::Tenant,
                group: GateGroup::Tenant,
                layer: "tenant".to_owned(),
                point: t.point,
                kind: t.kind,
                question: t.question,
                options: t.options.into_iter().map(RawOption::base).collect(),
                generated_from: None,
                threshold: t.threshold.unwrap_or_else(|| "any true".to_owned()),
                threshold_value: t.threshold_value,
                fail: t.fail,
                safety: false,
                effect: t.effect.unwrap_or_default(),
                rule: t.rule,
                action: t.action,
                tools: t.tools,
                updated_by: by,
                updated_at: at,
                customer_reply: t.customer_reply,
            },
        );
    }
    for r in design::values_of_kind(&mut *c, reg, job, of, "gate_setting").await? {
        let (Some(gate), Some(v)) = (ref_str(&r.r#ref, "gate"), r.snap.filter(|v| !v.is_null()))
        else {
            continue;
        };
        if let Some(setting) = decoded("gate_setting", gate, v) {
            out.settings.insert(gate.to_owned(), setting);
        }
    }
    for r in design::values_of_kind(&mut *c, reg, job, of, "overlay").await? {
        if ref_str(&r.r#ref, "object") != Some(GATE_OPTION) {
            continue;
        }
        let (Some(key), Some(v)) = (ref_str(&r.r#ref, "key"), r.snap.filter(|v| !v.is_null()))
        else {
            continue;
        };
        let row = if v.get("kind").and_then(Value::as_str) == Some(TOMBSTONE) {
            OverlayOption::Hidden
        } else {
            OverlayOption::Item(
                v.get("item")
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default(),
            )
        };
        out.overlay.insert(key.to_owned(), row);
    }
    Ok(out)
}

/// The bases and the tenant's gate rows of design `of`.
pub async fn of_design(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    of: &DesignOf,
) -> ApiResult<(Arc<Bases>, GateRows)> {
    let bases = base::at(&mut *c, reg, compiler, job, of).await?;
    let rows = rows(&mut *c, reg, job, of).await?;
    Ok((bases, rows))
}

// ---- the fold -------------------------------------------------------------------------------------------------------

/// Every question the job asks, before refinements: the library at the pins (its order), then the tenant's
/// questions by key (a tenant key the library has is shadowed).
pub fn questions(bases: &Bases, rows: &GateRows) -> Vec<Question> {
    let mut out: Vec<Question> = bases.gates.iter().filter_map(library_question).collect();
    let library: BTreeSet<String> = out.iter().map(|q| q.key.clone()).collect();
    out.extend(
        rows.questions
            .values()
            .filter(|q| !library.contains(&q.key))
            .cloned(),
    );
    out
}

/// Question `key` of the job, before refinements.
pub fn question(bases: &Bases, rows: &GateRows, key: &str) -> Option<Question> {
    questions(bases, rows).into_iter().find(|q| q.key == key)
}

/// The template rule with a tuned threshold in place: gte / lt move the line, `in` the minimum confidence (fixture
/// `tunedRule`).
pub fn tuned_rule(rule: &GateRule, tuned: Option<f64>) -> GateRule {
    let mut r = rule.clone();
    if let Some(t) = tuned {
        if r.fire_when == GateFireWhen::In {
            r.min_confidence = Some(t);
        } else {
            r.threshold = Some(t);
        }
    }
    r
}

/// Is option `o` of `q` hidden in this design (a B5 refinement, or a B2 tombstone of a generated option)?
pub fn option_hidden(q: &Question, o: &BaseOption, rows: &GateRows) -> bool {
    if o.generated {
        matches!(
            rows.overlay.get(&overlay_key(&q.key, &o.key)),
            Some(OverlayOption::Hidden)
        )
    } else {
        rows.options
            .get(&(q.key.clone(), o.key.clone()))
            .is_some_and(|r| r.hidden)
    }
}

fn eff_option(q: &Question, o: &BaseOption, rows: &GateRows, stats: &Stats) -> EffGateOption {
    let (label, description, refined) = if o.generated {
        match rows.overlay.get(&overlay_key(&q.key, &o.key)) {
            Some(OverlayOption::Item(item)) => (
                item.get("label")
                    .and_then(Value::as_str)
                    .filter(|l| !l.is_empty())
                    .unwrap_or(o.label.as_str())
                    .to_owned(),
                item.get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                true,
            ),
            Some(OverlayOption::Hidden) => (o.label.clone(), o.description.clone(), true),
            None => (o.label.clone(), o.description.clone(), false),
        }
    } else {
        match rows.options.get(&(q.key.clone(), o.key.clone())) {
            Some(r) => (
                r.label
                    .clone()
                    .filter(|l| !l.is_empty())
                    .unwrap_or_else(|| o.label.clone()),
                r.description
                    .clone()
                    .unwrap_or_else(|| o.description.clone()),
                true,
            ),
            None => (o.label.clone(), o.description.clone(), false),
        }
    };
    EffGateOption {
        key: o.key.clone(),
        label,
        description,
        generated: o.generated,
        base_label: o.label.clone(),
        hidden: option_hidden(q, o, rows),
        refined,
        verdicts: stats.verdicts(&q.key, &o.key),
    }
}

/// One question as J7 shows it and the runtime asks it (fixture `effGate`, + T8a's setting).
pub fn eff_gate(q: &Question, rows: &GateRows, stats: &Stats) -> EffGateQuestion {
    let r = rows.rules.get(&q.key);
    let tuned = r.and_then(|r| r.threshold_value);
    let refined_rule = r.and_then(|r| r.rule.clone());
    let setting = rows.settings.get(&q.key);
    let customer_reply_default = q.customer_reply.clone();
    EffGateQuestion {
        key: q.key.clone(),
        group: q.group,
        layer: q.layer.clone(),
        point: q.point,
        kind: q.kind,
        question: q.question.clone(),
        options: q
            .options
            .iter()
            .map(|o| eff_option(q, o, rows, stats))
            .collect(),
        generated_from: q.generated_from.clone(),
        threshold: q.threshold.clone(),
        threshold_value: tuned.or(q.threshold_value),
        fail: q.fail,
        safety: q.safety,
        effect: q.effect.clone(),
        rule: refined_rule
            .clone()
            .unwrap_or_else(|| tuned_rule(&q.rule, tuned)),
        action: q.action,
        tools: q.tools.clone(),
        origin: q.origin,
        updated_by: q.updated_by.clone(),
        updated_at: q.updated_at,
        base_threshold_value: q.threshold_value,
        threshold_refined: tuned.is_some(),
        base_rule: q.rule.clone(),
        rule_refined: refined_rule.is_some(),
        stats: stats.of(&q.key, q.kind == GateType::Choice),
        enabled: q.safety || setting.and_then(|s| s.enabled).unwrap_or(true),
        customer_reply: setting
            .and_then(|s| s.customer_reply.clone())
            .or_else(|| customer_reply_default.clone()),
        customer_reply_default,
    }
}

/// Every question of the design, effective.
pub fn effective(bases: &Bases, rows: &GateRows, stats: &Stats) -> Vec<EffGateQuestion> {
    questions(bases, rows)
        .iter()
        .map(|q| eff_gate(q, rows, stats))
        .collect()
}

/// F-17: why `q` is not asked, or `None` (a disabled question; `SkippedQuestion.reason`).
pub fn skip_reason(q: &EffGateQuestion) -> Option<&'static str> {
    (!q.enabled).then_some(DISABLED_REASON)
}

/// F-16: what the customer reads when `q` decides `decision`: its effective `customer_reply`, else the care-bus
/// default of a handover / a block; `None` for `allow`.
pub fn customer_text(q: &EffGateQuestion, decision: GateDecision) -> Option<String> {
    let default = match decision {
        GateDecision::Allow => return None,
        GateDecision::Handover => DEFAULT_HANDOVER_REPLY,
        GateDecision::Block => DEFAULT_BLOCK_REPLY,
    };
    Some(
        q.customer_reply
            .clone()
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| default.to_owned()),
    )
}

// ---- rule helpers (gateRules.ts) -------------------------------------------------------------------------------------

/// JavaScript `String(n)` of a finite number (`4`, `0.65`).
pub fn js_num(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn js_opt(n: Option<f64>) -> String {
    n.map_or_else(|| "null".to_owned(), js_num)
}

/// The rule as it would be saved: `in` keeps fire values and drops the threshold, gte / lt the reverse
/// (`normaliseRule`).
pub fn normalise_rule(rule: &GateRule) -> GateRule {
    let is_in = rule.fire_when == GateFireWhen::In;
    let num = |v: Option<f64>| v.filter(|x| x.is_finite());
    GateRule {
        fire_when: rule.fire_when,
        threshold: if is_in { None } else { num(rule.threshold) },
        fire_values: if is_in {
            Some(rule.fire_values.clone().unwrap_or_default())
        } else {
            None
        },
        min_confidence: num(rule.min_confidence),
    }
}

/// Why `rule` cannot be saved for a question of `kind` with `options`, or `None` (`ruleError`).
pub fn rule_error(kind: GateType, options: &[BaseOption], rule: &GateRule) -> Option<String> {
    let prob = |v: Option<f64>| v.is_some_and(|x| x.is_finite() && (0.0..=1.0).contains(&x));
    if rule.min_confidence.is_some() && !prob(rule.min_confidence) {
        return Some("The minimum confidence is between 0 and 1 (blank: any).".into());
    }
    if rule.fire_when == GateFireWhen::In {
        if kind != GateType::Choice {
            return Some("“in” only applies to choice questions.".into());
        }
        let vals = rule.fire_values.clone().unwrap_or_default();
        if vals.is_empty() {
            return Some("Pick at least one answer that fires.".into());
        }
        let unknown: Vec<&str> = vals
            .iter()
            .filter(|v| !options.iter().any(|o| &o.key == *v))
            .map(String::as_str)
            .collect();
        if !options.is_empty() && !unknown.is_empty() {
            return Some(format!("Not an option: {}.", unknown.join(", ")));
        }
        return None;
    }
    let Some(t) = rule.threshold.filter(|t| t.is_finite()) else {
        return Some("Set a threshold.".into());
    };
    if kind == GateType::Score {
        let n = options.len();
        if t.fract() != 0.0 || t < 1.0 || t > n as f64 {
            return Some(format!("A score threshold is a level from 1 to {n}."));
        }
    } else if !prob(Some(t)) {
        return Some("The threshold is a probability between 0 and 1.".into());
    }
    None
}

/// Why `v` is not a threshold of a question of `kind` with `n` options, or `None` (fixture `checkThreshold`).
pub fn threshold_error(kind: GateType, n: usize, v: f64) -> Option<String> {
    if kind == GateType::Score {
        if !v.is_finite() || v.fract() != 0.0 || v < 1.0 || v > n as f64 {
            return Some(format!("A score threshold is a level from 1 to {n}."));
        }
    } else if !(v > 0.0 && v <= 1.0) {
        return Some("A choice threshold is between 0 and 1.".into());
    }
    None
}

/// `gte 0.6`, `in issue|need_change @0.5` (fixture `ruleShort`).
pub fn rule_short(r: &GateRule) -> String {
    let when = match r.fire_when {
        GateFireWhen::Gte => "gte",
        GateFireWhen::Lt => "lt",
        GateFireWhen::In => "in",
    };
    let what = if r.fire_when == GateFireWhen::In {
        r.fire_values.clone().unwrap_or_default().join("|")
    } else {
        js_opt(r.threshold)
    };
    let min = r
        .min_confidence
        .map(|m| format!(" @{}", js_num(m)))
        .unwrap_or_default();
    format!("{when} {what}{min}")
}

// ---- releases and changes (J7) --------------------------------------------------------------------------------------

/// `job_release` row: `(release_id, state, approved_at, released_at)`.
type ReleaseRow = (String, String, DateTime<Utc>, Option<DateTime<Utc>>);

/// The applied and the approved release of a job (fixture `gateReleases`).
pub async fn releases(c: &mut PgConnection, job: &JobRef) -> ApiResult<JobGateReleases> {
    let rows: Vec<ReleaseRow> = sqlx::query_as(
        "SELECT release_id, state, approved_at, released_at FROM job_release \
          WHERE job_id = $1 AND state IN ('current', 'approved') \
          ORDER BY approved_at DESC, release_id DESC",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    let live = rows.iter().find_map(|(id, state, _, released)| {
        (state == "current")
            .then_some(())
            .and(*released)
            .map(|released_at| LiveRelease {
                id: id.clone(),
                released_at,
            })
    });
    let approved = rows
        .iter()
        .find(|(_, state, _, _)| state == "approved")
        .map(|(id, _, at, _)| ApprovedRelease {
            id: id.clone(),
            approved_at: *at,
        });
    Ok(JobGateReleases { live, approved })
}

/// Where the change at ledger `key` stands (D54, B2's release states): no ledger row = it is what Production runs;
/// a ledger row bundled in an approved (cut, not applied) release = `approved`; else a draft for the next cut.
async fn ship_state(
    c: &mut PgConnection,
    job: &JobRef,
    key: &str,
    rel: &JobGateReleases,
) -> ApiResult<(GateChangeShips, Option<String>)> {
    if design::ledger::find(&mut *c, job, key).await?.is_none() {
        return Ok((
            GateChangeShips::Live,
            rel.live.as_ref().map(|l| l.id.clone()),
        ));
    }
    let cuts = design::ledger::approved_cuts(&mut *c, job, key).await?;
    Ok(match cuts.into_iter().next() {
        Some(id) => (GateChangeShips::Approved, Some(id)),
        None => (GateChangeShips::NextCut, None),
    })
}

/// One change before its release state and the person's name.
struct Pending {
    ledger_key: String,
    gate: String,
    option: Option<String>,
    field: GateChangeField,
    from: String,
    to: String,
    by: String,
    at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct OptionRow {
    gate: String,
    option: String,
    label: Option<String>,
    description: Option<String>,
    hidden: bool,
    updated_by: String,
    updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    gate: String,
    threshold_value: Option<f64>,
    rule: Option<Value>,
    updated_by: String,
    updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct OverlayRow {
    key: String,
    kind: String,
    item: Option<Value>,
    updated_by: String,
    updated_at: DateTime<Utc>,
}

/// The ledger key of the place `pairs` of target `t`.
fn key_of(t: &dyn design::DesignTarget, pairs: &[(&str, Option<&str>)]) -> String {
    let mut m = DraftRef::new();
    for (k, v) in pairs {
        m.insert(
            (*k).to_owned(),
            v.map_or(Value::Null, |s| Value::String(s.to_owned())),
        );
    }
    t.key(&m)
}

/// J7 `changes`: the job's tenant refinements of its gates, field by field (fixture `gateChanges`), from the
/// working rows (B5's tables and B2's `gate_option` overlay rows), each with the release it ships in; newest first.
pub async fn changes(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    bases: &Bases,
    rows: &GateRows,
    rel: &JobGateReleases,
) -> ApiResult<Vec<GateChange>> {
    let all = questions(bases, rows);
    let find = |k: &str| all.iter().find(|q| q.key == k);
    let gate_t = reg.target("gate")?;
    let question_t = reg.target("gate_question")?;
    let overlay_t = reg.target("overlay")?;
    let mut pending: Vec<Pending> = Vec::new();
    for q in rows.questions.values() {
        pending.push(Pending {
            ledger_key: key_of(question_t, &[("gate", Some(q.key.as_str()))]),
            gate: q.key.clone(),
            option: None,
            field: GateChangeField::Question,
            from: "not asked".into(),
            to: "added".into(),
            by: q.updated_by.clone().unwrap_or_default(),
            at: q.updated_at,
        });
    }
    let rule_rows = sqlx::query_as::<_, RuleRow>(
        "SELECT gate, threshold_value::float8 AS threshold_value, rule, updated_by, updated_at \
           FROM job_gate_rule_refinement WHERE job_id = $1",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    for r in rule_rows {
        let Some(q) = find(&r.gate) else { continue };
        let ledger_key = key_of(gate_t, &[("gate", Some(r.gate.as_str())), ("option", None)]);
        if let Some(t) = r.threshold_value {
            pending.push(Pending {
                ledger_key: ledger_key.clone(),
                gate: r.gate.clone(),
                option: None,
                field: GateChangeField::Threshold,
                from: js_opt(q.threshold_value),
                to: js_num(t),
                by: r.updated_by.clone(),
                at: r.updated_at,
            });
        }
        if let Some(rule) = r
            .rule
            .filter(|v| !v.is_null())
            .and_then(|v| serde_json::from_value::<GateRule>(v).ok())
        {
            pending.push(Pending {
                ledger_key,
                gate: r.gate.clone(),
                option: None,
                field: GateChangeField::Rule,
                from: rule_short(&q.rule),
                to: rule_short(&rule),
                by: r.updated_by,
                at: r.updated_at,
            });
        }
    }
    let option_rows = sqlx::query_as::<_, OptionRow>(
        "SELECT gate, option, label, description, hidden, updated_by, updated_at \
           FROM job_gate_option_refinement WHERE job_id = $1",
    )
    .bind(job.id)
    .fetch_all(&mut *c)
    .await?;
    for r in option_rows {
        let Some(q) = find(&r.gate) else { continue };
        let op = q.option(&r.option);
        let ledger_key = key_of(
            gate_t,
            &[
                ("gate", Some(r.gate.as_str())),
                ("option", Some(r.option.as_str())),
            ],
        );
        let mut push = |field, from: String, to: String| {
            pending.push(Pending {
                ledger_key: ledger_key.clone(),
                gate: r.gate.clone(),
                option: Some(r.option.clone()),
                field,
                from,
                to,
                by: r.updated_by.clone(),
                at: r.updated_at,
            });
        };
        if let Some(l) = r.label.clone().filter(|l| !l.is_empty()) {
            push(
                GateChangeField::Label,
                op.map(|o| o.label.clone()).unwrap_or_default(),
                l,
            );
        }
        if let Some(d) = r.description.clone().filter(|d| !d.is_empty()) {
            push(
                GateChangeField::Description,
                op.map(|o| o.description.clone()).unwrap_or_default(),
                d,
            );
        }
        if r.hidden {
            push(GateChangeField::Hidden, "offered".into(), "hidden".into());
        }
    }
    let overlay_rows = sqlx::query_as::<_, OverlayRow>(
        "SELECT key, kind, item, updated_by, updated_at FROM job_overlay_row \
          WHERE job_id = $1 AND object = $2",
    )
    .bind(job.id)
    .bind(GATE_OPTION)
    .fetch_all(&mut *c)
    .await?;
    for r in overlay_rows {
        let Some((gate, option)) = r.key.split_once(" · ") else {
            continue;
        };
        let Some(q) = find(gate) else { continue };
        let op = q.option(option);
        let ledger_key = key_of(
            overlay_t,
            &[("object", Some(GATE_OPTION)), ("key", Some(r.key.as_str()))],
        );
        let mut push = |field, from: String, to: String| {
            pending.push(Pending {
                ledger_key: ledger_key.clone(),
                gate: gate.to_owned(),
                option: Some(option.to_owned()),
                field,
                from,
                to,
                by: r.updated_by.clone(),
                at: r.updated_at,
            });
        };
        if r.kind == TOMBSTONE {
            push(GateChangeField::Hidden, "offered".into(), "hidden".into());
            continue;
        }
        let item = r.item.as_ref().and_then(Value::as_object);
        let text = |f: &str| {
            item.and_then(|m| m.get(f))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let (base_label, base_description) = op
            .map(|o| (o.label.clone(), o.description.clone()))
            .unwrap_or_default();
        let label = text("label");
        if !label.is_empty() && label != base_label {
            push(GateChangeField::Label, base_label, label);
        }
        let description = text("description");
        if description != base_description {
            push(GateChangeField::Description, base_description, description);
        }
    }
    let mut subs: Vec<String> = pending.iter().map(|p| p.by.clone()).collect();
    subs.sort();
    subs.dedup();
    subs.retain(|s| !s.is_empty());
    let names = crate::members::names::lookup(&mut *c, &subs).await?;
    let mut out = Vec::with_capacity(pending.len());
    for p in pending {
        let (ships, release_id) = ship_state(&mut *c, job, &p.ledger_key, rel).await?;
        out.push(GateChange {
            by_name: names.get(&p.by).cloned().unwrap_or_else(|| p.by.clone()),
            gate: p.gate,
            option: p.option,
            field: p.field,
            from: p.from,
            to: p.to,
            by: p.by,
            at: p.at,
            ships,
            release_id,
        });
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.at));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rule(v: Value) -> GateRule {
        serde_json::from_value(v).unwrap()
    }

    fn opts(keys: &[&str]) -> Vec<BaseOption> {
        keys.iter()
            .map(|k| BaseOption {
                key: (*k).into(),
                label: (*k).into(),
                description: String::new(),
                generated: false,
            })
            .collect()
    }

    #[test]
    fn rule_errors_as_gate_rules_ts() {
        let noul = opts(&["true", "false"]);
        let in_rule = rule(
            json!({"fire_when": "in", "threshold": null, "fire_values": ["true"], "min_confidence": null}),
        );
        assert_eq!(
            rule_error(GateType::Noul, &noul, &in_rule).as_deref(),
            Some("“in” only applies to choice questions.")
        );
        let choice = opts(&["a", "b"]);
        let bad = rule(
            json!({"fire_when": "in", "threshold": null, "fire_values": ["z"], "min_confidence": null}),
        );
        assert_eq!(
            rule_error(GateType::Choice, &choice, &bad).as_deref(),
            Some("Not an option: z.")
        );
        let score = opts(&["1", "2", "3", "4", "5"]);
        let gte = |t: f64| {
            rule(
                json!({"fire_when": "gte", "threshold": t, "fire_values": null, "min_confidence": null}),
            )
        };
        assert!(rule_error(GateType::Score, &score, &gte(4.0)).is_none());
        assert!(rule_error(GateType::Score, &score, &gte(6.0)).is_some());
        assert!(rule_error(GateType::Noul, &noul, &gte(1.5)).is_some());
        let mc = rule(
            json!({"fire_when": "gte", "threshold": 0.5, "fire_values": null, "min_confidence": 2}),
        );
        assert!(rule_error(GateType::Noul, &noul, &mc).is_some());
    }

    #[test]
    fn normalise_and_short() {
        let r = rule(
            json!({"fire_when": "in", "threshold": 0.4, "fire_values": ["issue", "need_change"], "min_confidence": 0.5}),
        );
        let n = normalise_rule(&r);
        assert_eq!(n.threshold, None);
        assert_eq!(rule_short(&n), "in issue|need_change @0.5");
        let g = rule(
            json!({"fire_when": "gte", "threshold": 4, "fire_values": ["x"], "min_confidence": null}),
        );
        let n = normalise_rule(&g);
        assert_eq!(n.fire_values, None);
        assert_eq!(rule_short(&n), "gte 4");
        assert_eq!(js_num(0.65), "0.65");
    }

    #[test]
    fn thresholds_and_tuning() {
        assert!(threshold_error(GateType::Score, 5, 3.0).is_none());
        assert!(threshold_error(GateType::Score, 5, 3.5).is_some());
        assert!(threshold_error(GateType::Choice, 4, 0.0).is_some());
        assert!(threshold_error(GateType::Choice, 4, 1.0).is_none());
        let r = rule(
            json!({"fire_when": "in", "threshold": null, "fire_values": ["a"], "min_confidence": 0.5}),
        );
        assert_eq!(tuned_rule(&r, Some(0.7)).min_confidence, Some(0.7));
        let g = rule(
            json!({"fire_when": "gte", "threshold": 0.6, "fire_values": null, "min_confidence": null}),
        );
        assert_eq!(tuned_rule(&g, Some(0.8)).threshold, Some(0.8));
        assert_eq!(tuned_rule(&g, None), g);
    }
}
