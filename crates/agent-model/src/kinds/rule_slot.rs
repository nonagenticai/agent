//! `rule_slot` (0222): a business-rule slot a super_admin authors on a template layer (industry, department, base,
//! job; never a profile layer). The key is the rule id (`RET-01`); the fields mirror agent-api's `RuleSlot` without
//! the id: what the rule guards (`applies_to`), what it reads (`inputs`), its `outcome`, the one-sentence
//! `statement` (`{key}` = a value slot) and the `values` a tenant fills in. A tenant fills the values of a slot; it
//! may also author separate validated definitions through the job rule-authoring API. `slot_layer` defaults to the layer the row lives in. Rows fold as catalogue items (add
//! sets, patch replaces the whole slot, tombstone removes it) in the compile's layer order.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::{FieldError, done, require_text};

/// One value slot of a rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleValueFields {
    pub key: String,
    pub label: String,
    pub unit: String,
    #[serde(default)]
    pub hint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleUseFields {
    pub activity: String,
    pub activity_label: String,
    #[serde(default)]
    pub procedure: Option<String>,
    pub step: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleOutcomeFields {
    pub code: String,
    pub label: String,
    pub agent_does: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleFactFields {
    pub key: String,
    pub label: String,
    /// `date` | `text` | `choice` | `number` | `bool`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleExampleFields {
    pub label: String,
    pub facts: std::collections::BTreeMap<String, String>,
}

/// Fields of a `rule_slot` row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSlotFields {
    pub label: String,
    pub applies_to: Vec<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
    pub outcome: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statement: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_layer: Option<String>,
    #[serde(default)]
    pub values: Vec<RuleValueFields>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<RuleUseFields>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outcomes: Vec<RuleOutcomeFields>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<RuleFactFields>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<RuleExampleFields>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<crate::rules::RuleProgram>,
}

/// A rule id: an upper-case letter, then upper-case letters, digits or `-` (at most 32): `RET-01`, `DIS-02`.
pub(crate) fn is_rule_id(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next().is_some_and(|c| c.is_ascii_uppercase())
        && s.len() <= 32
        && cs.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-')
}

pub(crate) fn check(f: &RuleSlotFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&f.label, "label", &mut errors);
    require_text(&f.outcome, "outcome", &mut errors);
    if f.applies_to.is_empty() || f.applies_to.iter().any(|a| a.trim().is_empty()) {
        errors.push(FieldError::new(
            "invalid_value",
            Some("applies_to"),
            "must name at least one operation, none blank",
        ));
    }
    if f.statement.as_deref().is_some_and(|s| s.trim().is_empty()) {
        errors.push(FieldError::new(
            "invalid_value",
            Some("statement"),
            "must be a non-empty string when given",
        ));
    }
    let mut seen = BTreeSet::new();
    for (i, v) in f.values.iter().enumerate() {
        if !super::is_snake_name(&v.key) {
            errors.push(FieldError::at(
                "invalid_value",
                format!("values[{i}].key"),
                "must be a snake_case name",
            ));
        } else if !seen.insert(v.key.as_str()) {
            errors.push(FieldError::at(
                "invalid_value",
                format!("values[{i}].key"),
                "is used by an earlier value",
            ));
        }
        if v.label.trim().is_empty() {
            errors.push(FieldError::at(
                "invalid_value",
                format!("values[{i}].label"),
                "must be a non-empty string",
            ));
        }
        if v.unit.trim().is_empty() {
            errors.push(FieldError::at(
                "invalid_value",
                format!("values[{i}].unit"),
                "must be a non-empty string",
            ));
        }
    }
    if let Some(s) = &f.statement {
        // every `{key}` of the statement must be one of the values
        let mut rest = s.as_str();
        while let Some(open) = rest.find('{') {
            let Some(close) = rest[open..].find('}') else {
                errors.push(FieldError::new(
                    "invalid_value",
                    Some("statement"),
                    "has an unclosed `{`",
                ));
                break;
            };
            let key = &rest[open + 1..open + close];
            if !f.values.iter().any(|v| v.key == key) {
                errors.push(FieldError::new(
                    "invalid_value",
                    Some("statement"),
                    format!("names `{{{key}}}`, which is not one of the values"),
                ));
            }
            rest = &rest[open + close + 1..];
        }
    }
    if let Some(program) = &f.evaluation {
        let mut declared = BTreeSet::new();
        if f.facts.len() > 32 || f.values.len() > 32 {
            errors.push(FieldError::new(
                "invalid_value",
                Some("facts"),
                "at most 32 facts and 32 thresholds are allowed",
            ));
        }
        for fact in &f.facts {
            if fact.key.trim().is_empty()
                || fact.key.len() > 128
                || fact.label.trim().is_empty()
                || !declared.insert(&fact.key)
                || !["date", "text", "choice", "number", "bool"].contains(&fact.kind.as_str())
            {
                errors.push(FieldError::new(
                    "invalid_value",
                    Some("facts"),
                    "facts need unique non-empty keys (at most 128 bytes), labels and valid types",
                ));
            }
        }
        let facts = f
            .inputs
            .iter()
            .map(String::as_str)
            .chain(f.facts.iter().map(|f| f.key.as_str()))
            .collect();
        let values = f.values.iter().map(|v| v.key.as_str()).collect();
        if let Err(message) = program.validate(&facts, &values) {
            errors.push(FieldError::new(
                "invalid_value",
                Some("evaluation"),
                message,
            ));
        }
    }
    done(errors)
}

