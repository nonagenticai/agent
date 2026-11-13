//! Classifier axes, profiles and axis conditions (configurator spec section 4.3). The condition JSON is the UI's
//! `AxisCondition` (`{"eq":["a","v"]}` …) and `axis = v` holds for descendants of `v` too (`provider.implicit`
//! satisfies `provider`), as the web `evaluate` does.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AxisError {
    #[error("unknown axis: {0}")]
    UnknownAxis(String),
    #[error("unknown value {value} on axis {axis}")]
    UnknownValue { axis: String, value: String },
    #[error("axis {axis} has cardinality one but the profile selects {count} values")]
    TooManyValues { axis: String, count: usize },
    #[error("axis {axis}: value {value}: {reason}")]
    BadHierarchy {
        axis: String,
        value: String,
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    One,
    Many,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxisValue {
    /// Full dotted path, e.g. `digital_wallet.mobile`.
    pub key: String,
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Axis {
    pub key: String,
    pub cardinality: Cardinality,
    pub values: Vec<AxisValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

fn find_axis<'a>(axes: &'a [Axis], key: &str) -> Result<&'a Axis, AxisError> {
    axes.iter()
        .find(|a| a.key == key)
        .ok_or_else(|| AxisError::UnknownAxis(key.to_owned()))
}

fn check_value(axis: &Axis, value: &str) -> Result<(), AxisError> {
    if axis.values.iter().any(|v| v.key == value) {
        Ok(())
    } else {
        Err(AxisError::UnknownValue {
            axis: axis.key.clone(),
            value: value.to_owned(),
        })
    }
}

/// `selected` is `wanted` or a descendant of it (dotted prefix on a segment boundary): `provider.implicit`
/// holds `provider`, `providers` does not.
pub fn holds(selected: &str, wanted: &str) -> bool {
    selected == wanted
        || selected
            .strip_prefix(wanted)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// Parent must exist in the same axis, key must equal `parent + "." + leaf`, no cycles.
pub fn validate_axis(axis: &Axis) -> Result<(), AxisError> {
    let bad = |value: &str, reason: &str| AxisError::BadHierarchy {
        axis: axis.key.clone(),
        value: value.to_owned(),
        reason: reason.to_owned(),
    };
    let mut seen = BTreeSet::new();
    for v in &axis.values {
        if !seen.insert(v.key.as_str()) {
            return Err(bad(&v.key, "duplicate value key"));
        }
    }
    let parents: BTreeMap<&str, Option<&str>> = axis
        .values
        .iter()
        .map(|v| (v.key.as_str(), v.parent.as_deref()))
        .collect();
    for v in &axis.values {
        match v.parent.as_deref() {
            None => {
                if v.key.is_empty() || v.key.contains('.') {
                    return Err(bad(
                        &v.key,
                        "root value key must be a single non-empty segment",
                    ));
                }
            }
            Some(parent) => {
                if !parents.contains_key(parent) {
                    return Err(bad(&v.key, &format!("parent {parent} not in axis")));
                }
                let leaf = v
                    .key
                    .strip_prefix(parent)
                    .and_then(|rest| rest.strip_prefix('.'));
                match leaf {
                    Some(l) if !l.is_empty() && !l.contains('.') => {}
                    _ => return Err(bad(&v.key, "key must equal parent + \".\" + leaf")),
                }
            }
        }
        // Cycle check: walk up the parent chain.
        let mut visited = BTreeSet::new();
        let mut cur = Some(v.key.as_str());
        while let Some(k) = cur {
            if !visited.insert(k) {
                return Err(bad(&v.key, "cycle in parent chain"));
            }
            cur = parents.get(k).copied().flatten();
        }
    }
    Ok(())
}

/// A business-model selection: axis → selected values (the UI's `Record<axis, values[]>`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Profile(pub BTreeMap<String, BTreeSet<String>>);

impl Profile {
    pub fn validate(&self, axes: &[Axis]) -> Result<(), AxisError> {
        for (axis_key, values) in &self.0 {
            let axis = find_axis(axes, axis_key)?;
            for v in values {
                check_value(axis, v)?;
            }
            if axis.cardinality == Cardinality::One && values.len() > 1 {
                return Err(AxisError::TooManyValues {
                    axis: axis_key.clone(),
                    count: values.len(),
                });
            }
        }
        Ok(())
    }

    /// `axis = wanted` holds: some selected value of the axis is `wanted` or a descendant of it.
    pub fn holds(&self, axis: &str, wanted: &str) -> bool {
        self.0
            .get(axis)
            .is_some_and(|sel| sel.iter().any(|s| holds(s, wanted)))
    }
}

/// Boolean language over axis values. JSON: `{"eq":["a","v"]}`, `{"in":["a",["v"]]}`,
/// `{"and":[..]}`, `{"or":[..]}`, `{"not":{..}}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Condition {
    /// `(axis, value)`
    Eq(String, String),
    /// `(axis, values)`
    In(String, Vec<String>),
    And(Vec<Condition>),
    Or(Vec<Condition>),
    Not(Box<Condition>),
}

impl Condition {
    pub fn validate(&self, axes: &[Axis]) -> Result<(), AxisError> {
        match self {
            Condition::Eq(axis, value) => check_value(find_axis(axes, axis)?, value),
            Condition::In(axis, values) => {
                let a = find_axis(axes, axis)?;
                values.iter().try_for_each(|v| check_value(a, v))
            }
            Condition::And(cs) | Condition::Or(cs) => cs.iter().try_for_each(|c| c.validate(axes)),
            Condition::Not(c) => c.validate(axes),
        }
    }

    pub fn eval(&self, profile: &Profile) -> bool {
        match self {
            Condition::Eq(axis, value) => profile.holds(axis, value),
            Condition::In(axis, values) => values.iter().any(|v| profile.holds(axis, v)),
            Condition::And(cs) => cs.iter().all(|c| c.eval(profile)),
            Condition::Or(cs) => cs.iter().any(|c| c.eval(profile)),
            Condition::Not(c) => !c.eval(profile),
        }
    }

    /// Human form, as the web `describe`: `order_type = continuous ∧ payment_type = balancing`.
    pub fn describe(&self) -> String {
        fn all(cs: &[Condition], sep: &str) -> String {
            cs.iter()
                .map(Condition::describe)
                .collect::<Vec<_>>()
                .join(sep)
        }
        match self {
            Condition::Eq(axis, value) => format!("{axis} = {value}"),
            Condition::In(axis, values) => format!("{axis} ∈ {{{}}}", values.join(", ")),
            Condition::And(cs) => all(cs, " ∧ "),
            Condition::Or(cs) => format!("({})", all(cs, " ∨ ")),
            Condition::Not(c) => format!("¬({})", c.describe()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn val(key: &str, parent: Option<&str>) -> AxisValue {
        AxisValue {
            key: key.to_owned(),
            parent: parent.map(str::to_owned),
            label: None,
            description: None,
        }
    }

    fn axes() -> Vec<Axis> {
        vec![
            Axis {
                key: "payment_method".into(),
                cardinality: Cardinality::Many,
                values: vec![
                    val("digital", None),
                    val("digital_wallet", None),
                    val("digital_wallet.mobile", Some("digital_wallet")),
                    val("card", None),
                ],
                label: None,
                description: None,
            },
            Axis {
                key: "order_type".into(),
                cardinality: Cardinality::One,
                values: vec![val("continuous", None), val("one_off", None)],
                label: None,
                description: None,
            },
            Axis {
                key: "payment_type".into(),
                cardinality: Cardinality::Many,
                values: vec![val("balancing", None), val("prepaid", None)],
                label: None,
                description: None,
            },
        ]
    }

    fn profile(pairs: &[(&str, &[&str])]) -> Profile {
        Profile(
            pairs
                .iter()
                .map(|(a, vs)| {
                    (
                        (*a).to_owned(),
                        vs.iter().map(|v| (*v).to_owned()).collect(),
                    )
                })
                .collect(),
        )
    }

    fn eq(a: &str, v: &str) -> Condition {
        Condition::Eq(a.into(), v.into())
    }

    #[test]
    fn holds_descendant_values() {
        assert!(holds("provider.implicit", "provider"));
        assert!(holds("provider", "provider"));
        assert!(!holds("provider", "provider.implicit"));
        assert!(!holds("providers", "provider"));
        assert!(!holds("provider_x", "provider"));
        let p = profile(&[("renewal", &["provider.implicit"])]);
        assert!(p.holds("renewal", "provider"));
        assert!(!p.holds("other", "provider"));
        assert!(eq("renewal", "provider").eval(&p));
    }

    #[test]
    fn eq_matches_descendant() {
        let p = profile(&[("payment_method", &["digital_wallet.mobile"])]);
        assert!(eq("payment_method", "digital_wallet").eval(&p));
        assert!(eq("payment_method", "digital_wallet.mobile").eval(&p));
        assert!(!eq("payment_method", "digital").eval(&p));
        let p2 = profile(&[("payment_method", &["digital_wallet"])]);
        assert!(!eq("payment_method", "digital").eval(&p2));
        assert!(!eq("payment_method", "digital_wallet.mobile").eval(&p2));
    }

    #[test]
    fn in_any() {
        let p = profile(&[("payment_method", &["digital_wallet.mobile"])]);
        let c = Condition::In(
            "payment_method".into(),
            vec!["card".into(), "digital_wallet".into()],
        );
        assert!(c.eval(&p));
        let none = Condition::In(
            "payment_method".into(),
            vec!["card".into(), "digital".into()],
        );
        assert!(!none.eval(&p));
    }

    #[test]
    fn and_or_not() {
        let p = profile(&[
            ("order_type", &["continuous"]),
            ("payment_type", &["balancing"]),
        ]);
        let a = eq("order_type", "continuous");
        let b = eq("payment_type", "prepaid");
        assert!(!Condition::And(vec![a.clone(), b.clone()]).eval(&p));
        assert!(Condition::Or(vec![a.clone(), b.clone()]).eval(&p));
        assert!(Condition::Not(Box::new(b)).eval(&p));
        assert!(!Condition::Not(Box::new(a)).eval(&p));
        assert!(Condition::And(vec![]).eval(&p));
        assert!(!Condition::Or(vec![]).eval(&p));
    }

    #[test]
    fn unknown_axis_rejected_on_validate() {
        let ax = axes();
        assert!(eq("order_type", "continuous").validate(&ax).is_ok());
        assert_eq!(
            eq("nope", "x").validate(&ax),
            Err(AxisError::UnknownAxis("nope".into()))
        );
        assert_eq!(
            eq("order_type", "nope").validate(&ax),
            Err(AxisError::UnknownValue {
                axis: "order_type".into(),
                value: "nope".into()
            })
        );
        let nested = Condition::Not(Box::new(Condition::In(
            "order_type".into(),
            vec!["bad".into()],
        )));
        assert!(matches!(
            nested.validate(&ax),
            Err(AxisError::UnknownValue { .. })
        ));
        assert!(profile(&[("nope", &["x"])]).validate(&ax).is_err());
        assert!(profile(&[("order_type", &["bad"])]).validate(&ax).is_err());
    }

    #[test]
    fn one_axis_two_values_rejected() {
        let ax = axes();
        let p = profile(&[("order_type", &["continuous", "one_off"])]);
        assert_eq!(
            p.validate(&ax),
            Err(AxisError::TooManyValues {
                axis: "order_type".into(),
                count: 2
            })
        );
        assert!(
            profile(&[("payment_type", &["balancing", "prepaid"])])
                .validate(&ax)
                .is_ok()
        );
    }

    #[test]
    fn json_round_trip() {
        let c = Condition::And(vec![
            eq("order_type", "continuous"),
            Condition::Not(Box::new(Condition::In(
                "payment_type".into(),
                vec!["balancing".into()],
            ))),
            Condition::Or(vec![eq("a", "b")]),
        ]);
        let s = serde_json::to_string(&c).unwrap();
        assert_eq!(
            s,
            r#"{"and":[{"eq":["order_type","continuous"]},{"not":{"in":["payment_type",["balancing"]]}},{"or":[{"eq":["a","b"]}]}]}"#
        );
        let back: Condition = serde_json::from_str(&s).unwrap();
        assert_eq!(back, c);
        assert_eq!(
            serde_json::to_string(&eq("order_type", "continuous")).unwrap(),
            r#"{"eq":["order_type","continuous"]}"#
        );
    }

    #[test]
    fn describe_matches_web() {
        let c = Condition::And(vec![
            eq("order_type", "continuous"),
            eq("payment_type", "balancing"),
        ]);
        assert_eq!(
            c.describe(),
            "order_type = continuous ∧ payment_type = balancing"
        );
        let i = Condition::In(
            "product_type".into(),
            vec!["virtual".into(), "access".into()],
        );
        assert_eq!(i.describe(), "product_type ∈ {virtual, access}");
        let o = Condition::Not(Box::new(Condition::Or(vec![eq("a", "b"), eq("c", "d")])));
        assert_eq!(o.describe(), "¬((a = b ∨ c = d))");
    }

    #[test]
    fn axis_parent_must_prefix_key() {
        let ok = &axes()[0];
        assert!(validate_axis(ok).is_ok());
        let mk = |values| Axis {
            key: "x".into(),
            cardinality: Cardinality::Many,
            values,
            label: None,
            description: None,
        };
        // key does not carry parent prefix
        assert!(validate_axis(&mk(vec![val("a", None), val("b.c", Some("a"))])).is_err());
        // missing parent
        assert!(validate_axis(&mk(vec![val("a.b", Some("a"))])).is_err());
        // cycle (also fails prefix rule, must still be an error)
        assert!(validate_axis(&mk(vec![val("a", Some("b")), val("b", Some("a"))])).is_err());
        // duplicate
        assert!(validate_axis(&mk(vec![val("a", None), val("a", None)])).is_err());
    }
}
