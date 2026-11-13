//! J7 targets: `gate` (a refinement of a LIBRARY gate's option, or of a gate's threshold / rule) and
//! `gate_question` (a question the tenant added to the job).
//!
//! - `gate` at `{gate, option}` = the `job_gate_option_refinement` row `{label, description, hidden, base_hash}`;
//!   at `{gate, option: null}` = the `job_gate_rule_refinement` row `{threshold_value, rule, base_hash}`. `proj` =
//!   `[label, description, hidden, threshold_value, rule]` (fixture; `base_hash` is not design). Refinements of a
//!   GENERATED option are B2 `overlay` places (`overlay:gate_option:<GATE> · <option>`), never here.
//! - `gate_question` at `{gate}` = the `job_gate_question` row `{key, point, type, question, options, threshold,
//!   threshold_value, fail, effect, rule, action, tools}`, plus `customer_reply` when the question has its own text
//!   (0146, T8a; absent = none, so a value written before T8a reads back unchanged).
//!
//! `threshold_value` is `numeric` in the table and a JSON number (`float8`) in the value.
use futures_util::future::BoxFuture;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::{SET_BY, decode, field, internal, job_of, js_string, present};
use crate::design::{DesignTarget, DraftRef, Place, Scope, Snap};
use crate::error::ApiResult;

/// The option of a `gate` ref; `None` = the gate's threshold / rule place.
fn option_of(r: &DraftRef) -> Option<&str> {
    r.get("option").and_then(Value::as_str)
}

fn gate_ref(gate: &str, option: Option<&str>) -> DraftRef {
    let mut m = DraftRef::new();
    m.insert("gate".to_owned(), Value::String(gate.to_owned()));
    m.insert(
        "option".to_owned(),
        option.map_or(Value::Null, |o| Value::String(o.to_owned())),
    );
    m
}

// ---- gate -----------------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
struct OptionRefinement {
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    hidden: bool,
    base_hash: String,
}

#[derive(Debug, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
struct RuleRefinement {
    #[serde(default)]
    threshold_value: Option<f64>,
    #[serde(default)]
    rule: Option<Value>,
    #[serde(default)]
    base_hash: Option<String>,
}

/// A refinement of a library gate option or of a gate's threshold / rule.
pub struct Gate;