/// The row's one-line summary: its statement, else its outcome.
pub(crate) fn summary(f: &RuleSlotFields) -> String {
    f.statement.clone().unwrap_or_else(|| f.outcome.clone())
}

#[cfg(test)]
mod tests {
    use crate::kinds::{Kind, Scope, validate_fields, validate_key};
    use serde_json::json;

    fn ok() -> serde_json::Value {
        json!({
            "label": "Return window",
            "applies_to": ["order.return"],
            "inputs": ["order.delivered_at"],
            "outcome": "eligible | ineligible:RET-01",
            "statement": "Returns are accepted within {window_days} of delivery.",
            "values": [{"key": "window_days", "label": "Return window", "unit": "days", "hint": "Days."}]
        })
    }

    #[test]
    fn valid_slot_and_keys() {
        assert_eq!(validate_fields(Kind::RuleSlot, &ok()), Ok(()));
        assert_eq!(validate_key(Kind::RuleSlot, "RET-01"), Ok(()));
        for bad in ["ret-01", "1RET", "", "RET 01", "RET:01"] {
            assert!(validate_key(Kind::RuleSlot, bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn scopes() {
        for s in [
            Scope::Base,
            Scope::Industry,
            Scope::BaseDept,
            Scope::IndustryDept,
            Scope::Job,
        ] {
            assert!(Kind::RuleSlot.allowed_in(s), "{s}");
        }
        assert!(!Kind::RuleSlot.allowed_in(Scope::Profile));
    }

    #[test]
    fn refusals() {
        let mut v = ok();
        v["extra"] = json!(1);
        assert!(validate_fields(Kind::RuleSlot, &v).is_err());
        let mut v = ok();
        v["applies_to"] = json!([]);
        assert!(validate_fields(Kind::RuleSlot, &v).is_err());
        let mut v = ok();
        v["statement"] = json!("Within {nope}.");
        assert!(validate_fields(Kind::RuleSlot, &v).is_err());
        let mut v = ok();
        v["values"] = json!([
            {"key": "a", "label": "A", "unit": "days"},
            {"key": "a", "label": "B", "unit": "days"}
        ]);
        v["statement"] = json!("{a}");
        assert!(validate_fields(Kind::RuleSlot, &v).is_err());
        let mut v = ok();
        v["outcome"] = json!(" ");
        assert!(validate_fields(Kind::RuleSlot, &v).is_err());
    }
}
