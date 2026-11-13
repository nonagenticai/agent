//! Business rules (B2 T9, J9; spec B2 §4): the template's rule slots merged with the job's values. A port of the
//! fixture `businessRules` / `saveBusinessRule` / `linkRulePassage` / `rulePassages` / `evaluateRule` and `stripRule`
//! (`web/src/lib/api/areas/release.ts`).
//!
//! - The slots come from the bundle the job's DRAFT selection compiles to ([`draft_slots`]); a job with no selection,
//!   or one whose selection does not compile, has none. A stored `business_rule` row without a slot is not shown.
//! - Values and passage are design places (`rule_values` / `rule_passage`, crate::release::targets): a save goes
//!   through `design::track` (D54: the draft, never live on save); [`BusinessRule::live_values`] shows the published
//!   values while the draft differs from them.
//! - `history` = `business_rule_history`, newest first; `decisions` stay `[]` until B4 records `rule.evaluate` case
//!   events; [`passages`] is `[]` until B3 lists the policy passages (§7).
//! - [`statuses`] is the readiness view (T10's rules contributor): each slot's unset values in a design.
//!
//! Every function runs in the caller's job-scoped transaction, after `design::ensure_job`.
use std::collections::BTreeMap;

use agent_model::compile::Selection;
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::bundle::RuleSlot;
use super::compile::{CompileError, TemplateCompiler};
use super::rules_eval::{self, RuleEvaluation};
use crate::design::hooks::is_template_refusal;
use crate::design::{self, DesignOf, DraftRef, DraftWrite, JobRef, Label, Registry, Snap};
use crate::error::{ApiError, ApiResult};
use crate::platform::templates::TemplatePin;
use crate::tenancy::JobCtx;

/// The units whose value is a whole number 1–3650 (fixture `saveBusinessRule`).
pub const NUMERIC_UNITS: [&str; 3] = ["days", "months", "EUR"];
/// The longest value, in UTF-16 code units (JavaScript `length`).
pub const MAX_VALUE_LEN: usize = 280;

fn internal(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(what))
}

fn iso(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn rule_ref(id: &str) -> DraftRef {
    let mut m = DraftRef::new();
    m.insert("id".to_string(), Value::String(id.to_string()));
    m
}

fn rule_not_found(id: &str) -> ApiError {
    ApiError::Missing("rule_not_found", format!("No rule {id}."))
}

fn unknown_value(id: &str, k: &str) -> ApiError {
    ApiError::Unprocessable("unknown_value", format!("{id} has no value “{k}”."))
}

/// The rule slots of the bundle the job's DRAFT selection compiles to (fixture `w.release.rules` of the job). None
/// when no selection is chosen, or when it does not compile (422 `compile_failed`, or a template refusal such as 404
/// `profile_not_found`); any other error propagates.
pub(crate) async fn template_draft_slots(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
) -> ApiResult<Vec<RuleSlot>> {
    let stored: Option<Option<Value>> =
        sqlx::query_scalar("SELECT selection FROM job_target WHERE job_id = $1")
            .bind(job.id)
            .fetch_optional(&mut *c)
            .await?;
    let Some(sel) = stored.flatten() else {
        return Ok(Vec::new());
    };
    let sel: Selection = serde_json::from_value(sel)
        .map_err(|e| internal(format!("job_target.selection is unreadable: {e}")))?;
    let pins: Value = sqlx::query_scalar("SELECT pins FROM jobs WHERE id = $1")
        .bind(job.id)
        .fetch_one(&mut *c)
        .await?;
    let pins: Vec<TemplatePin> = serde_json::from_value(pins)
        .map_err(|e| internal(format!("jobs.pins is unreadable: {e}")))?;
    match compiler.compile(&mut *c, &sel, &pins).await {
        Ok(compiled) => {
            let mut slots: Vec<RuleSlot> = Vec::new();
            for r in compiled.bundle.rules {
                if !slots.iter().any(|s| s.id == r.id) {
                    slots.push(r);
                }
            }
            Ok(slots)
        }
        Err(CompileError::Api(e)) if !is_template_refusal(&e) => Err(*e),
        Err(_) => Ok(Vec::new()),
    }
}

pub async fn draft_slots(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
) -> ApiResult<Vec<RuleSlot>> {
    let mut slots = template_draft_slots(&mut *c, compiler, job).await?;
    super::rule_authoring::merge(&mut slots, super::rule_authoring::working(c, job).await?)?;
    Ok(slots)
}

/// The slot `id` of the job's draft bundle, else 404 `rule_not_found`.
async fn slot_of(
    c: &mut PgConnection,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    id: &str,
) -> ApiResult<RuleSlot> {
    draft_slots(c, compiler, job)
        .await?
        .into_iter()
        .find(|s| s.id == id)
        .ok_or_else(|| rule_not_found(id))
}

/// A `rule_values` value as the slot's values, in slot order (`None` = not set). A slot the value does not name, or
/// names with a non-string, is not set.
pub fn slot_values(slot: &RuleSlot, v: &Snap) -> Vec<(String, Option<String>)> {
    let m = v.as_ref().and_then(Value::as_object);
    slot.values
        .iter()
        .map(|x| {
            let val = m
                .and_then(|m| m.get(&x.key))
                .and_then(Value::as_str)
                .map(str::to_owned);
            (x.key.clone(), val)
        })
        .collect()
}

/// One rule's readiness (10 §5.2 step 7): which of its values are unset in the design read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleStatus {
    pub id: String,
    pub label: String,
    /// Any value unset (fixture `BusinessRule.placeholder`).
    pub placeholder: bool,
    /// The unset value keys, in slot order.
    pub unset: Vec<String>,
}