impl DesignTarget for Gate {
    fn kind(&self) -> &'static str {
        "gate"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    /// Fixture `gate:${gate}:${option ?? 'threshold'}`.
    fn key(&self, r: &DraftRef) -> String {
        format!(
            "gate:{}:{}",
            field(r, "gate"),
            option_of(r).unwrap_or("threshold")
        )
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let gate = field(r, "gate");
            match option_of(r) {
                Some(option) => {
                    let row = sqlx::query_as::<_, OptionRefinement>(
                        "SELECT label, description, hidden, base_hash FROM job_gate_option_refinement \
                          WHERE job_id = $1 AND gate = $2 AND option = $3",
                    )
                    .bind(job)
                    .bind(gate)
                    .bind(option)
                    .fetch_optional(&mut *c)
                    .await?;
                    Ok(row.map(|o| {
                        json!({
                            "label": o.label,
                            "description": o.description,
                            "hidden": o.hidden,
                            "base_hash": o.base_hash,
                        })
                    }))
                }
                None => {
                    let row = sqlx::query_as::<_, RuleRefinement>(
                        "SELECT threshold_value::float8 AS threshold_value, rule, base_hash \
                           FROM job_gate_rule_refinement WHERE job_id = $1 AND gate = $2",
                    )
                    .bind(job)
                    .bind(gate)
                    .fetch_optional(&mut *c)
                    .await?;
                    Ok(row.map(|x| {
                        json!({
                            "threshold_value": x.threshold_value,
                            "rule": x.rule,
                            "base_hash": x.base_hash,
                        })
                    }))
                }
            }
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let gate = field(r, "gate");
            match (option_of(r), present(v)) {
                (Some(option), None) => {
                    sqlx::query(
                        "DELETE FROM job_gate_option_refinement \
                          WHERE job_id = $1 AND gate = $2 AND option = $3",
                    )
                    .bind(job)
                    .bind(gate)
                    .bind(option)
                    .execute(&mut *c)
                    .await?;
                }
                (None, None) => {
                    sqlx::query(
                        "DELETE FROM job_gate_rule_refinement WHERE job_id = $1 AND gate = $2",
                    )
                    .bind(job)
                    .bind(gate)
                    .execute(&mut *c)
                    .await?;
                }
                (Some(option), Some(x)) => {
                    let o: OptionRefinement = decode("gate", x)?;
                    sqlx::query(
                        "INSERT INTO job_gate_option_refinement \
                                (job_id, gate, option, label, description, hidden, base_hash, updated_by) \
                         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                         ON CONFLICT (tenant_id, job_id, gate, option) DO UPDATE \
                            SET label = EXCLUDED.label, description = EXCLUDED.description, \
                                hidden = EXCLUDED.hidden, base_hash = EXCLUDED.base_hash, \
                                updated_at = clock_timestamp()",
                    )
                    .bind(job)
                    .bind(gate)
                    .bind(option)
                    .bind(&o.label)
                    .bind(&o.description)
                    .bind(o.hidden)
                    .bind(&o.base_hash)
                    .bind(SET_BY)
                    .execute(&mut *c)
                    .await?;
                }
                (None, Some(x)) => {
                    let t: RuleRefinement = decode("gate", x)?;
                    if t.threshold_value.is_some() && t.rule.as_ref().is_some_and(|x| !x.is_null())
                    {
                        return Err(internal(format!(
                            "gate value holds both a threshold and a rule: {x}"
                        )));
                    }
                    let rule = t.rule.filter(|x| !x.is_null());
                    sqlx::query(
                        "INSERT INTO job_gate_rule_refinement \
                                (job_id, gate, threshold_value, rule, base_hash, updated_by) \
                         VALUES ($1, $2, $3::float8, $4, $5, $6) \
                         ON CONFLICT (tenant_id, job_id, gate) DO UPDATE \
                            SET threshold_value = EXCLUDED.threshold_value, rule = EXCLUDED.rule, \
                                base_hash = EXCLUDED.base_hash, updated_at = clock_timestamp()",
                    )
                    .bind(job)
                    .bind(gate)
                    .bind(t.threshold_value)
                    .bind(rule)
                    .bind(&t.base_hash)
                    .bind(SET_BY)
                    .execute(&mut *c)
                    .await?;
                }
            }
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let rows: Vec<(String, Option<String>)> = sqlx::query_as(
                "SELECT gate, option FROM job_gate_option_refinement WHERE job_id = $1 \
                 UNION ALL SELECT gate, NULL FROM job_gate_rule_refinement WHERE job_id = $1 \
                 ORDER BY 1, 2 NULLS FIRST",
            )
            .bind(job)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                rows.iter()
                    .map(|(g, o)| gate_ref(g, o.as_deref()))
                    .collect(),
            ))
        })
    }
    /// Fixture: `[label ?? null, description ?? null, !!hidden, threshold_value ?? null, rule ?? null]`.
    fn proj(&self, v: &Snap) -> Value {
        let Some(x) = present(v) else {
            return Value::Null;
        };
        let pick = |f: &str| x.get(f).cloned().unwrap_or(Value::Null);
        json!([
            pick("label"),
            pick("description"),
            x.get("hidden").and_then(Value::as_bool).unwrap_or(false),
            pick("threshold_value"),
            pick("rule"),
        ])
    }
    fn text(&self, v: &Snap, r: &DraftRef) -> Option<String> {
        let Some(x) = present(v) else {
            return Some("template value".to_string());
        };
        if option_of(r).is_none() {
            if let Some(rule) = x.get("rule").filter(|r| !r.is_null()) {
                let when = rule.get("fire_when").map(js_string).unwrap_or_default();
                let what = if when == "in" {
                    rule.get("fire_values")
                        .and_then(Value::as_array)
                        .map(|xs| xs.iter().map(js_string).collect::<Vec<_>>().join(", "))
                        .unwrap_or_default()
                } else {
                    rule.get("threshold").map(js_string).unwrap_or_default()
                };
                return Some(format!("fires when {when} {what}"));
            }
            return Some(match x.get("threshold_value").filter(|t| !t.is_null()) {
                Some(t) => format!("threshold {}", js_string(t)),
                None => "template value".to_string(),
            });
        }
        let mut parts = Vec::new();
        if let Some(l) = x
            .get("label")
            .and_then(Value::as_str)
            .filter(|l| !l.is_empty())
        {
            parts.push(format!("“{l}”"));
        }
        if x.get("hidden").and_then(Value::as_bool).unwrap_or(false) {
            parts.push("hidden".to_string());
        }
        Some(if parts.is_empty() {
            "described".to_string()
        } else {
            parts.join(" · ")
        })
    }
}

// ---- gate_question --------------------------------------------------------------------------------------------------

/// A tenant question as its place stores it (`key` = the ref's `gate`).
#[derive(Debug, Deserialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
struct Question {
    key: String,
    point: String,
    #[serde(rename = "type", default = "noul")]
    #[sqlx(rename = "type")]
    kind: String,
    question: String,
    options: Value,
    #[serde(default)]
    threshold: Option<String>,
    #[serde(default)]
    threshold_value: Option<f64>,
    fail: String,
    #[serde(default)]
    effect: Option<String>,
    rule: Value,
    action: String,
    #[serde(default)]
    tools: Option<Vec<String>>,
    #[serde(default)]
    customer_reply: Option<String>,
}

