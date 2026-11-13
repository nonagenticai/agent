//! The pure half of the gate library routes (T12, T12d): the `GateQuestion` view of a row, the edits of
//! `updateGateQuestion` / `addGateOption` / `removeGateOption`, and the `createGateQuestion` draft. Ports of the
//! fixture (`web/src/lib/api/areas/ontology.ts` P14) on top of the agent-model gate validators, same codes and
//! messages.
use agent_model::kinds::gate::{
    self, FireWhen, GateAction, GateFail, GateGroup, GateOption, GatePoint, GateQuestionFields,
    GateRule, GateType, MAX_OPTIONS,
};
use agent_model::kinds::{self, FieldError, Kind, Refusal};
use axum::http::StatusCode;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};

use super::super::{conflict, missing, unprocessable};
use crate::error::{ApiError, ApiResult};

/// `generated_from` of a question whose options are switched to generated (fixture text).
pub const GENERATED_FROM: &str = "the release cut, from the job’s effective grid";

/// An agent-model refusal as the route's error (status and code one to one).
pub fn refused(r: Refusal) -> ApiError {
    match r.status {
        404 => ApiError::Missing(r.code, r.message),
        409 => ApiError::Conflict(r.code, r.message),
        422 => ApiError::Unprocessable(r.code, r.message),
        s => ApiError::detailed(
            StatusCode::from_u16(s).unwrap_or(StatusCode::UNPROCESSABLE_ENTITY),
            r.code,
            r.message,
        ),
    }
}

/// The whole-row validation as a 422 with the first error's code (`invalid_rule`, `too_many_options`, ...) and
/// every error under `errors`.
pub fn field_errors(errors: Vec<FieldError>) -> ApiError {
    let first = errors.first().cloned();
    let list = serde_json::to_value(&errors).unwrap_or(Value::Null);
    match first {
        Some(e) => ApiError::Unprocessable(e.code, e.message).with("errors", list),
        None => ApiError::Unprocessable("invalid_row", "The row is not valid.".into()),
    }
}

/// [`kinds::validate_fields`] of a `gate_question`, as [`field_errors`].
pub fn validate(fields: &Value) -> ApiResult<()> {
    kinds::validate_fields(Kind::GateQuestion, fields).map_err(field_errors)
}

/// `choice`, `noul`, `score`.
pub fn type_name(kind: GateType) -> String {
    serde_json::to_value(kind)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// An ISO timestamp as the UI writes it (`2026-10-12T09:00:00Z`).
pub fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// The web `GateQuestion` of a current row: the fields with their defaults, plus `key`, `layer` (the row's
/// layer), `origin` `platform`, `customer_reply` (null when absent), and `updated_by` / `updated_at` = the row's
/// author / time (plan §5).
pub fn view(
    key: &str,
    layer: &str,
    q: &GateQuestionFields,
    author: &str,
    at: DateTime<Utc>,
) -> Value {
    let mut v = serde_json::to_value(q).unwrap_or_else(|_| json!({}));
    if let Some(m) = v.as_object_mut() {
        m.remove("summary");
        m.insert("key".into(), json!(key));
        m.insert("layer".into(), json!(layer));
        m.insert("origin".into(), json!("platform"));
        m.entry("customer_reply").or_insert(Value::Null);
        m.insert("updated_by".into(), json!(author));
        m.insert("updated_at".into(), json!(iso(at)));
    }
    v
}

/// A stored `gate_question` row's fields.
pub fn parse(fields: &Value) -> Result<GateQuestionFields, String> {
    serde_json::from_value(fields.clone()).map_err(|e| e.to_string())
}

/// `null` = present and null; absent = `None` (with `#[serde(default)]`).
fn present<'de, D, T>(d: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Some)
}

/// `PATCH /api/platform/gates/{key}` body (web `GateQuestionEdit` + F-16 `customer_reply`).
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateEdit {
    #[serde(default)]
    pub question: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub threshold_value: Option<Option<f64>>,
    #[serde(default)]
    pub generated: Option<bool>,
    #[serde(default)]
    pub rule: Option<GateRule>,
    #[serde(default, deserialize_with = "present")]
    pub customer_reply: Option<Option<String>>,
}