/// The readiness view of `slots` in the job's design `of` (T10's rules contributor: placeholders → `rule_placeholder`
/// go-live gaps). `slots` are the rule slots of what is checked: [`draft_slots`] for the draft, the release bundle's
/// `rules` for a release.
pub async fn statuses(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    of: &DesignOf,
    slots: &[RuleSlot],
) -> ApiResult<Vec<RuleStatus>> {
    let mut out = Vec::with_capacity(slots.len());
    for slot in slots {
        let v = design::value(&mut *c, reg, job, of, "rule_values", &rule_ref(&slot.id)).await?;
        let unset: Vec<String> = slot_values(slot, &v)
            .into_iter()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k)
            .collect();
        out.push(RuleStatus {
            id: slot.id.clone(),
            label: slot.label.clone(),
            placeholder: !unset.is_empty(),
            unset,
        });
    }
    Ok(out)
}

#[derive(sqlx::FromRow)]
struct RuleRow {
    updated_by: String,
    updated_at: DateTime<Utc>,
}

#[derive(sqlx::FromRow)]
struct HistoryRow {
    at: DateTime<Utc>,
    by_sub: String,
    change: String,
}

/// The job's current release id (fixture `slot_release`: the applied release that carries the slot).
async fn current_release(c: &mut PgConnection, job: &JobRef) -> ApiResult<Option<String>> {
    Ok(
        sqlx::query_scalar("SELECT current_release_id FROM jobs WHERE id = $1")
            .bind(job.id)
            .fetch_one(&mut *c)
            .await?,
    )
}

