//! Declarative business-rule conditions. Definitions are data, never executable tenant scripts.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

pub const MAX_CONDITION_DEPTH: usize = 8;
pub const MAX_CONDITION_NODES: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleProgram {
    pub condition: RuleCondition,
    pub eligible_reason: String,
    pub ineligible_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuleCondition {
    All {
        conditions: Vec<RuleCondition>,
    },
    Any {
        conditions: Vec<RuleCondition>,
    },
    Compare {
        left: RuleOperand,
        op: RuleComparison,
        right: RuleOperand,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuleOperand {
    Fact { key: String },
    Value { key: String },
    Literal { value: String },
    DaysSince { key: String },
    MonthsSince { key: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleComparison {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    In,
    NotIn,
}

impl RuleComparison {
    pub fn numeric(self) -> bool {
        matches!(self, Self::Lt | Self::Lte | Self::Gt | Self::Gte)
    }
}

impl RuleProgram {
    /// Check references against declared facts/thresholds and bound the evaluation cost before storing a definition.
    pub fn validate(&self, facts: &BTreeSet<&str>, values: &BTreeSet<&str>) -> Result<(), String> {
        for reason in [&self.eligible_reason, &self.ineligible_reason] {
            if reason.trim().is_empty() || reason.chars().count() > 1000 {
                return Err("Both outcome reasons must contain 1–1000 characters.".into());
            }
        }
        let mut nodes = 0;
        self.condition.validate(facts, values, 0, &mut nodes)
    }
}

impl RuleCondition {
    fn validate(
        &self,
        facts: &BTreeSet<&str>,
        values: &BTreeSet<&str>,
        depth: usize,
        nodes: &mut usize,
    ) -> Result<(), String> {
        *nodes += 1;
        if depth > MAX_CONDITION_DEPTH || *nodes > MAX_CONDITION_NODES {
            return Err("A rule may contain at most 64 conditions and eight nested groups.".into());
        }
        match self {
            Self::All { conditions } | Self::Any { conditions } => {
                if conditions.is_empty() {
                    return Err("A condition group cannot be empty.".into());
                }
                for c in conditions {
                    c.validate(facts, values, depth + 1, nodes)?;
                }
            }
            Self::Compare { left, op, right } => {
                for operand in [left, right] {
                    match operand {
                        RuleOperand::Fact { key }
                        | RuleOperand::DaysSince { key }
                        | RuleOperand::MonthsSince { key } => {
                            if !facts.contains(key.as_str()) {
                                return Err(format!("Undeclared fact {key}."));
                            }
                        }
                        RuleOperand::Value { key } => {
                            if !values.contains(key.as_str()) {
                                return Err(format!("Undeclared threshold {key}."));
                            }
                        }
                        RuleOperand::Literal { value } => {
                            if value.chars().count() > 280 {
                                return Err("A literal is limited to 280 characters.".into());
                            }
                            if op.numeric()
                                && value
                                    .trim()
                                    .parse::<f64>()
                                    .ok()
                                    .is_none_or(|n| !n.is_finite())
                            {
                                return Err(
                                    "Numeric comparisons require finite numeric literals.".into()
                                );
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn validates_references_cost_and_finite_literals() {
        let mut p: RuleProgram = serde_json::from_value(json!({"condition": {"kind":"compare","left":{"kind":"fact","key":"order.total"},"op":"lte","right":{"kind":"value","key":"limit"}},"eligible_reason":"Within policy.","ineligible_reason":"Above policy."})).unwrap();
        let facts = ["order.total"].into_iter().collect();
        let values = ["limit"].into_iter().collect();
        assert!(p.validate(&facts, &values).is_ok());
        assert!(p.validate(&BTreeSet::new(), &values).is_err());
        assert!(p.validate(&facts, &BTreeSet::new()).is_err());
        let comparison = p.condition.clone();
        p.condition = RuleCondition::Any { conditions: vec![] };
        assert!(p.validate(&facts, &values).is_err());
        p.condition = RuleCondition::All {
            conditions: vec![comparison; 65],
        };
        assert!(p.validate(&facts, &values).is_err());
        p.condition = serde_json::from_value(json!({"kind":"compare","left":{"kind":"fact","key":"order.total"},"op":"lte","right":{"kind":"literal","value":"NaN"}})).unwrap();
        assert!(p.validate(&facts, &values).is_err());
    }
}