/// A customer reply as stored: trimmed; blank or null clears it; else 1–500 characters (422
/// `invalid_customer_reply`).
pub fn customer_reply(text: Option<&str>) -> ApiResult<Option<String>> {
    match text.map(str::trim) {
        None | Some("") => Ok(None),
        Some(t) => {
            gate::check_customer_reply(t).map_err(refused)?;
            Ok(Some(t.to_owned()))
        }
    }
}

/// The first number of a written threshold replaced by `v` (fixture `threshold.replace(/\d+(\.\d+)?/, v)`).
pub fn replace_number(threshold: &str, v: f64) -> String {
    let b = threshold.as_bytes();
    let Some(start) = b.iter().position(u8::is_ascii_digit) else {
        return threshold.to_owned();
    };
    let mut end = start;
    while end < b.len() && b[end].is_ascii_digit() {
        end += 1;
    }
    if end + 1 < b.len() && b[end] == b'.' && b[end + 1].is_ascii_digit() {
        end += 1;
        while end < b.len() && b[end].is_ascii_digit() {
            end += 1;
        }
    }
    format!("{}{v}{}", &threshold[..start], &threshold[end..])
}

/// `updateGateQuestion` on the row's fields, in the fixture's order: question text (422 `invalid_question`),
/// tuned threshold (409 `threshold_not_tunable`, 422 `invalid_threshold`), rule (409 `safety_gate`, 422
/// `invalid_rule`), generated options (409 `options_fixed`), customer reply (422 `invalid_customer_reply`).
pub fn apply_edit(key: &str, q: &mut GateQuestionFields, e: GateEdit) -> ApiResult<()> {
    if let Some(text) = e.question {
        q.question = gate::check_question_text(&text).map_err(refused)?;
    }
    if let Some(tv) = e.threshold_value {
        gate::check_tunable(key, q).map_err(refused)?;
        let v = tv.ok_or_else(|| unprocessable("invalid_threshold", "Give a threshold."))?;
        gate::check_threshold(q.kind, q.options.len(), v).map_err(refused)?;
        q.threshold_value = Some(v);
        q.threshold = replace_number(&q.threshold, v);
        q.rule = gate::tuned_rule(&q.rule, Some(v));
    }
    if let Some(rule) = e.rule {
        gate::check_rule_editable(key, q).map_err(refused)?;
        q.rule = gate::check_rule(q.kind, &q.options, &rule).map_err(refused)?;
    }
    if let Some(generated) = e.generated {
        if q.kind != GateType::Choice {
            return Err(conflict(
                "options_fixed",
                format!(
                    "{} options are fixed; only choice options can be generated.",
                    type_name(q.kind)
                ),
            ));
        }
        q.generated_from = if generated {
            q.generated_from
                .clone()
                .or_else(|| Some(GENERATED_FROM.to_owned()))
        } else {
            None
        };
        for o in &mut q.options {
            o.generated = generated;
        }
    }
    if let Some(reply) = e.customer_reply {
        q.customer_reply = customer_reply(reply.as_deref())?;
    }
    Ok(())
}

/// An option key from its label (fixture: lower case, runs of other characters → `_`, no `_` at either end).
pub fn option_key(label: &str) -> String {
    let mut out = String::new();
    let mut gap = false;
    for c in label.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if gap && !out.is_empty() {
                out.push('_');
            }
            gap = false;
            out.push(c);
        } else {
            gap = true;
        }
    }
    out
}

fn check_authored_options(key: &str, q: &GateQuestionFields) -> ApiResult<()> {
    if q.kind != GateType::Choice {
        return Err(conflict(
            "options_fixed",
            format!(
                "{} options are fixed; only choice questions take options.",
                type_name(q.kind)
            ),
        ));
    }
    if q.generated_from.is_some() {
        return Err(conflict(
            "options_generated",
            format!("{key} options are generated at the release cut; they are read-only."),
        ));
    }
    Ok(())
}