/// One `BusinessRule` (fixture `stripRule`): the slot, the draft values, the live values while they differ, the
/// passage, who changed it last, the history.
pub(crate) async fn view(
    c: &mut PgConnection,
    reg: &Registry,
    job: &JobRef,
    slot: &RuleSlot,
    slot_release: Option<&str>,
) -> ApiResult<Value> {
    let r = rule_ref(&slot.id);
    let draft = design::value(&mut *c, reg, job, &DesignOf::Draft, "rule_values", &r).await?;
    let live = design::value(&mut *c, reg, job, &DesignOf::Live, "rule_values", &r).await?;
    let t = reg.target("rule_values")?;
    // fixture: a rule always has a live value, every slot named (`null` = not set); here no row = `None`, and a
    // stored value omits unset keys (`write_values`), so both are rendered as the slot's full map
    let live_values: Option<Value> = (!design::same(t, &live, &draft)).then(|| {
        Value::Object(
            slot_values(slot, &live)
                .into_iter()
                .map(|(k, v)| (k, v.map_or(Value::Null, Value::String)))
                .collect(),
        )
    });
    let current = slot_values(slot, &draft);
    let values: Vec<Value> = slot
        .values
        .iter()
        .zip(&current)
        .map(|(x, (_, v))| {
            json!({
                "key": x.key,
                "label": x.label,
                "unit": x.unit,
                "value": v,
                "hint": x.hint,
                "suggested": x.suggested,
            })
        })
        .collect();
    let row = sqlx::query_as::<_, RuleRow>(
        "SELECT updated_by, updated_at FROM (SELECT updated_by, updated_at FROM business_rule WHERE job_id = $1 AND rule_id = $2 UNION ALL SELECT updated_by, updated_at FROM job_overlay_row WHERE job_id = $1 AND object = 'rule_slot' AND key = $2) changes ORDER BY updated_at DESC LIMIT 1",
    )
    .bind(job.id)
    .bind(&slot.id)
    .fetch_optional(&mut *c)
    .await?;
    let history = sqlx::query_as::<_, HistoryRow>(
        "SELECT at, by_sub, change FROM business_rule_history WHERE job_id = $1 AND rule_id = $2 \
          ORDER BY id DESC",
    )
    .bind(job.id)
    .bind(&slot.id)
    .fetch_all(&mut *c)
    .await?;
    let mut subs: Vec<String> = history.iter().map(|h| h.by_sub.clone()).collect();
    if let Some(row) = &row {
        subs.push(row.updated_by.clone());
    }
    subs.sort();
    subs.dedup();
    let who = crate::members::names::lookup(&mut *c, &subs).await?;
    let name = |s: &str| who.get(s).cloned().unwrap_or_else(|| s.to_string());
    let history: Vec<Value> = history
        .iter()
        .map(|h| {
            json!({
                "at": iso(h.at),
                "by": h.by_sub,
                "by_name": name(&h.by_sub),
                "change": h.change,
            })
        })
        .collect();
    let passage = design::value(&mut *c, reg, job, &DesignOf::Draft, "rule_passage", &r).await?;
    let mut out = json!({
        "id": slot.id,
        "label": slot.label,
        "applies_to": slot.applies_to,
        "inputs": slot.inputs,
        "outcome": slot.outcome,
        "values": values,
        "placeholder": current.iter().any(|(_, v)| v.is_none()),
        "passage": passage.unwrap_or(Value::Null),
        "updated_by": row.as_ref().map(|x| x.updated_by.clone()),
        "updated_at": row.as_ref().map(|x| iso(x.updated_at)),
        "slot_release": slot_release,
        "uses": slot.uses,
        "outcomes": slot.outcomes,
        "facts": slot.facts,
        "examples": slot.examples,
        "evaluation": slot.evaluation,
        "tenant_authored": slot.slot_layer.as_deref() == Some("tenant"),
        "history": history,
        "decisions": [],
    });
    let o = out
        .as_object_mut()
        .ok_or_else(|| internal("a rule view is an object".into()))?;
    if let Some(row) = &row {
        o.insert(
            "updated_by_name".to_string(),
            Value::String(name(&row.updated_by)),
        );
    }
    if let Some(s) = &slot.statement {
        o.insert("statement".to_string(), Value::String(s.clone()));
    }
    if let Some(s) = &slot.slot_layer {
        o.insert("slot_layer".to_string(), Value::String(s.clone()));
    }
    if let Some(lv) = live_values {
        o.insert("live_values".to_string(), lv);
    }
    Ok(out)
}