fn noul() -> String {
    "noul".to_owned()
}

impl Question {
    fn value(self) -> Value {
        let mut v = json!({
            "key": self.key,
            "point": self.point,
            "type": self.kind,
            "question": self.question,
            "options": self.options,
            "threshold": self.threshold,
            "threshold_value": self.threshold_value,
            "fail": self.fail,
            "effect": self.effect,
            "rule": self.rule,
            "action": self.action,
            "tools": self.tools,
        });
        if let (Some(reply), Some(m)) = (self.customer_reply, v.as_object_mut()) {
            m.insert("customer_reply".to_owned(), Value::String(reply));
        }
        v
    }
}

/// A question the tenant added to the job (absent = not asked).
pub struct GateQuestion;

impl DesignTarget for GateQuestion {
    fn kind(&self) -> &'static str {
        "gate_question"
    }
    fn scope(&self) -> Scope {
        Scope::Job
    }
    fn key(&self, r: &DraftRef) -> String {
        format!("gate-question:{}", field(r, "gate"))
    }
    fn get<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
    ) -> BoxFuture<'a, ApiResult<Snap>> {
        Box::pin(async move {
            let row = sqlx::query_as::<_, Question>(
                "SELECT key, point, type, question, options, threshold, \
                        threshold_value::float8 AS threshold_value, fail, effect, rule, action, tools, \
                        customer_reply \
                   FROM job_gate_question WHERE job_id = $1 AND key = $2",
            )
            .bind(job_of(at)?)
            .bind(field(r, "gate"))
            .fetch_optional(&mut *c)
            .await?;
            Ok(row.map(Question::value))
        })
    }
    fn set<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
        r: &'a DraftRef,
        v: &'a Snap,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let job = job_of(at)?;
            let gate = field(r, "gate");
            let Some(x) = present(v) else {
                sqlx::query("DELETE FROM job_gate_question WHERE job_id = $1 AND key = $2")
                    .bind(job)
                    .bind(gate)
                    .execute(&mut *c)
                    .await?;
                return Ok(());
            };
            let q: Question = decode("gate_question", x)?;
            sqlx::query(
                "INSERT INTO job_gate_question \
                        (job_id, key, point, type, question, options, threshold, threshold_value, fail, effect, \
                         rule, action, tools, customer_reply, updated_by) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::float8, $9, $10, $11, $12, $13, $14, $15) \
                 ON CONFLICT (tenant_id, job_id, key) DO UPDATE \
                    SET point = EXCLUDED.point, type = EXCLUDED.type, question = EXCLUDED.question, \
                        options = EXCLUDED.options, threshold = EXCLUDED.threshold, \
                        threshold_value = EXCLUDED.threshold_value, fail = EXCLUDED.fail, \
                        effect = EXCLUDED.effect, rule = EXCLUDED.rule, action = EXCLUDED.action, \
                        tools = EXCLUDED.tools, customer_reply = EXCLUDED.customer_reply, \
                        updated_at = clock_timestamp()",
            )
            .bind(job)
            .bind(gate)
            .bind(&q.point)
            .bind(&q.kind)
            .bind(&q.question)
            .bind(&q.options)
            .bind(&q.threshold)
            .bind(q.threshold_value)
            .bind(&q.fail)
            .bind(&q.effect)
            .bind(&q.rule)
            .bind(&q.action)
            .bind(&q.tools)
            .bind(&q.customer_reply)
            .bind(SET_BY)
            .execute(&mut *c)
            .await?;
            Ok(())
        })
    }
    fn places<'a>(
        &'a self,
        c: &'a mut PgConnection,
        at: Place<'a>,
    ) -> BoxFuture<'a, ApiResult<Option<Vec<DraftRef>>>> {
        Box::pin(async move {
            let keys: Vec<String> = sqlx::query_scalar(
                "SELECT key FROM job_gate_question WHERE job_id = $1 ORDER BY key",
            )
            .bind(job_of(at)?)
            .fetch_all(&mut *c)
            .await?;
            Ok(Some(
                keys.iter()
                    .map(|k| super::place_ref(&[("gate", k.as_str())]))
                    .collect(),
            ))
        })
    }
    /// Fixture: `${key} “${question}”`, else `not asked`.
    fn text(&self, v: &Snap, _r: &DraftRef) -> Option<String> {
        Some(match present(v) {
            Some(q) => format!(
                "{} “{}”",
                q.get("key").map(js_string).unwrap_or_default(),
                q.get("question").map(js_string).unwrap_or_default()
            ),
            None => "not asked".to_string(),
        })
    }
}
