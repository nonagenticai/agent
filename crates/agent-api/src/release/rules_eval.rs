//! The built-in `rule.evaluate` (K49), a pure function: a business rule's facts + the tenant's values → outcome.
//! A port of `web/src/lib/release/J9-evaluate.ts` `evaluateJ9` with the same reasons and traces (spec B2 §2.7, the
//! hook B4's built-in op calls). The J9 dry run (`POST /api/jobs/{job}/rules/{id}/evaluate`) answers with it and
//! stores nothing. A rule with an unset value cannot be evaluated: the agent escalates instead (10 §5.2 step 7,
//! "placeholder").
//!
//! Numbers follow JavaScript: a value or fact is read with `Number(..)` semantics ([`js_number`]) and printed as
//! JavaScript prints it ([`js_num`]), so a value the save check never saw (a dry run's override) gives the fixture's
//! text too.
use std::collections::BTreeMap;

use agent_model::rules::{
    MAX_CONDITION_DEPTH, MAX_CONDITION_NODES, RuleComparison, RuleCondition, RuleOperand,
    RuleProgram,
};
use chrono::{Datelike, NaiveDate};
use serde::Serialize;
use serde_json::{Map, Value};

/// A dry-run or runtime evaluation (fixture `RuleEvaluation`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuleEvaluation {
    pub rule: String,
    /// `eligible` | `ineligible` | `escalate` | `placeholder`.
    pub outcome: &'static str,
    /// `eligible`, `ineligible:<id>`, `escalate`, `placeholder`.
    pub code: String,
    pub reason: String,
    /// Each comparison the rule made, in order.
    pub trace: Vec<String>,
    pub values_used: Map<String, Value>,
}

/// JavaScript `String#trim`: white space (Rust's `char::is_whitespace`) and U+FEFF, which JavaScript also strips.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{FEFF}')
}

/// JavaScript `Number(s)` for a string: surrounding white space ignored, empty → 0, `0x`/`0o`/`0b` integers,
/// `Infinity`, else a decimal literal; anything else is NaN.
pub fn js_number(s: &str) -> f64 {
    let t = js_trim(s);
    if t.is_empty() {
        return 0.0;
    }
    let radix = |p: &str, r: u32| {
        let digits = &t[p.len()..];
        // `u128::from_str_radix` takes a leading `+`; JavaScript's `Number("0x+1")` is NaN.
        if digits.is_empty() || digits.starts_with('+') {
            return f64::NAN;
        }
        u128::from_str_radix(digits, r).map_or(f64::NAN, |n| n as f64)
    };
    for (p, r) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if t.starts_with(p) {
            return radix(p, r);
        }
    }
    match t {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    // Rust also reads `inf` / `nan` and friends, JavaScript does not: only digits, a sign, a dot and an exponent.
    if !t
        .chars()
        .all(|ch| ch.is_ascii_digit() || matches!(ch, '+' | '-' | '.' | 'e' | 'E'))
    {
        return f64::NAN;
    }
    t.parse::<f64>().unwrap_or(f64::NAN)
}

/// A number as a JavaScript template string prints it (`30`, `30.5`, `NaN`, `Infinity`, `1e-7`, `1e+21`).
pub fn js_num(n: f64) -> String {
    if n.is_nan() {
        "NaN".to_string()
    } else if n.is_infinite() {
        let s = if n > 0.0 { "Infinity" } else { "-Infinity" };
        s.to_string()
    } else if n != 0.0 && (n.abs() < 1e-6 || n.abs() >= 1e21) {
        // JavaScript switches to exponent form here and signs a positive exponent (`1e+21`).
        let s = format!("{n:e}");
        match s.split_once('e') {
            Some((m, e)) if !e.starts_with('-') => format!("{m}e+{e}"),
            _ => s,
        }
    } else if n == n.trunc() && n.abs() < 1e21 {
        format!("{}", n as i128)
    } else {
        format!("{n}")
    }
}