/// `GET /api/jobs/{job}/rules`: every rule slot of the draft bundle with the job's values (fixture `businessRules`).
pub async fn list(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
) -> ApiResult<Vec<Value>> {
    let slots = draft_slots(&mut *c, compiler, job).await?;
    let release = current_release(&mut *c, job).await?;
    let mut out = Vec::with_capacity(slots.len());
    for slot in &slots {
        out.push(view(&mut *c, reg, job, slot, release.as_deref()).await?);
    }
    Ok(out)
}

/// `GET /api/jobs/{job}/rules/passages`: the policy passages a rule may cite. `[]` until B3 lists them from the
/// documents of the stores bound to `policy.*` knowledge kinds (spec B2 §7).
pub async fn passages(_c: &mut PgConnection, _job: &JobRef) -> ApiResult<Vec<Value>> {
    Ok(Vec::new())
}

/// Check one value against its slot (fixture `saveBusinessRule`): a whole number 1–3650 for `days`/`months`/`EUR`,
/// at most 280 characters. A blank value means "not set" and is not checked.
fn check_value(slot_label: &str, unit: &str, v: &str) -> ApiResult<()> {
    if rules_eval::js_trim(v).is_empty() {
        return Ok(());
    }
    if unit == "number" && v.trim().parse::<f64>().ok().is_none_or(|n| !n.is_finite()) {
        return Err(ApiError::Unprocessable(
            "invalid_value",
            format!("{slot_label} must be a finite number."),
        ));
    }
    if NUMERIC_UNITS.contains(&unit) {
        let n = rules_eval::js_number(v);
        if !(n.is_finite() && n == n.trunc() && (1.0..=3650.0).contains(&n)) {
            return Err(ApiError::Unprocessable(
                "invalid_value",
                format!("{slot_label} must be a whole number of {unit} (1–3650)."),
            ));
        }
    }
    if v.encode_utf16().count() > MAX_VALUE_LEN {
        return Err(ApiError::Unprocessable(
            "invalid_value",
            format!("{slot_label} is at most {MAX_VALUE_LEN} characters."),
        ));
    }
    Ok(())
}

fn shown(v: &Option<String>) -> &str {
    v.as_deref().unwrap_or("not set")
}

/// `{label}: {from} → {to}` for every value of `slot` that differs between `from` and `to` (both in slot order).
fn diffs(
    slot: &RuleSlot,
    from: &[(String, Option<String>)],
    to: &[(String, Option<String>)],
) -> Vec<String> {
    slot.values
        .iter()
        .zip(from.iter().zip(to))
        .filter(|(_, ((_, f), (_, t)))| f != t)
        .map(|(x, ((_, f), (_, t)))| format!("{}: {} → {}", x.label, shown(f), shown(t)))
        .collect()
}

/// The ledger label of a values save: what the draft `after` changes against `live`.
fn ledger_label(
    slot: &RuleSlot,
    live: &[(String, Option<String>)],
    after: &[(String, Option<String>)],
) -> String {
    let vs = diffs(slot, live, after);
    if vs.is_empty() {
        format!("{}: back to the live values", slot.label)
    } else {
        format!("{}: {}", slot.label, vs.join("; "))
    }
}

/// Write the rule's values (the `rule_values` working row; run inside `design::track`). All values unset = no values:
/// the row's `slot_values` is emptied (a row with no passage goes), so a draft put back to "nothing set" equals a
/// live design that never had a row.
async fn write_values(
    c: &mut PgConnection,
    job: Uuid,
    rule: String,
    vals: Option<Value>,
    by: String,
) -> ApiResult<()> {
    match vals {
        Some(v) => {
            sqlx::query(
                "INSERT INTO business_rule (job_id, rule_id, slot_values, updated_by) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (tenant_id, job_id, rule_id) DO UPDATE \
                    SET slot_values = EXCLUDED.slot_values, updated_by = EXCLUDED.updated_by, \
                        updated_at = clock_timestamp()",
            )
            .bind(job)
            .bind(&rule)
            .bind(v)
            .bind(&by)
            .execute(&mut *c)
            .await?;
        }
        None => {
            sqlx::query(
                "DELETE FROM business_rule WHERE job_id = $1 AND rule_id = $2 AND passage IS NULL",
            )
            .bind(job)
            .bind(&rule)
            .execute(&mut *c)
            .await?;
            sqlx::query(
                "UPDATE business_rule SET slot_values = '{}', updated_by = $3, updated_at = clock_timestamp() \
                  WHERE job_id = $1 AND rule_id = $2",
            )
            .bind(job)
            .bind(&rule)
            .bind(&by)
            .execute(&mut *c)
            .await?;
        }
    }
    Ok(())
}

