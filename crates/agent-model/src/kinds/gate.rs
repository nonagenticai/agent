//! `gate_question` rows (P14 gate library, 12 §8.2): the web `GateQuestion` minus `key` and `layer` (row key and
//! row layer), options folded in (B6-2). The rule checks are ports of `web/src/lib/ontology/gateRules.ts`
//! (`ruleError`, `normaliseRule`) and of `checkThreshold` / `tunedRule` and the edit refusals in
//! `web/src/lib/api/areas/ontology.ts`, with the same codes and messages.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{FieldError, Refusal, done};

/// Where in a turn the runtime asks: P1 the request, P2 before a tool call, R1 the draft response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum GatePoint {
    P1,
    P2,
    R1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateType {
    Noul,
    Choice,
    Score,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateFail {
    Open,
    Closed,
}

/// Library grouping; `tenant` is a question a tenant added to one job (never in the platform library).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateGroup {
    Base,
    Safety,
    Post,
    Billing,
    Itsd,
    Utilities,
    Tenant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GateAction {
    Handover,
    Block,
    Annotate,
}

/// `gte` / `lt` compare the answer's value with `threshold`; `in` fires on one of `fire_values`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FireWhen {
    Gte,
    Lt,
    In,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRule {
    pub fire_when: FireWhen,
    /// gte / lt: the line (0–1 probability, or a score level). null with `in`.
    #[serde(default)]
    pub threshold: Option<f64>,
    /// in: the option keys that fire. null with gte / lt.
    #[serde(default)]
    pub fire_values: Option<Vec<String>>,
    /// Below this confidence the answer is unsure and the fail policy applies. null = any confidence.
    #[serde(default)]
    pub min_confidence: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateOption {
    pub key: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    /// Produced at the release cut from the job's effective grid; read-only.
    #[serde(default)]
    pub generated: bool,
}

/// `gate_question` row fields.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateQuestionFields {
    pub group: GateGroup,
    pub point: GatePoint,
    #[serde(rename = "type")]
    pub kind: GateType,
    pub question: String,
    #[serde(default)]
    pub options: Vec<GateOption>,
    /// Where the generated options come from, shown instead of an editor.
    #[serde(default)]
    pub generated_from: Option<String>,
    /// Threshold as written (`top ≥ 0.6`, `any true`).
    #[serde(default)]
    pub threshold: String,
    /// Numeric part a tenant may tune; null = not tunable.
    #[serde(default)]
    pub threshold_value: Option<f64>,
    pub fail: GateFail,
    /// Safety gate: fail closed, never hideable (D29).
    #[serde(default)]
    pub safety: bool,
    #[serde(default)]
    pub effect: String,
    pub rule: GateRule,
    pub action: GateAction,
    /// Tool point only: the tools (`object.verb`) it is asked for; null / empty = every tool.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    /// F-16: the platform default text the customer reads when the question hands over or blocks (1–500
    /// characters; B5 T8a reads it); absent = the care-bus default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_reply: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// A choice takes at most this many options.
pub const MAX_OPTIONS: usize = 255;

/// A customer reply is at most this many characters (B5 0146 `customer_reply` CHECK).
pub const MAX_CUSTOMER_REPLY: usize = 500;

/// F-16: a customer reply is 1–500 characters and not blank; 422 `invalid_customer_reply`.
pub fn check_customer_reply(text: &str) -> Result<(), Refusal> {
    let n = text.chars().count();
    if text.trim().is_empty() || n > MAX_CUSTOMER_REPLY {
        return Err(Refusal::new(
            422,
            "invalid_customer_reply",
            format!("A customer reply is 1 to {MAX_CUSTOMER_REPLY} characters."),
        ));
    }
    Ok(())
}

fn is_probability(v: f64) -> bool {
    v.is_finite() && (0.0..=1.0).contains(&v)
}

fn is_integer(v: f64) -> bool {
    v.is_finite() && v.fract().abs() < f64::EPSILON
}

/// Why `rule` cannot be saved for a question of `kind` with `options`, or `None` (web `ruleError`).
pub fn rule_error(kind: GateType, options: &[GateOption], rule: &GateRule) -> Option<String> {
    if let Some(m) = rule.min_confidence
        && !is_probability(m)
    {
        return Some("The minimum confidence is between 0 and 1 (blank: any).".to_owned());
    }
    if rule.fire_when == FireWhen::In {
        if kind != GateType::Choice {
            return Some("“in” only applies to choice questions.".to_owned());
        }
        let values = rule.fire_values.as_deref().unwrap_or_default();
        if values.is_empty() {
            return Some("Pick at least one answer that fires.".to_owned());
        }
        let keys: BTreeSet<&str> = options.iter().map(|o| o.key.as_str()).collect();
        let unknown: Vec<&str> = values
            .iter()
            .map(String::as_str)
            .filter(|v| !keys.contains(v))
            .collect();
        if !keys.is_empty() && !unknown.is_empty() {
            return Some(format!("Not an option: {}.", unknown.join(", ")));
        }
        return None;
    }
    let Some(threshold) = rule.threshold.filter(|t| t.is_finite()) else {
        return Some("Set a threshold.".to_owned());
    };
    if kind == GateType::Score {
        let n = options.len();
        let max = n as f64;
        if !is_integer(threshold) || !(1.0..=max).contains(&threshold) {
            return Some(format!("A score threshold is a level from 1 to {n}."));
        }
    } else if !is_probability(threshold) {
        return Some("The threshold is a probability between 0 and 1.".to_owned());
    }
    None
}

/// The rule as it would be saved: `in` keeps fire values and drops the threshold, gte / lt the reverse; a
/// non-finite number becomes null (web `normaliseRule`).
pub fn normalise_rule(rule: &GateRule) -> GateRule {
    let is_in = rule.fire_when == FireWhen::In;
    let finite = |v: Option<f64>| v.filter(|x| x.is_finite());
    GateRule {
        fire_when: rule.fire_when,
        threshold: if is_in { None } else { finite(rule.threshold) },
        fire_values: if is_in {
            Some(rule.fire_values.clone().unwrap_or_default())
        } else {
            None
        },
        min_confidence: finite(rule.min_confidence),
    }
}

/// `normalise_rule` then `rule_error`: 422 `invalid_rule` with the web message.
pub fn check_rule(
    kind: GateType,
    options: &[GateOption],
    rule: &GateRule,
) -> Result<GateRule, Refusal> {
    let next = normalise_rule(rule);
    match rule_error(kind, options, &next) {
        Some(message) => Err(Refusal::new(422, "invalid_rule", message)),
        None => Ok(next),
    }
}

/// The template rule with a tuned threshold in place: gte / lt move the line, `in` the minimum confidence
/// (web `tunedRule`).
pub fn tuned_rule(rule: &GateRule, tuned: Option<f64>) -> GateRule {
    let mut out = rule.clone();
    match tuned {
        Some(v) if rule.fire_when == FireWhen::In => out.min_confidence = Some(v),
        Some(v) => out.threshold = Some(v),
        None => {}
    }
    out
}

/// A tuned threshold value (web `checkThreshold`): a score level 1..=options, else a probability in (0, 1].
/// 422 `invalid_threshold`.
pub fn check_threshold(kind: GateType, option_count: usize, value: f64) -> Result<(), Refusal> {
    if kind == GateType::Score {
        let max = option_count as f64;
        if !is_integer(value) || !(1.0..=max).contains(&value) {
            return Err(Refusal::new(
                422,
                "invalid_threshold",
                format!("A score threshold is a level from 1 to {option_count}."),
            ));
        }
        return Ok(());
    }
    let in_range = value > 0.0 && value <= 1.0;
    if in_range {
        Ok(())
    } else {
        Err(Refusal::new(
            422,
            "invalid_threshold",
            "A choice threshold is between 0 and 1.",
        ))
    }
}

/// A safety gate, or one without a numeric threshold, has nothing to tune: 409 `threshold_not_tunable`.
pub fn check_tunable(key: &str, q: &GateQuestionFields) -> Result<(), Refusal> {
    if q.safety || q.threshold_value.is_none() {
        return Err(Refusal::new(
            409,
            "threshold_not_tunable",
            format!("{key} acts on any true; it has no threshold."),
        ));
    }
    Ok(())
}

/// A safety gate's rule is fixed: 409 `safety_gate`.
pub fn check_rule_editable(key: &str, q: &GateQuestionFields) -> Result<(), Refusal> {
    if q.safety {
        return Err(Refusal::new(
            409,
            "safety_gate",
            format!("{key} is a safety gate; its rule stays fail closed on any true (D29)."),
        ));
    }
    Ok(())
}

/// Question text, trimmed; fewer than 8 characters is 422 `invalid_question`.
pub fn check_question_text(text: &str) -> Result<String, Refusal> {
    let text = text.trim();
    if text.encode_utf16().count() < 8 {
        return Err(Refusal::new(
            422,
            "invalid_question",
            "Write the question in full.",
        ));
    }
    Ok(text.to_owned())
}

/// The whole stored question: text, rule, tunable threshold, option count and keys, tools only at P2.
pub(crate) fn check(q: &GateQuestionFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    if let Err(r) = check_question_text(&q.question) {
        errors.push(FieldError::new(r.code, Some("question"), r.message));
    }
    if let Some(message) = rule_error(q.kind, &q.options, &q.rule) {
        errors.push(FieldError::new("invalid_rule", Some("rule"), message));
    } else if q.rule != normalise_rule(&q.rule) {
        // CB 0016: a threshold only without `in`, fire values only with `in`.
        errors.push(FieldError::new(
            "invalid_rule",
            Some("rule"),
            "`in` takes fire values and no threshold; gte / lt take a threshold and no fire values.",
        ));
    }
    if let Some(v) = q.threshold_value
        && let Err(r) = check_threshold(q.kind, q.options.len(), v)
    {
        errors.push(FieldError::new(r.code, Some("threshold_value"), r.message));
    }
    if q.options.len() > MAX_OPTIONS {
        errors.push(FieldError::new(
            "too_many_options",
            Some("options"),
            "A choice takes at most 255 options.",
        ));
    }
    let mut keys = BTreeSet::new();
    for (i, o) in q.options.iter().enumerate() {
        if o.key.trim().is_empty() || !keys.insert(o.key.as_str()) {
            errors.push(FieldError::at(
                "invalid_value",
                format!("options[{i}].key"),
                format!("option key {:?} is empty or used twice", o.key),
            ));
        }
    }
    if let Some(reply) = &q.customer_reply
        && let Err(r) = check_customer_reply(reply)
    {
        errors.push(FieldError::new(r.code, Some("customer_reply"), r.message));
    }
    let has_tools = q.tools.as_ref().is_some_and(|t| !t.is_empty());
    if has_tools && q.point != GatePoint::P2 {
        errors.push(FieldError::new(
            "invalid_value",
            Some("tools"),
            "only a tool-point (P2) question names tools",
        ));
    }
    done(errors)
}

#[cfg(test)]
mod tests {
    use super::super::{Kind, validate_fields};
    use super::*;
    use serde_json::json;

    fn rule(fire_when: FireWhen) -> GateRule {
        GateRule {
            fire_when,
            threshold: None,
            fire_values: None,
            min_confidence: None,
        }
    }

    fn with_threshold(t: f64) -> GateRule {
        GateRule {
            threshold: Some(t),
            ..rule(FireWhen::Gte)
        }
    }

    fn with_values(values: &[&str]) -> GateRule {
        GateRule {
            fire_values: Some(values.iter().map(|v| (*v).to_owned()).collect()),
            ..rule(FireWhen::In)
        }
    }

    fn option(key: &str) -> GateOption {
        GateOption {
            key: key.to_owned(),
            label: key.to_uppercase(),
            description: String::new(),
            generated: false,
        }
    }

    /// Ten cases copied from the web expectations: `tests/unit/ontology-J7-gates.test.ts` "ruleError +
    /// ruleSummary" (1–6), `tests/unit/ontology-api.test.ts` `setGateThreshold('GS', 1.4 | 0.7)` (7, 8) and
    /// `updateGateQuestion('I2', …)` threshold_not_tunable (9), and the score branch of `ruleError` (10).
    #[test]
    fn gate_rule_errors_match_ts() {
        let choice = [option("a")];
        let five: Vec<GateOption> = (1..=5).map(|i| option(&i.to_string())).collect();
        // (case, Err(code) / Ok, message fragment the TS test matches)
        let cases: [(&str, Result<(), Refusal>, &str); 10] = [
            (
                "noul + in",
                check_rule(GateType::Noul, &choice, &with_values(&["a"])).map(|_| ()),
                "only",
            ),
            (
                "in without values",
                check_rule(GateType::Choice, &choice, &with_values(&[])).map(|_| ()),
                "at least one",
            ),
            (
                "in with an unknown option",
                check_rule(GateType::Choice, &choice, &with_values(&["zz"])).map(|_| ()),
                "Not an option: zz",
            ),
            (
                "choice threshold above 1",
                check_rule(GateType::Choice, &choice, &with_threshold(1.4)).map(|_| ()),
                "between 0 and 1",
            ),
            (
                "min confidence above 1",
                check_rule(
                    GateType::Choice,
                    &choice,
                    &GateRule {
                        min_confidence: Some(2.0),
                        ..with_threshold(0.6)
                    },
                )
                .map(|_| ()),
                "minimum confidence",
            ),
            (
                "valid gte",
                check_rule(GateType::Choice, &choice, &with_threshold(0.6)).map(|_| ()),
                "",
            ),
            (
                "tuned threshold 1.4",
                check_threshold(GateType::Choice, 9, 1.4),
                "between 0 and 1",
            ),
            (
                "tuned threshold 0.7",
                check_threshold(GateType::Choice, 9, 0.7),
                "",
            ),
            (
                "not tunable",
                check_tunable(
                    "I2",
                    &GateQuestionFields {
                        threshold_value: None,
                        ..question()
                    },
                ),
                "acts on any true",
            ),
            (
                "score level out of range",
                check_rule(GateType::Score, &five, &with_threshold(7.0)).map(|_| ()),
                "A score threshold is a level from 1 to 5.",
            ),
        ];
        let expected_codes = [
            Some("invalid_rule"),
            Some("invalid_rule"),
            Some("invalid_rule"),
            Some("invalid_rule"),
            Some("invalid_rule"),
            None,
            Some("invalid_threshold"),
            None,
            Some("threshold_not_tunable"),
            Some("invalid_rule"),
        ];
        for ((case, result, fragment), code) in cases.into_iter().zip(expected_codes) {
            match (result, code) {
                (Ok(()), None) => {}
                (Err(r), Some(code)) => {
                    assert_eq!(r.code, code, "{case}");
                    assert!(r.message.contains(fragment), "{case}: {}", r.message);
                }
                (other, code) => panic!("{case}: got {other:?}, expected {code:?}"),
            }
        }
        // The exact web strings.
        assert_eq!(
            rule_error(GateType::Choice, &choice, &rule(FireWhen::Lt)).as_deref(),
            Some("Set a threshold.")
        );
        assert_eq!(
            rule_error(GateType::Noul, &[], &with_values(&["a"])).as_deref(),
            Some("“in” only applies to choice questions.")
        );
        // No options declared (generated at the cut): any fire value passes.
        assert_eq!(
            rule_error(GateType::Choice, &[], &with_values(&["zz"])),
            None
        );
        assert_eq!(
            rule_error(GateType::Score, &five, &with_threshold(4.0)),
            None
        );
        assert!(rule_error(GateType::Score, &five, &with_threshold(2.5)).is_some());
    }

    #[test]
    fn normalise_and_tune() {
        let messy = GateRule {
            fire_when: FireWhen::In,
            threshold: Some(0.5),
            fire_values: None,
            min_confidence: Some(f64::NAN),
        };
        assert_eq!(
            normalise_rule(&messy),
            GateRule {
                fire_values: Some(vec![]),
                ..rule(FireWhen::In)
            }
        );
        let gte = GateRule {
            fire_values: Some(vec!["a".into()]),
            ..with_threshold(0.7)
        };
        assert_eq!(normalise_rule(&gte), with_threshold(0.7));
        assert_eq!(
            tuned_rule(&with_threshold(0.7), Some(0.9)),
            with_threshold(0.9)
        );
        assert_eq!(tuned_rule(&with_threshold(0.7), None), with_threshold(0.7));
        let tuned_in = tuned_rule(&with_values(&["a"]), Some(0.6));
        assert_eq!(
            tuned_in,
            GateRule {
                min_confidence: Some(0.6),
                ..with_values(&["a"])
            }
        );
        assert_eq!(check_threshold(GateType::Score, 5, 3.0), Ok(()));
        assert_eq!(
            check_threshold(GateType::Score, 5, 0.5).map_err(|r| r.code),
            Err("invalid_threshold")
        );
        assert_eq!(
            check_threshold(GateType::Noul, 2, 0.0).map_err(|r| r.code),
            Err("invalid_threshold")
        );
        let safety = GateQuestionFields {
            safety: true,
            threshold_value: Some(0.5),
            ..question()
        };
        assert_eq!(
            check_rule_editable("B1", &safety).map_err(|r| (r.status, r.code)),
            Err((409, "safety_gate"))
        );
        assert_eq!(
            check_tunable("B8", &safety).map_err(|r| r.code),
            Err("threshold_not_tunable")
        );
        assert_eq!(
            check_question_text("  short ").map_err(|r| r.code),
            Err("invalid_question")
        );
        assert_eq!(
            check_question_text(" Is the customer upset? ").as_deref(),
            Ok("Is the customer upset?")
        );
    }

    fn question() -> GateQuestionFields {
        serde_json::from_value(fixture_gt()).expect("GT parses")
    }

    /// `GT` from `web/src/lib/fixtures/ontology.ts` `gateLibrary()`, minus key and layer.
    fn fixture_gt() -> serde_json::Value {
        json!({
            "group": "base",
            "point": "P1",
            "type": "choice",
            "question": "Which theme is the customer in?",
            "options": [
                {"key": "need", "label": "Need", "description": "", "generated": false},
                {"key": "issue", "label": "Issue", "description": "", "generated": false}
            ],
            "generated_from": null,
            "threshold": "top ≥ 0.5",
            "threshold_value": 0.5,
            "fail": "open",
            "safety": false,
            "effect": "Tone; stage prior.",
            "rule": {"fire_when": "in", "threshold": null, "fire_values": ["issue"], "min_confidence": 0.5},
            "action": "annotate",
            "tools": null,
            "origin": "platform",
            "updated_by": null,
            "updated_at": "2026-10-12T09:00:00Z"
        })
    }

    #[test]
    fn gate_question_rows() {
        assert_eq!(validate_fields(Kind::GateQuestion, &fixture_gt()), Ok(()));
        let codes = |v: &serde_json::Value| -> Vec<&'static str> {
            validate_fields(Kind::GateQuestion, v)
                .err()
                .unwrap_or_default()
                .iter()
                .map(|e| e.code)
                .collect()
        };
        let mut bad_rule = fixture_gt();
        bad_rule["rule"]["fire_values"] = json!(["zz"]);
        assert_eq!(codes(&bad_rule), vec!["invalid_rule"]);
        let mut in_with_threshold = fixture_gt();
        in_with_threshold["rule"]["threshold"] = json!(0.5);
        assert_eq!(codes(&in_with_threshold), vec!["invalid_rule"]);
        let mut gte_with_values = fixture_gt();
        gte_with_values["rule"] = json!({"fire_when": "gte", "threshold": 0.5, "fire_values": ["issue"], "min_confidence": null});
        assert_eq!(codes(&gte_with_values), vec!["invalid_rule"]);
        let mut gte_ok = fixture_gt();
        gte_ok["rule"] = json!({"fire_when": "gte", "threshold": 0.5, "fire_values": null, "min_confidence": null});
        assert_eq!(codes(&gte_ok), Vec::<&str>::new());
        let mut bad_threshold = fixture_gt();
        bad_threshold["threshold_value"] = json!(1.5);
        assert_eq!(codes(&bad_threshold), vec!["invalid_threshold"]);
        let mut short = fixture_gt();
        short["question"] = json!("Why?");
        assert_eq!(codes(&short), vec!["invalid_question"]);
        let mut tools = fixture_gt();
        tools["tools"] = json!(["refund.issue"]);
        assert_eq!(codes(&tools), vec!["invalid_value"]);
        let mut keyed = fixture_gt();
        keyed["key"] = json!("GT");
        assert_eq!(codes(&keyed), vec!["unknown_field"]);
    }

    /// F-16: `customer_reply` round-trips through the row and is 1–500 characters.
    #[test]
    fn gate_question_customer_reply() {
        let mut row = fixture_gt();
        row["customer_reply"] = json!("A colleague will call you back today.");
        assert_eq!(validate_fields(Kind::GateQuestion, &row), Ok(()));
        let q: GateQuestionFields = serde_json::from_value(row.clone()).expect("parses");
        assert_eq!(
            q.customer_reply.as_deref(),
            Some("A colleague will call you back today.")
        );
        assert_eq!(
            serde_json::to_value(&q).expect("json")["customer_reply"],
            row["customer_reply"]
        );
        // absent stays absent (seeded rows keep their hash)
        let plain: GateQuestionFields = serde_json::from_value(fixture_gt()).expect("parses");
        assert!(
            serde_json::to_value(&plain)
                .expect("json")
                .get("customer_reply")
                .is_none()
        );
        for bad in [json!("   "), json!("x".repeat(501))] {
            let mut r = fixture_gt();
            r["customer_reply"] = bad;
            let codes: Vec<&str> = validate_fields(Kind::GateQuestion, &r)
                .err()
                .unwrap_or_default()
                .iter()
                .map(|e| e.code)
                .collect();
            assert_eq!(codes, vec!["invalid_customer_reply"]);
        }
    }
}