/// JavaScript `Date.parse(iso.slice(0, 10))` as a UTC calendar date: `YYYY-MM-DD`, `YYYY-MM` (the 1st) or `YYYY`
/// (1 January); anything else is no date.
///
/// Deliberately stricter than V8 (T9 review M2, keep it): V8 rolls an impossible day over (`2026-02-30` →
/// 2 March) and reads non-ISO forms (`2026/10/09`, `Oct 9 2026`) as LOCAL time, so the answer depends on the
/// server's time zone. Here both are no date, and the rule escalates ("Fact … is missing") instead of deciding on a
/// date nobody wrote.
fn date_of(iso: &str) -> Option<NaiveDate> {
    let head: String = iso.chars().take(10).collect();
    let parts: Vec<&str> = head.split('-').collect();
    let num = |s: &str, len: usize| -> Option<u32> {
        if s.len() == len && s.chars().all(|c| c.is_ascii_digit()) {
            s.parse().ok()
        } else {
            None
        }
    };
    let (y, m, d) = match parts.as_slice() {
        [y] => (num(y, 4)?, 1, 1),
        [y, m] => (num(y, 4)?, num(m, 2)?, 1),
        [y, m, d] => (num(y, 4)?, num(m, 2)?, num(d, 2)?),
        _ => return None,
    };
    NaiveDate::from_ymd_opt(i32::try_from(y).ok()?, m, d)
}

/// Whole days from `from` to `to` (fixture `daysBetween`); `None` when either is no date.
fn days_between(from: &str, to: &str) -> Option<i64> {
    Some((date_of(to)? - date_of(from)?).num_days())
}

/// Whole months from `from` to `to` (fixture `monthsBetween`); `None` when either is no date.
fn months_between(from: &str, to: &str) -> Option<i64> {
    let (a, b) = (date_of(from)?, date_of(to)?);
    let mut m = i64::from(b.year() - a.year()) * 12 + i64::from(b.month()) - i64::from(a.month());
    if b.day() < a.day() {
        m -= 1;
    }
    Some(m)
}

/// Fixture `list`: comma separated, trimmed, lower-cased, empties dropped.
fn list(v: Option<&str>) -> Vec<String> {
    v.unwrap_or("")
        .split(',')
        .map(|x| js_trim(x).to_lowercase())
        .filter(|x| !x.is_empty())
        .collect()
}

/// Evaluate an authored definition without dispatching on its identifier. Missing/invalid facts never guess;
/// unset tenant thresholds remain placeholders. Existing release slots without a program keep their legacy evaluator.
pub fn evaluate_definition(
    id: &str,
    program: Option<&RuleProgram>,
    facts: &BTreeMap<String, String>,
    values: &[(String, Option<String>)],
    today: &str,
) -> RuleEvaluation {
    let Some(program) = program else {
        return evaluate(id, facts, values, today);
    };
    let mut result = RuleEvaluation {
        rule: id.to_owned(),
        outcome: "escalate",
        code: "escalate".into(),
        reason: String::new(),
        trace: Vec::new(),
        values_used: values
            .iter()
            .map(|(k, v)| (k.clone(), v.clone().map_or(Value::Null, Value::String)))
            .collect(),
    };
    let unset: Vec<_> = values
        .iter()
        .filter(|(_, v)| v.as_deref().is_none_or(|v| js_trim(v).is_empty()))
        .map(|(k, _)| k.as_str())
        .collect();
    if !unset.is_empty() {
        result.outcome = "placeholder";
        result.code = "placeholder".into();
        result.reason = format!(
            "{id} has no value for {}: a person must decide.",
            unset.join(", ")
        );
        return result;
    }
    let mut nodes = 0;
    match condition(
        &program.condition,
        facts,
        values,
        today,
        0,
        &mut nodes,
        &mut result.trace,
    ) {
        Ok(true) => {
            result.outcome = "eligible";
            result.code = "eligible".into();
            result.reason = program.eligible_reason.clone();
        }
        Ok(false) => {
            result.outcome = "ineligible";
            result.code = format!("ineligible:{id}");
            result.reason = program.ineligible_reason.clone();
        }
        Err(reason) => {
            result.reason = reason;
        }
    }
    result
}