/// Write the rule's passage (the `rule_passage` working row; run inside `design::track`). `None` unlinks it (a row
/// with no values goes).
async fn write_passage(
    c: &mut PgConnection,
    job: Uuid,
    rule: String,
    passage: Option<Value>,
    by: String,
) -> ApiResult<()> {
    match passage {
        Some(p) => {
            sqlx::query(
                "INSERT INTO business_rule (job_id, rule_id, passage, updated_by) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT (tenant_id, job_id, rule_id) DO UPDATE \
                    SET passage = EXCLUDED.passage, updated_by = EXCLUDED.updated_by, \
                        updated_at = clock_timestamp()",
            )
            .bind(job)
            .bind(&rule)
            .bind(p)
            .bind(&by)
            .execute(&mut *c)
            .await?;
        }
        None => {
            sqlx::query(
                "DELETE FROM business_rule WHERE job_id = $1 AND rule_id = $2 AND slot_values = '{}'",
            )
            .bind(job)
            .bind(&rule)
            .execute(&mut *c)
            .await?;
            sqlx::query(
                "UPDATE business_rule SET passage = NULL, updated_by = $3, updated_at = clock_timestamp() \
                  WHERE job_id = $1 AND rule_id = $2",
            )
            .bind(job)
            .bind(&rule)
            .bind(&by)
            .execute(&mut *c)
            .await?;
        }
    }
    Ok(())
}

async fn add_history(
    c: &mut PgConnection,
    job: &JobRef,
    rule: &str,
    by: &str,
    change: &str,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO business_rule_history (job_id, rule_id, by_sub, change) VALUES ($1, $2, $3, $4)",
    )
    .bind(job.id)
    .bind(rule)
    .bind(by)
    .bind(change)
    .execute(&mut *c)
    .await?;
    Ok(())
}

/// Lock the job row (`FOR UPDATE`, as `design::track` does) so a read-then-write of a rule is not interleaved with
/// another (T9 review I1).
async fn lock_job(c: &mut PgConnection, job: &JobRef) -> ApiResult<()> {
    sqlx::query("SELECT 1 FROM jobs WHERE id = $1 FOR UPDATE")
        .bind(job.id)
        .execute(&mut *c)
        .await?;
    Ok(())
}

fn job_ref(j: &JobCtx) -> JobRef {
    JobRef {
        id: j.job_id,
        key: j.job_key.clone(),
    }
}

