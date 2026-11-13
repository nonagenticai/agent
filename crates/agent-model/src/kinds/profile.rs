//! `profile` rows (P12 business models: the web `BusinessProfile` minus `key`) and `axis_rule` rows (the rule
//! definition a template family carries into its tags; the rows it adds live in `profile/<rule>`).

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{FieldError, Refusal, done};
use crate::axis::{Axis, Condition, Profile};
use crate::layer::{LayerKey, Scope};

/// `profile` row fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileFields {
    pub label: String,
    #[serde(default)]
    pub industry: Option<String>,
    #[serde(default)]
    pub description: String,
    /// Axis → selected values.
    pub selection: BTreeMap<String, Vec<String>>,
    /// Extra notes shown with the profile (party roles, freemium offerings).
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

impl ProfileFields {
    /// The selection as an axis profile, for evaluating axis rules (`Condition::eval`).
    pub fn axis_profile(&self) -> Profile {
        Profile(
            self.selection
                .iter()
                .map(|(axis, values)| {
                    (
                        axis.clone(),
                        values.iter().cloned().collect::<BTreeSet<_>>(),
                    )
                })
                .collect(),
        )
    }
}

/// 422 `invalid_label` (blank label), 422 `empty_selection` (no axis value selected).
pub fn check_profile_shape(p: &ProfileFields) -> Result<(), Refusal> {
    if p.label.trim().is_empty() {
        return Err(Refusal::new(
            422,
            "invalid_label",
            "Give the business model a name.",
        ));
    }
    if p.selection.values().all(Vec::is_empty) {
        return Err(Refusal::new(
            422,
            "empty_selection",
            "Pick at least one axis value.",
        ));
    }
    Ok(())
}

/// Every selected axis and value exists: 422 `unknown_axis`, 422 `unknown_axis_value`. Cardinality is not
/// checked here (the fixture profiles hold two values on some `one` axes; `setAxisCardinality` owns that rule).
pub fn check_profile_axes(p: &ProfileFields, axes: &[Axis]) -> Result<(), Refusal> {
    // As the web: an axis with no selected value is dropped, not looked up.
    for (axis_key, values) in p.selection.iter().filter(|(_, v)| !v.is_empty()) {
        let Some(axis) = axes.iter().find(|a| &a.key == axis_key) else {
            return Err(Refusal::new(
                422,
                "unknown_axis",
                format!("No axis {axis_key}."),
            ));
        };
        if let Some(v) = values
            .iter()
            .find(|v| !axis.values.iter().any(|known| &known.key == *v))
        {
            return Err(Refusal::new(
                422,
                "unknown_axis_value",
                format!("{axis_key} has no value {v}."),
            ));
        }
    }
    Ok(())
}

pub(crate) fn check_profile(p: &ProfileFields) -> Result<(), Vec<FieldError>> {
    match check_profile_shape(p) {
        Ok(()) => Ok(()),
        Err(r) => Err(vec![FieldError::new(r.code, None, r.message)]),
    }
}

/// `axis_rule` row fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AxisRuleFields {
    pub priority: i64,
    pub condition: Condition,
    /// The profile layer that holds the rows the rule adds (`profile/<rule>`).
    pub layer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

pub(crate) fn check_axis_rule(r: &AxisRuleFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    match LayerKey::parse_label(&r.layer) {
        Ok(key) if key.scope == Scope::Profile => {}
        _ => errors.push(FieldError::new(
            "invalid_value",
            Some("layer"),
            format!("{:?} is not a profile layer (profile/<rule>)", r.layer),
        )),
    }
    done(errors)
}

#[cfg(test)]
mod tests {
    use super::super::{Kind, validate_fields};
    use super::*;
    use crate::axis::{AxisValue, Cardinality};
    use serde_json::json;

    fn retail() -> serde_json::Value {
        json!({
            "label": "Retail e-commerce",
            "industry": "retail",
            "description": "B2C webshop: discrete orders, delivered or picked up, paid once.",
            "selection": {
                "order_type": ["discrete"],
                "offering.availability": ["regular", "preorder"],
                "payment_method": ["card.debit"]
            },
            "notes": ["Household and virtual products are optional."],
            "updated_at": "2026-10-12T08:10:00Z",
            "updated_by": "u-owner"
        })
    }

    fn axes() -> Vec<Axis> {
        let value = |key: &str, parent: Option<&str>| AxisValue {
            key: key.to_owned(),
            parent: parent.map(str::to_owned),
            label: None,
            description: None,
        };
        let axis = |key: &str, cardinality, values| Axis {
            key: key.to_owned(),
            cardinality,
            values,
            label: None,
            description: None,
        };
        vec![
            axis(
                "order_type",
                Cardinality::One,
                vec![value("discrete", None)],
            ),
            axis(
                "offering.availability",
                Cardinality::One,
                vec![value("regular", None), value("preorder", None)],
            ),
            axis(
                "payment_method",
                Cardinality::Many,
                vec![value("card", None), value("card.debit", Some("card"))],
            ),
        ]
    }

    #[test]
    fn profile_rows() {
        assert_eq!(validate_fields(Kind::Profile, &retail()), Ok(()));
        let p: ProfileFields = serde_json::from_value(retail()).expect("profile");
        assert_eq!(check_profile_axes(&p, &axes()), Ok(()));
        assert!(p.axis_profile().holds("payment_method", "card"));

        let mut unknown = p.clone();
        unknown.selection.insert("nope".into(), vec!["x".into()]);
        assert_eq!(
            check_profile_axes(&unknown, &axes()).map_err(|r| r.code),
            Err("unknown_axis")
        );
        let mut bad_value = p.clone();
        bad_value
            .selection
            .insert("order_type".into(), vec!["recurring".into()]);
        assert_eq!(
            check_profile_axes(&bad_value, &axes()).map_err(|r| r.code),
            Err("unknown_axis_value")
        );
        let mut empty_unknown = p.clone();
        empty_unknown.selection.insert("nope".into(), vec![]);
        assert_eq!(check_profile_axes(&empty_unknown, &axes()), Ok(()));
        assert_eq!(
            check_profile_shape(&ProfileFields {
                label: " ".into(),
                ..p.clone()
            })
            .err()
            .map(|r| r.message),
            Some("Give the business model a name.".to_owned())
        );
        let mut empty = retail();
        empty["selection"] = json!({"order_type": []});
        let codes: Vec<&str> = validate_fields(Kind::Profile, &empty)
            .err()
            .unwrap_or_default()
            .iter()
            .map(|e| e.code)
            .collect();
        assert_eq!(codes, vec!["empty_selection"]);
        let mut blank = retail();
        blank["label"] = json!(" ");
        assert_eq!(
            validate_fields(Kind::Profile, &blank)
                .err()
                .map(|e| e[0].code),
            Some("invalid_label")
        );
    }

    #[test]
    fn axis_rule_rows() {
        let rule = json!({
            "priority": 100,
            "condition": {"and": [{"eq": ["order_type", "continuous"]}, {"eq": ["payment_type", "balancing"]}]},
            "layer": "profile/metered_supply"
        });
        assert_eq!(validate_fields(Kind::AxisRule, &rule), Ok(()));
        let mut wrong_layer = rule.clone();
        wrong_layer["layer"] = json!("industry/retail");
        assert!(validate_fields(Kind::AxisRule, &wrong_layer).is_err());
        let mut bad_condition = rule.clone();
        bad_condition["condition"] = json!({"xor": []});
        assert_eq!(
            validate_fields(Kind::AxisRule, &bad_condition)
                .err()
                .map(|e| e[0].code),
            Some("invalid_value")
        );
    }
}