fn operand(
    operand: &RuleOperand,
    facts: &BTreeMap<String, String>,
    values: &[(String, Option<String>)],
    today: &str,
) -> Result<String, String> {
    match operand {
        RuleOperand::Fact { key } => facts
            .get(key)
            .filter(|v| !js_trim(v).is_empty())
            .cloned()
            .ok_or_else(|| format!("Fact {key} is missing: the agent reads it first.")),
        RuleOperand::Value { key } => values
            .iter()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| v.clone())
            .filter(|v| !js_trim(v).is_empty())
            .ok_or_else(|| format!("Threshold {key} is not set: a person must decide.")),
        RuleOperand::Literal { value } => Ok(value.clone()),
        RuleOperand::DaysSince { key } | RuleOperand::MonthsSince { key } => {
            let date = facts
                .get(key)
                .ok_or_else(|| format!("Fact {key} is missing: the agent reads it first."))?;
            let age = if matches!(operand, RuleOperand::DaysSince { .. }) {
                days_between(date, today)
            } else {
                months_between(date, today)
            };
            age.filter(|n| *n >= 0).map(|n| n.to_string()).ok_or_else(|| format!("Fact {key} or the evaluation date is invalid or in the future: a person must decide."))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn condition(
    test: &RuleCondition,
    facts: &BTreeMap<String, String>,
    values: &[(String, Option<String>)],
    today: &str,
    depth: usize,
    nodes: &mut usize,
    trace: &mut Vec<String>,
) -> Result<bool, String> {
    *nodes += 1;
    if depth > MAX_CONDITION_DEPTH || *nodes > MAX_CONDITION_NODES {
        return Err(
            "The rule exceeds the allowed condition complexity: a person must decide.".into(),
        );
    }
    match test {
        RuleCondition::All { conditions } | RuleCondition::Any { conditions } => {
            if conditions.is_empty() {
                return Err(
                    "The rule contains an empty condition group: a person must decide.".into(),
                );
            }
            let all = matches!(test, RuleCondition::All { .. });
            for test in conditions {
                let matched = condition(test, facts, values, today, depth + 1, nodes, trace)?;
                if all != matched {
                    return Ok(matched);
                }
            }
            Ok(all)
        }
        RuleCondition::Compare { left, op, right } => {
            let a = operand(left, facts, values, today)?;
            let b = operand(right, facts, values, today)?;
            let matched = match op {
                RuleComparison::Eq => a == b,
                RuleComparison::Ne => a != b,
                RuleComparison::In | RuleComparison::NotIn => {
                    let found = list(Some(&b)).contains(&js_trim(&a).to_lowercase());
                    if *op == RuleComparison::In {
                        found
                    } else {
                        !found
                    }
                }
                op => {
                    let finite = |v: &str| v.trim().parse::<f64>().ok().filter(|n| n.is_finite());
                    let (Some(a), Some(b)) = (finite(&a), finite(&b)) else {
                        return Err("A numeric comparison received a missing or invalid number: a person must decide.".into());
                    };
                    match op {
                        RuleComparison::Lt => a < b,
                        RuleComparison::Lte => a <= b,
                        RuleComparison::Gt => a > b,
                        RuleComparison::Gte => a >= b,
                        _ => unreachable!(),
                    }
                }
            };
            trace.push(format!(
                "{left:?} = {a:?} {op:?} {right:?} = {b:?} → {matched}"
            ));
            Ok(matched)
        }
    }
}

/// Evaluate rule `id` with `facts` against `values` (in the rule's slot order; `None` = not set) on day `today`
/// (`YYYY-MM-DD…`). Fixture `evaluateJ9`.
pub fn evaluate(
    id: &str,
    facts: &BTreeMap<String, String>,
    values: &[(String, Option<String>)],
    today: &str,
) -> RuleEvaluation {
    let values_used: Map<String, Value> = values
        .iter()
        .map(|(k, v)| (k.clone(), v.clone().map_or(Value::Null, Value::String)))
        .collect();
    let out =
        |outcome: &'static str, code: String, reason: String, trace: Vec<String>| RuleEvaluation {
            rule: id.to_string(),
            outcome,
            code,
            reason,
            trace,
            values_used: values_used.clone(),
        };
    let unset: Vec<&str> = values
        .iter()
        .filter(|(_, v)| v.as_deref().is_none_or(|s| js_trim(s).is_empty()))
        .map(|(k, _)| k.as_str())
        .collect();
    if !unset.is_empty() {
        let names = unset.join(", ");
        return out(
            "placeholder",
            "placeholder".to_string(),
            format!(
                "{id} has no value for {names}: the agent cannot decide and hands the case to a person."
            ),
            vec![format!("value {names} = not set → cannot evaluate")],
        );
    }
    let no = |reason: String, trace: Vec<String>| {
        out("ineligible", format!("ineligible:{id}"), reason, trace)
    };
    let yes =
        |reason: String, trace: Vec<String>| out("eligible", "eligible".into(), reason, trace);
    let escalate =
        |reason: String, trace: Vec<String>| out("escalate", "escalate".into(), reason, trace);
    let missing = |k: &str| {
        escalate(
            format!("Fact {k} is missing: the agent reads it first."),
            vec![format!("{k} = ?")],
        )
    };
    let value = |k: &str| -> Option<&str> {
        values
            .iter()
            .find(|(key, _)| key == k)
            .and_then(|(_, v)| v.as_deref())
    };
    let fact = |k: &str| facts.get(k).map(String::as_str);
    // JavaScript `undefined` in a template string
    let shown = |v: Option<&str>| v.unwrap_or("undefined").to_string();

    match id {
        "RET-01" => {
            let Some(delivered) = fact("order.delivered_at").filter(|s| !s.is_empty()) else {
                return missing("order.delivered_at");
            };
            let window = value("window_days").map_or(f64::NAN, js_number);
            let Some(days) = days_between(delivered, today) else {
                return missing("order.delivered_at");
            };
            let category = js_trim(fact("offering.category").unwrap_or(""));
            let excluded = list(value("excluded_categories")).join(", ");
            let shown_category = if category.is_empty() { "—" } else { category };
            let first = format!("category “{shown_category}” ∉ excluded [{excluded}]");
            if !category.is_empty()
                && list(value("excluded_categories")).contains(&category.to_lowercase())
            {
                return no(
                    format!("{category} cannot be returned."),
                    vec![format!("category “{category}” ∈ excluded [{excluded}]")],
                );
            }
            let d = days as f64;
            if d > window {
                return no(
                    format!(
                        "The return window closed {} days ago (day {days} of {}).",
                        js_num(d - window),
                        js_num(window)
                    ),
                    vec![
                        first,
                        format!("{days} days since delivery > {}", js_num(window)),
                    ],
                );
            }
            yes(
                format!(
                    "Day {days} of {}: the return can be opened.",
                    js_num(window)
                ),
                vec![
                    first,
                    format!("{days} days since delivery ≤ {}", js_num(window)),
                ],
            )
        }
        "WAR-01" => {
            let Some(delivered) = fact("order.delivered_at").filter(|s| !s.is_empty()) else {
                return missing("order.delivered_at");
            };
            let period = value("period_months").map_or(f64::NAN, js_number);
            let Some(months) = months_between(delivered, today) else {
                return missing("order.delivered_at");
            };
            if months as f64 >= period {
                return no(
                    format!(
                        "Out of warranty: delivered {months} months ago, the guarantee is {} months.",
                        js_num(period)
                    ),
                    vec![format!(
                        "{months} months since delivery ≥ {}",
                        js_num(period)
                    )],
                );
            }
            yes(
                format!(
                    "Month {} of {}: a warranty claim can be opened.",
                    months + 1,
                    js_num(period)
                ),
                vec![format!(
                    "{months} months since delivery < {}",
                    js_num(period)
                )],
            )
        }
        "PAY-04" => {
            let status = js_trim(fact("payment.status").unwrap_or(""));
            if status.is_empty() {
                return missing("payment.status");
            }
            if status != "pending" {
                return yes(
                    format!("The charge is {status}, not pending: PAY-04 does not block a refund."),
                    vec![format!("payment.status = {status} ≠ pending")],
                );
            }
            let age = fact("payment.at")
                .filter(|s| !s.is_empty())
                .and_then(|at| days_between(at, today));
            let limit = value("drop_off_days").map_or(f64::NAN, js_number);
            if let Some(age) = age
                && age as f64 > limit
            {
                return escalate(
                    format!(
                        "The reservation has been pending {age} days, longer than {}: a person checks it with the bank.",
                        js_num(limit)
                    ),
                    vec![
                        "payment.status = pending".to_string(),
                        format!("{age} days pending > {}", js_num(limit)),
                    ],
                );
            }
            no(
                format!(
                    "It is a reservation, not a payment. Customer is told: “{}”",
                    shown(value("customer_text"))
                ),
                vec![
                    "payment.status = pending".to_string(),
                    format!(
                        "{} days pending ≤ {}",
                        age.map_or_else(|| "?".to_string(), |a| a.to_string()),
                        js_num(limit)
                    ),
                ],
            )
        }
        "DIS-02" => {
            if fact("party.vulnerable") == Some("yes") {
                return no(
                    "Vulnerable customer: no disconnection notice is sent; a person takes over."
                        .to_string(),
                    vec!["party.vulnerable = yes".to_string()],
                );
            }
            yes(
                format!(
                    "A notice of {} days is required before disconnection.",
                    shown(value("notice_days"))
                ),
                vec!["party.vulnerable = no".to_string()],
            )
        }
        "ARR-01" => {
            let amount = fact("arrangement.instalment_eur").map_or(f64::NAN, js_number);
            let min = value("min_instalment_eur").map_or(f64::NAN, js_number);
            if !amount.is_finite() {
                return missing("arrangement.instalment_eur");
            }
            let (a, m) = (js_num(amount), js_num(min));
            if amount < min {
                return no(
                    format!("€{a} a month is below the €{m} minimum."),
                    vec![format!("€{a} < €{m}")],
                );
            }
            yes(
                format!("€{a} a month meets the €{m} minimum."),
                vec![format!("€{a} ≥ €{m}")],
            )
        }
        _ => escalate(
            format!("Rule {id} has no evaluation definition: a person must decide."),
            Vec::new(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authored_groups_dates_thresholds_and_missing_facts() {
        let p: RuleProgram = serde_json::from_value(serde_json::json!({"condition":{"kind":"all","conditions":[
            {"kind":"compare","left":{"kind":"days_since","key":"order.delivered_at"},"op":"lte","right":{"kind":"value","key":"window"}},
            {"kind":"any","conditions":[{"kind":"compare","left":{"kind":"fact","key":"order.category"},"op":"in","right":{"kind":"literal","value":"books, tools"}},{"kind":"compare","left":{"kind":"fact","key":"order.total"},"op":"lt","right":{"kind":"literal","value":"50"}}]}
        ]},"eligible_reason":"Within policy.","ineligible_reason":"Outside policy."})).unwrap();
        let facts = [
            ("order.delivered_at".into(), "2026-10-01".into()),
            ("order.category".into(), "Books".into()),
        ]
        .into_iter()
        .collect();
        let values = vec![("window".into(), Some("30".into()))];
        assert_eq!(
            evaluate_definition("ANY-ID", Some(&p), &facts, &values, "2026-10-20").outcome,
            "eligible"
        );
        assert_eq!(
            evaluate_definition("ANY-ID", Some(&p), &facts, &values, "2026-11-20").outcome,
            "ineligible"
        );
        assert_eq!(
            evaluate_definition("ANY-ID", Some(&p), &BTreeMap::new(), &values, "2026-10-20")
                .outcome,
            "escalate"
        );
        assert_eq!(
            evaluate_definition(
                "ANY-ID",
                Some(&p),
                &facts,
                &[("window".into(), None)],
                "2026-10-20"
            )
            .outcome,
            "placeholder"
        );
        assert_eq!(
            evaluate_definition("ANY-ID", Some(&p), &facts, &values, "2026-09-20").outcome,
            "escalate"
        );
    }

    #[test]
    fn numbers_read_and_print_as_javascript() {
        assert_eq!(js_number(" 30 "), 30.0);
        assert_eq!(js_number(""), 0.0);
        assert_eq!(js_number("0x1A"), 26.0);
        assert_eq!(js_number("1e2"), 100.0);
        assert!(js_number("inf").is_nan());
        assert!(js_number("abc").is_nan());
        assert_eq!(js_number("Infinity"), f64::INFINITY);
        assert_eq!(js_num(30.0), "30");
        assert_eq!(js_num(30.5), "30.5");
        assert_eq!(js_num(-22.0), "-22");
        assert_eq!(js_num(f64::NAN), "NaN");
        // review M1/M3/M5: exponent forms, `0x+1`, U+FEFF
        assert_eq!(js_num(1e-7), "1e-7");
        assert_eq!(js_num(-1.5e-7), "-1.5e-7");
        assert_eq!(js_num(1e-6), "0.000001");
        assert_eq!(js_num(1e21), "1e+21");
        assert_eq!(js_num(1e20), "100000000000000000000");
        assert!(js_number("0x+1").is_nan());
        assert_eq!(js_number("\u{FEFF}30\u{FEFF}"), 30.0);
        assert_eq!(js_trim("\u{FEFF} a \u{FEFF}"), "a");
    }

    #[test]
    fn dates_read_as_the_fixture_slices_them() {
        assert_eq!(days_between("2026-10-09", "2026-10-18"), Some(9));
        assert_eq!(days_between("2026-10-09T23:59:00Z", "2026-10-18"), Some(9));
        assert_eq!(days_between("soon", "2026-10-18"), None);
        assert_eq!(months_between("2024-10-19", "2026-10-18"), Some(23));
        assert_eq!(months_between("2024-10-18", "2026-10-18"), Some(24));
        // deliberate deviation from V8 (review M2): no roll-over, no local-time forms
        assert_eq!(days_between("2026-02-30", "2026-10-18"), None);
        assert_eq!(days_between("2026/10/09", "2026-10-18"), None);
        assert_eq!(days_between("Oct 9 2026", "2026-10-18"), None);
    }
}