/// `addGateOption`: 409 `options_fixed`, `options_generated`, `option_exists`; 422 `invalid_option`,
/// `too_many_options`.
pub fn add_option(
    key: &str,
    q: &mut GateQuestionFields,
    label: &str,
    description: &str,
) -> ApiResult<()> {
    check_authored_options(key, q)?;
    let label = label.trim();
    if label.is_empty() {
        return Err(unprocessable("invalid_option", "An option needs a label."));
    }
    let opt_key = option_key(label);
    if opt_key.is_empty() {
        return Err(unprocessable(
            "invalid_option",
            "An option label needs a letter or a digit.",
        ));
    }
    if q.options
        .iter()
        .any(|o| o.key == opt_key || o.label.to_lowercase() == label.to_lowercase())
    {
        return Err(conflict(
            "option_exists",
            format!("{key} already has “{label}”."),
        ));
    }
    if q.options.len() >= MAX_OPTIONS {
        return Err(unprocessable(
            "too_many_options",
            "A choice takes at most 255 options.",
        ));
    }
    q.options.push(GateOption {
        key: opt_key,
        label: label.to_owned(),
        description: description.trim().to_owned(),
        generated: false,
    });
    Ok(())
}

/// `removeGateOption`: 409 `options_fixed`, `options_generated`, `last_option`; 404 `option_not_found`; 422
/// `invalid_rule` while the rule still fires on the option (change the rule first).
pub fn remove_option(key: &str, q: &mut GateQuestionFields, option: &str) -> ApiResult<()> {
    check_authored_options(key, q)?;
    let Some(i) = q.options.iter().position(|o| o.key == option) else {
        return Err(missing(
            "option_not_found",
            format!("{key} has no option {option}."),
        ));
    };
    if q.options.len() == 1 {
        return Err(conflict(
            "last_option",
            format!("{option} is the last option of {key}; a choice keeps at least one."),
        ));
    }
    let fires = q.rule.fire_when == FireWhen::In
        && q.rule
            .fire_values
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|v| v == option);
    if fires {
        return Err(unprocessable(
            "invalid_rule",
            format!("The rule of {key} fires on {option}; change the rule first."),
        ));
    }
    q.options.remove(i);
    Ok(())
}

/// One option of a new question; `key` defaults to the label's [`option_key`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionDraft {
    #[serde(default)]
    pub key: Option<String>,
    pub label: String,
    #[serde(default)]
    pub description: String,
}

/// `POST /api/platform/gates` body (`GateQuestionDraft`). `fail` defaults to open (closed, and only closed, for a
/// safety question), `action` to annotate; a noul question without options gets `true` / `false`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateDraft {
    pub key: String,
    pub layer: String,
    pub group: GateGroup,
    pub point: GatePoint,
    #[serde(rename = "type")]
    pub kind: GateType,
    pub question: String,
    #[serde(default)]
    pub options: Vec<OptionDraft>,
    #[serde(default)]
    pub threshold: String,
    #[serde(default)]
    pub threshold_value: Option<f64>,
    pub rule: GateRule,
    #[serde(default)]
    pub fail: Option<GateFail>,
    #[serde(default)]
    pub action: Option<GateAction>,
    #[serde(default)]
    pub effect: String,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub customer_reply: Option<String>,
}

/// A new library key: `G` then 1–7 capital letters or digits (the agent-model gate key, at most 8); 422
/// `invalid_gate_key`.
pub fn check_new_key(key: &str) -> ApiResult<()> {
    let ok = key.starts_with('G') && kinds::validate_key(Kind::GateQuestion, key).is_ok();
    if ok {
        Ok(())
    } else {
        Err(unprocessable(
            "invalid_gate_key",
            format!("“{key}” is not a new gate key: G and 1 to 7 capital letters or digits (GX1)."),
        ))
    }
}