/// `PUT /api/jobs/{job}/rules/{id}` `{values}` (fixture `saveBusinessRule`): 404 `rule_not_found`, 422
/// `unknown_value` / `invalid_value` before any write. A blank value = not set; others are stored trimmed. When a
/// value changes: one tracked `rule_values` write (area `rules`, page J9; label = what changed against the live
/// values) and a history row. Returns the rule.
pub async fn save(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    id: &str,
    next: &BTreeMap<String, Option<String>>,
) -> ApiResult<Value> {
    let job = job_ref(j);
    let slot = slot_of(&mut *c, compiler, &job, id).await?;
    for (k, v) in next {
        let Some(x) = slot.values.iter().find(|x| x.key == *k) else {
            return Err(unknown_value(id, k));
        };
        if let Some(v) = v {
            check_value(&x.label, &x.unit, v)?;
        }
    }
    // Lock the job before reading the draft (T9 review I1): `after` is the whole value map, so two saves of different
    // values of one rule must not both start from the same `before`. `design::track` re-locks the same row (free).
    lock_job(&mut *c, &job).await?;
    let r = rule_ref(id);
    let draft = design::value(&mut *c, reg, &job, &DesignOf::Draft, "rule_values", &r).await?;
    let before = slot_values(&slot, &draft);
    let after: Vec<(String, Option<String>)> = before
        .iter()
        .map(|(k, b)| {
            let v = match next.get(k) {
                Some(n) => n
                    .as_deref()
                    .map(rules_eval::js_trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned),
                None => b.clone(),
            };
            (k.clone(), v)
        })
        .collect();
    let changed = diffs(&slot, &before, &after);
    if !changed.is_empty() {
        let live = design::value(&mut *c, reg, &job, &DesignOf::Live, "rule_values", &r).await?;
        let label = ledger_label(&slot, &slot_values(&slot, &live), &after);
        // only the set values are stored (review M7): `{"a": "30"}` and `{"a": "30", "b": null}` mean the same, and
        // a stored `null` would make a save-then-revert differ from live under the target's `proj`
        let set: Map<String, Value> = after
            .iter()
            .filter_map(|(k, v)| v.clone().map(|v| (k.clone(), Value::String(v))))
            .collect();
        let vals: Option<Value> = (!set.is_empty()).then_some(Value::Object(set));
        let w = DraftWrite {
            area: "rules",
            page: "J9",
            kind: "rule_values",
            r#ref: r.clone(),
            label: Label::Now(label),
        };
        let (job_id, rule, by) = (job.id, id.to_string(), j.t.caller.sub.clone());
        design::track::<(), _>(&mut *c, reg, j, w, move |c| {
            Box::pin(write_values(c, job_id, rule, vals, by))
        })
        .await?;
        add_history(&mut *c, &job, id, &j.t.caller.sub, &changed.join("; ")).await?;
    }
    let release = current_release(&mut *c, &job).await?;
    view(&mut *c, reg, &job, &slot, release.as_deref()).await
}

/// `PUT /api/jobs/{job}/rules/{id}/passage` `{passage_id}` (fixture `linkRulePassage`): 404 `rule_not_found`,
/// `passage_not_found` (the id is not one of [`passages`]); `null` unlinks. One tracked `rule_passage` write and a
/// history row. Returns the rule.
pub async fn link_passage(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    j: &JobCtx,
    id: &str,
    passage_id: Option<&str>,
) -> ApiResult<Value> {
    let job = job_ref(j);
    let slot = slot_of(&mut *c, compiler, &job, id).await?;
    lock_job(&mut *c, &job).await?;
    let passage: Option<Value> = match passage_id {
        None => None,
        Some(pid) => Some(
            passages(&mut *c, &job)
                .await?
                .into_iter()
                .find(|p| p.get("id").and_then(Value::as_str) == Some(pid))
                .ok_or_else(|| {
                    ApiError::Missing("passage_not_found", format!("No policy passage {pid}."))
                })?,
        ),
    };
    let title: Option<String> = passage.as_ref().map(|p| {
        p.get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    });
    let (label, change) = match &title {
        Some(t) => (format!("{}: cites {t}", slot.label), format!("Linked {t}")),
        None => (
            format!("{}: policy passage unlinked", slot.label),
            "Unlinked the policy passage".to_string(),
        ),
    };
    let w = DraftWrite {
        area: "rules",
        page: "J9",
        kind: "rule_passage",
        r#ref: rule_ref(id),
        label: Label::Now(label),
    };
    let (job_id, rule, by) = (job.id, id.to_string(), j.t.caller.sub.clone());
    design::track::<(), _>(&mut *c, reg, j, w, move |c| {
        Box::pin(write_passage(c, job_id, rule, passage, by))
    })
    .await?;
    add_history(&mut *c, &job, id, &j.t.caller.sub, &change).await?;
    let release = current_release(&mut *c, &job).await?;
    view(&mut *c, reg, &job, &slot, release.as_deref()).await
}

/// The J9 dry-run input (`POST /api/jobs/{job}/rules/{id}/evaluate` body; fixture `evaluateRule`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DryRun {
    /// The facts the rule reads (`order.delivered_at` → `2026-10-09`).
    #[serde(default)]
    pub facts: BTreeMap<String, String>,
    /// Values laid over the job's draft values (`null` = not set).
    #[serde(default)]
    pub values: Option<BTreeMap<String, Option<String>>>,
    /// The day to evaluate on (`YYYY-MM-DD…`); default today (UTC).
    #[serde(default)]
    pub as_of: Option<String>,
}

/// `POST /api/jobs/{job}/rules/{id}/evaluate` (fixture `evaluateRule`): a dry run of the built-in `rule.evaluate`
/// over the draft values with `input.values` laid over them (422 `unknown_value` for a key the rule has no slot
/// for), on day `as_of` (default: today, UTC). Writes no design value, ledger row or history (review M9: like every
/// job route it runs `design::ensure_job` first, which can take a never-touched job's baseline).
pub async fn evaluate(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    job: &JobRef,
    id: &str,
    input: &DryRun,
) -> ApiResult<RuleEvaluation> {
    let slot = slot_of(&mut *c, compiler, job, id).await?;
    let draft = design::value(
        &mut *c,
        reg,
        job,
        &DesignOf::Draft,
        "rule_values",
        &rule_ref(id),
    )
    .await?;
    let current = slot_values(&slot, &draft);
    let none = BTreeMap::new();
    let over = input.values.as_ref().unwrap_or(&none);
    if let Some(k) = over
        .keys()
        .find(|k| !current.iter().any(|(key, _)| key == *k))
    {
        return Err(unknown_value(id, k));
    }
    let values: Vec<(String, Option<String>)> = current
        .into_iter()
        .map(|(k, v)| match over.get(&k) {
            Some(o) => (k, o.clone()),
            None => (k, v),
        })
        .collect();
    let today = match input.as_of.as_deref() {
        Some(d) => d.to_string(),
        None => Utc::now().format("%Y-%m-%d").to_string(),
    };
    Ok(rules_eval::evaluate_definition(
        id,
        slot.evaluation.as_ref(),
        &input.facts,
        &values,
        &today,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> RuleSlot {
        serde_json::from_value(json!({
            "id": "RET-01",
            "label": "Return window",
            "applies_to": [],
            "inputs": [],
            "outcome": "eligible",
            "values": [
                {"key": "window_days", "label": "Return window", "unit": "days", "hint": ""},
                {"key": "excluded_categories", "label": "Excluded categories", "unit": "text", "hint": ""},
            ],
        }))
        .expect("a RuleSlot")
    }

    fn vals(w: Option<&str>, x: Option<&str>) -> Vec<(String, Option<String>)> {
        vec![
            ("window_days".to_string(), w.map(str::to_owned)),
            ("excluded_categories".to_string(), x.map(str::to_owned)),
        ]
    }

    /// The ledger label names what the draft changes against LIVE, not against the previous draft; a draft equal
    /// to live says so (the API cannot show that label: the ledger drops the row, so it is pinned here).
    #[test]
    fn ledger_label_is_against_live() {
        let s = slot();
        let live = vals(Some("14"), None);
        assert_eq!(
            ledger_label(&s, &live, &vals(Some("30"), Some("rugs"))),
            "Return window: Return window: 14 → 30; Excluded categories: not set → rugs"
        );
        assert_eq!(
            ledger_label(&s, &live, &vals(Some("14"), None)),
            "Return window: back to the live values"
        );
        assert_eq!(
            diffs(&s, &vals(Some("30"), Some("rugs")), &vals(Some("14"), None)),
            [
                "Return window: 30 → 14",
                "Excluded categories: rugs → not set"
            ]
        );
    }
}