/// The fields of a new library question, checked like an edit: 422 `invalid_gate_key`, `invalid_question`,
/// `invalid_option`, `too_many_options`, `invalid_rule`, `invalid_threshold`, `invalid_customer_reply`, then the
/// whole-row validation.
pub fn draft_fields(d: GateDraft, by: &str, now: DateTime<Utc>) -> ApiResult<GateQuestionFields> {
    check_new_key(&d.key)?;
    if d.group == GateGroup::Tenant {
        return Err(unprocessable(
            "invalid_question",
            "A tenant question belongs to one job; the library takes base, safety, post or industry groups.",
        ));
    }
    let question = gate::check_question_text(&d.question).map_err(refused)?;
    let mut options: Vec<GateOption> = Vec::with_capacity(d.options.len());
    for (i, o) in d.options.into_iter().enumerate() {
        let label = o.label.trim().to_owned();
        let key = o
            .key
            .map(|k| k.trim().to_owned())
            .unwrap_or_else(|| option_key(&label));
        if label.is_empty() || key.is_empty() || options.iter().any(|x| x.key == key) {
            return Err(unprocessable(
                "invalid_option",
                format!("Option {} needs a label and a key of its own.", i + 1),
            ));
        }
        options.push(GateOption {
            key,
            label,
            description: o.description.trim().to_owned(),
            generated: false,
        });
    }
    if options.is_empty() && d.kind == GateType::Noul {
        options = ["true", "false"]
            .into_iter()
            .map(|k| GateOption {
                key: k.to_owned(),
                label: k.to_owned(),
                description: String::new(),
                generated: false,
            })
            .collect();
    }
    if options.len() > MAX_OPTIONS {
        return Err(unprocessable(
            "too_many_options",
            "A choice takes at most 255 options.",
        ));
    }
    let rule = gate::check_rule(d.kind, &options, &d.rule).map_err(refused)?;
    let safety = d.group == GateGroup::Safety;
    let threshold_value = if safety { None } else { d.threshold_value };
    if let Some(v) = threshold_value {
        gate::check_threshold(d.kind, options.len(), v).map_err(refused)?;
    }
    let q = GateQuestionFields {
        group: d.group,
        point: d.point,
        kind: d.kind,
        question,
        options,
        generated_from: None,
        threshold: d.threshold.trim().to_owned(),
        threshold_value,
        fail: if safety {
            GateFail::Closed
        } else {
            d.fail.unwrap_or(GateFail::Open)
        },
        safety,
        effect: d.effect.trim().to_owned(),
        rule,
        action: d.action.unwrap_or(GateAction::Annotate),
        tools: d.tools,
        customer_reply: customer_reply(d.customer_reply.as_deref())?,
        origin: Some("platform".to_owned()),
        updated_by: Some(by.to_owned()),
        updated_at: Some(iso(now)),
        summary: None,
    };
    validate(&serde_json::to_value(&q).unwrap_or(Value::Null))?;
    Ok(q)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gt() -> GateQuestionFields {
        serde_json::from_value(json!({
            "group": "base", "point": "P1", "type": "choice",
            "question": "Which theme is the customer in?",
            "options": [
                {"key": "need", "label": "Need", "description": "", "generated": false},
                {"key": "issue", "label": "Issue", "description": "", "generated": false}
            ],
            "threshold": "top ≥ 0.5", "threshold_value": 0.5, "fail": "open", "safety": false, "effect": "",
            "rule": {"fire_when": "in", "threshold": null, "fire_values": ["issue"], "min_confidence": 0.5},
            "action": "annotate"
        }))
        .expect("GT")
    }

    fn noul(safety: bool) -> GateQuestionFields {
        GateQuestionFields {
            kind: GateType::Noul,
            safety,
            threshold_value: None,
            threshold: "any true".into(),
            options: vec![],
            rule: GateRule {
                fire_when: FireWhen::Gte,
                threshold: Some(0.5),
                fire_values: None,
                min_confidence: None,
            },
            ..gt()
        }
    }

    fn code<T>(r: ApiResult<T>) -> &'static str {
        r.err().map(|e| e.code()).unwrap_or("ok")
    }

    fn edit(v: Value) -> GateEdit {
        serde_json::from_value(v).expect("edit")
    }

    #[test]
    fn edits_follow_the_fixture() {
        let mut q = gt();
        assert_eq!(
            code(apply_edit(
                "GT",
                &mut q,
                edit(json!({"question": " short "}))
            )),
            "invalid_question"
        );
        assert_eq!(
            code(apply_edit(
                "GT",
                &mut q,
                edit(json!({"threshold_value": null}))
            )),
            "invalid_threshold"
        );
        assert_eq!(
            code(apply_edit(
                "GT",
                &mut q,
                edit(json!({"threshold_value": 1.4}))
            )),
            "invalid_threshold"
        );
        apply_edit("GT", &mut q, edit(json!({"threshold_value": 0.7}))).expect("tune");
        assert_eq!(q.threshold, "top ≥ 0.7");
        assert_eq!(q.rule.min_confidence, Some(0.7));
        assert_eq!(
            code(apply_edit(
                "GC",
                &mut noul(false),
                edit(json!({"threshold_value": 0.5}))
            )),
            "threshold_not_tunable"
        );
        let rule = json!({"rule": {"fire_when": "gte", "threshold": 0.4, "fire_values": null, "min_confidence": null}});
        assert_eq!(
            code(apply_edit("B1", &mut noul(true), edit(rule.clone()))),
            "safety_gate"
        );
        assert_eq!(code(apply_edit("GC", &mut noul(false), edit(rule))), "ok");
        assert_eq!(
            code(apply_edit(
                "GC",
                &mut noul(false),
                edit(json!({"generated": true}))
            )),
            "options_fixed"
        );
        let mut g = gt();
        apply_edit("GT", &mut g, edit(json!({"generated": true}))).expect("generated");
        assert_eq!(g.generated_from.as_deref(), Some(GENERATED_FROM));
        assert!(g.options.iter().all(|o| o.generated));
        apply_edit(
            "GT",
            &mut g,
            edit(json!({"customer_reply": "  We will call you.  "})),
        )
        .expect("reply");
        assert_eq!(g.customer_reply.as_deref(), Some("We will call you."));
        apply_edit("GT", &mut g, edit(json!({"customer_reply": null}))).expect("clear");
        assert_eq!(g.customer_reply, None);
        assert_eq!(
            code(apply_edit(
                "GT",
                &mut g,
                edit(json!({"customer_reply": "x".repeat(501)}))
            )),
            "invalid_customer_reply"
        );
        assert!(serde_json::from_value::<GateEdit>(json!({"nope": 1})).is_err());
    }

    #[test]
    fn numbers_and_keys() {
        assert_eq!(replace_number("top ≥ 0.5", 0.7), "top ≥ 0.7");
        assert_eq!(
            replace_number("≥ 4 (twice → handover)", 3.0),
            "≥ 3 (twice → handover)"
        );
        assert_eq!(replace_number("any true", 0.5), "any true");
        assert_eq!(option_key(" Billing question! "), "billing_question");
        assert_eq!(option_key("--A  b--"), "a_b");
        assert_eq!(option_key("!!"), "");
        assert!(check_new_key("GX1").is_ok());
        for bad in ["XX", "G", "gx1", "GABCDEFGH", "G-1"] {
            assert_eq!(code(check_new_key(bad)), "invalid_gate_key", "{bad}");
        }
    }

    #[test]
    fn options() {
        let mut q = gt();
        add_option("GT", &mut q, " Billing question ", "money").expect("add");
        assert_eq!(
            q.options.last().map(|o| o.key.as_str()),
            Some("billing_question")
        );
        assert_eq!(
            code(add_option("GT", &mut q, "billing question", "")),
            "option_exists"
        );
        assert_eq!(code(add_option("GT", &mut q, "  ", "")), "invalid_option");
        assert_eq!(
            code(add_option("GC", &mut noul(false), "x", "")),
            "options_fixed"
        );
        let mut generated = GateQuestionFields {
            generated_from: Some("cut".into()),
            ..gt()
        };
        assert_eq!(
            code(add_option("GA", &mut generated, "x", "")),
            "options_generated"
        );
        assert_eq!(
            code(remove_option("GA", &mut generated, "need")),
            "options_generated"
        );
        assert_eq!(
            code(remove_option("GT", &mut q, "nope")),
            "option_not_found"
        );
        assert_eq!(code(remove_option("GT", &mut q, "issue")), "invalid_rule");
        remove_option("GT", &mut q, "need").expect("remove");
        remove_option("GT", &mut q, "billing_question").expect("remove");
        assert_eq!(code(remove_option("GT", &mut q, "issue")), "last_option");
    }

    fn draft(v: Value) -> GateDraft {
        serde_json::from_value(v).expect("draft")
    }

    fn base_draft() -> Value {
        json!({
            "key": "GX1", "layer": "base", "group": "base", "point": "P1", "type": "choice",
            "question": "Is the customer asking about a delivery?",
            "options": [{"label": "Yes"}, {"label": "No"}],
            "threshold": "top ≥ 0.6", "threshold_value": 0.6,
            "rule": {"fire_when": "in", "fire_values": ["yes"], "min_confidence": 0.6}
        })
    }

    #[test]
    fn drafts_validate_like_edits() {
        let now = Utc::now();
        let q = draft_fields(draft(base_draft()), "u-super", now).expect("draft");
        assert_eq!(q.options[0].key, "yes");
        assert_eq!(q.fail, GateFail::Open);
        assert_eq!(q.action, GateAction::Annotate);
        let mut short = base_draft();
        short["question"] = json!("Why?");
        assert_eq!(
            code(draft_fields(draft(short), "u", now)),
            "invalid_question"
        );
        let mut rule = base_draft();
        rule["rule"]["fire_values"] = json!(["zz"]);
        assert_eq!(code(draft_fields(draft(rule), "u", now)), "invalid_rule");
        let mut thr = base_draft();
        thr["threshold_value"] = json!(1.5);
        assert_eq!(
            code(draft_fields(draft(thr), "u", now)),
            "invalid_threshold"
        );
        let mut dup = base_draft();
        dup["options"] = json!([{"label": "Yes"}, {"label": "yes"}]);
        assert_eq!(code(draft_fields(draft(dup), "u", now)), "invalid_option");
        let mut key = base_draft();
        key["key"] = json!("B9");
        assert_eq!(code(draft_fields(draft(key), "u", now)), "invalid_gate_key");
        let mut tenant = base_draft();
        tenant["group"] = json!("tenant");
        assert_eq!(
            code(draft_fields(draft(tenant), "u", now)),
            "invalid_question"
        );
        let safety = draft(json!({
            "key": "GZ", "layer": "base", "group": "safety", "point": "R1", "type": "noul",
            "question": "Does the draft promise a refund?", "threshold": "any true", "threshold_value": 0.5,
            "rule": {"fire_when": "gte", "threshold": 0.5}, "fail": "open", "action": "block"
        }));
        let s = draft_fields(safety, "u", now).expect("safety");
        assert!(s.safety);
        assert_eq!(s.fail, GateFail::Closed);
        assert_eq!(s.threshold_value, None);
        assert_eq!(s.options.len(), 2);
    }

    #[test]
    fn view_has_the_web_shape() {
        let at = DateTime::parse_from_rfc3339("2026-10-12T09:00:00Z")
            .expect("ts")
            .with_timezone(&Utc);
        let v = view("GT", "base", &gt(), "owner", at);
        assert_eq!(v["key"], "GT");
        assert_eq!(v["layer"], "base");
        assert_eq!(v["origin"], "platform");
        assert_eq!(v["customer_reply"], Value::Null);
        assert_eq!(v["generated_from"], Value::Null);
        assert_eq!(v["tools"], Value::Null);
        assert_eq!(v["updated_by"], "owner");
        assert_eq!(v["updated_at"], "2026-10-12T09:00:00Z");
        assert!(v.get("summary").is_none());
    }
}
