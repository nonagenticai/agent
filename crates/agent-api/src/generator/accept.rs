//! B8 T10: what an accepted draft becomes. At `advanceGeneratorRun` of a PLATFORM run the step's live items (accepted
//! or renamed; a merged item is held by its target) are written as UNTAGGED revisions of the run's department layer
//! (`industry_dept/<industry>/<department>`) with author `generator:<run id>` (the grid reads that back as
//! `source_run`). Nothing is live: a job pinned to a tag compiles byte-identically until it re-pins (Review Focus 1).
//!
//! | step | items | row |
//! |---|---|---|
//! | journey | `journey_theme`, `journey_stage`, `activity` | the same-named overlay kinds; out-of-scope stages → `stage_scope` |
//! | objects | `activity_object` | the activity row, `objects[]` |
//! | operations | `operation_expectation` | the activity row, `operations[]` |
//! | use_cases | `use_case_card` | `use_case_card` |
//! | business_model, harmonise | advisory | nothing is written |
//!
//! An item whose row cannot be written (the activity it extends is not in the journey, the row is invalid, the key
//! was retired in the layer) is SKIPPED and reported in the step summary — never a failed advance. The writes run in
//! the caller's `agent_platform` transaction, one `layer.revision` audit row each (`write_revision`).
use std::collections::BTreeMap;

use agent_model::kinds::{Kind, Op};
use agent_model::layer::LayerKey;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use super::run::{RunRow, StageRow};
use super::store::ItemFull;
use crate::audit::Actor;
use crate::error::ApiResult;
use crate::journey::grid;
use crate::journey::model::{
    ActivityObject, GeneratorStepKey, IndustryJourney, JourneyActivity, OperationExpectation, Slug,
};
use crate::platform::db::authored::{Expect, current_revision, write_authored};
use crate::platform::db::layers;

static NULL: Value = Value::Null;

/// What a step wrote.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub written: usize,
    /// One line per item that could not be written.
    pub skipped: Vec<String>,
}

impl Outcome {
    fn merge(&mut self, o: Outcome) {
        self.written += o.written;
        self.skipped.extend(o.skipped);
    }
}

/// The department layer of the run.
pub fn dept_layer(run: &RunRow) -> String {
    LayerKey::industry_dept(&run.industry, &run.department).label()
}

/// The author of every revision a run writes.
pub fn author(run: i64) -> String {
    format!("generator:{run}")
}

fn live(i: &ItemFull) -> bool {
    matches!(i.decision.as_str(), "accepted" | "renamed")
}

fn payload(i: &ItemFull) -> &Value {
    i.payload.as_ref().unwrap_or(&NULL)
}

fn text(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// A snake_case key from `s`.
fn slug_from(s: &str) -> Option<String> {
    let mut out = String::new();
    for ch in s.trim().to_lowercase().chars() {
        let c = if ch.is_ascii_alphanumeric() { ch } else { '_' };
        if c == '_' && (out.is_empty() || out.ends_with('_')) {
            continue;
        }
        out.push(c);
    }
    // ASCII only by now: bytes are characters
    out.truncate(out.len().min(41));
    let out = out.trim_end_matches('_').to_owned();
    Slug::new(&out).map(|s| s.as_str().to_owned())
}

/// The row key of a theme or stage item: the payload's `key` when it is a slug, else a slug of the title.
fn key_of(i: &ItemFull) -> Option<String> {
    text(payload(i), "key")
        .filter(|k| Slug::new(k).is_some())
        .or_else(|| slug_from(&i.title))
}

/// The row key of an `activity` item, `stage:activity` (both slugs). A renamed item carries its key in `title`
/// (the rename writes the note there); otherwise the key is the payload's `key` (the prompts define `title` as a
/// short human name), and only a title that is itself a valid key stands in for a payload without one.
pub fn activity_key(decision: &str, title: &str, payload_key: Option<&str>) -> Option<String> {
    let valid = |k: &str| {
        k.split_once(':')
            .is_some_and(|(stage, slug)| Slug::new(stage).is_some() && Slug::new(slug).is_some())
    };
    let title = title.trim();
    if decision == "renamed" && valid(title) {
        return Some(title.to_owned());
    }
    payload_key
        .map(str::trim)
        .filter(|k| valid(k))
        .or_else(|| valid(title).then_some(title))
        .map(str::to_owned)
}

/// Copy the listed keys of `payload` that are present and not null.
fn pick(payload: &Value, keys: &[&str]) -> Map<String, Value> {
    let mut m = Map::new();
    for k in keys {
        if let Some(v) = payload.get(*k).filter(|v| !v.is_null()) {
            m.insert((*k).to_owned(), v.clone());
        }
    }
    m
}

/// The whole `activity` item of a grid activity (a patch sets the whole item, so a patch starts from this).
pub fn activity_fields(a: &JourneyActivity) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("label".into(), json!(a.label));
    if !a.trigger.is_empty() {
        m.insert("trigger".into(), json!(a.trigger));
    }
    m.insert(
        "actor".into(),
        serde_json::to_value(a.actor).unwrap_or(Value::Null),
    );
    if !a.also_in_stages.is_empty() {
        m.insert("also_in_stages".into(), json!(a.also_in_stages));
    }
    if let Some(area) = &a.area {
        m.insert("area".into(), json!(area));
    }
    if !a.objects.is_empty() {
        m.insert(
            "objects".into(),
            serde_json::to_value(&a.objects).unwrap_or(Value::Null),
        );
    }
    if !a.operations.is_empty() {
        m.insert(
            "operations".into(),
            serde_json::to_value(&a.operations).unwrap_or(Value::Null),
        );
    }
    m
}

/// How (`op`, `expect`) a write of (`kind`, `key`) in `layer` goes: a patch of this layer's own row; a patch with
/// no row of its own when a lower layer already defines the key (the catalogue folds a later patch over it); else an
/// add. `None` = the key was retired in this layer (retired keys are not reused).
async fn plan(
    conn: &mut PgConnection,
    layer: &layers::Layer,
    kind: Kind,
    key: &str,
) -> ApiResult<Option<(Op, Expect)>> {
    match current_revision(&mut *conn, layer, kind, key).await? {
        Some(c) if c.tombstone => Ok(None),
        Some(c) => Ok(Some((Op::Patch, Expect::Rev(c.rev)))),
        None => {
            let elsewhere: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM layer_head h WHERE h.kind = $1 AND h.key = $2 \
                                 AND h.op <> 'tombstone' AND h.layer_key <> $3)",
            )
            .bind(kind.as_str())
            .bind(key)
            .bind(&layer.key)
            .fetch_one(&mut *conn)
            .await?;
            Ok(Some((
                if elsewhere { Op::Patch } else { Op::Add },
                Expect::New,
            )))
        }
    }
}

/// Write one row. `Ok(true)` = written; `Ok(false)` = skipped, the reason pushed on `out.skipped`.
#[allow(clippy::too_many_arguments)]
async fn put(
    conn: &mut PgConnection,
    actor: &Actor,
    layer_key: &str,
    kind: Kind,
    key: &str,
    fields: Value,
    by: &str,
    out: &mut Outcome,
) -> ApiResult<bool> {
    let layer = layers::layer_by_key(&mut *conn, layer_key).await?;
    let Some((op, expect)) = plan(&mut *conn, &layer, kind, key).await? else {
        out.skipped
            .push(format!("{kind} {key}: retired in {layer_key}"));
        return Ok(false);
    };
    match write_authored(
        &mut *conn, actor, layer_key, kind, key, op, fields, by, expect,
    )
    .await
    {
        Ok(_) => {
            out.written += 1;
            Ok(true)
        }
        // refused before anything was written: the item is skipped, the advance goes on
        Err(e) if e.status_and_code().0 == axum::http::StatusCode::UNPROCESSABLE_ENTITY => {
            out.skipped.push(format!("{kind} {key}: {}", e.code()));
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// The accepted items of `step` as untagged revisions (module doc). `conn` is an `agent_platform` connection.
pub async fn write_step(
    conn: &mut PgConnection,
    actor: &Actor,
    run: &RunRow,
    step: GeneratorStepKey,
    items: &[ItemFull],
    stages: &[StageRow],
) -> ApiResult<Outcome> {
    let mine: Vec<&ItemFull> = items
        .iter()
        .filter(|i| i.step == step.as_str() && live(i))
        .collect();
    let mut out = Outcome::default();
    let layer = dept_layer(run);
    let by = author(run.id);
    match step {
        GeneratorStepKey::Journey => {
            let j = grid::load(&mut *conn, &run.industry, &run.department).await?;
            journey(&mut *conn, actor, &layer, &by, &j, &mine, &mut out).await?;
            out.merge(sync_scopes(&mut *conn, actor, run, stages).await?);
        }
        GeneratorStepKey::Objects => {
            let j = grid::load(&mut *conn, &run.industry, &run.department).await?;
            objects(&mut *conn, actor, &layer, &by, &j, &mine, &mut out).await?;
        }
        GeneratorStepKey::Operations => {
            let j = grid::load(&mut *conn, &run.industry, &run.department).await?;
            operations(&mut *conn, actor, &layer, &by, &j, &mine, &mut out).await?;
        }
        GeneratorStepKey::UseCases => {
            for it in mine.iter().filter(|i| i.kind == "use_case_card") {
                let p = payload(it);
                let key = text(p, "activity").unwrap_or_else(|| it.title.clone());
                let mut f = pick(
                    p,
                    &[
                        "summary",
                        "examples",
                        "metric",
                        "sample_dialogue",
                        "success_example",
                        "error_example",
                        "scenarios",
                    ],
                );
                if f.is_empty() {
                    out.skipped.push(format!("use_case_card {key}: empty card"));
                    continue;
                }
                f.insert("label".into(), json!(it.title));
                put(
                    &mut *conn,
                    actor,
                    &layer,
                    Kind::UseCaseCard,
                    &key,
                    Value::Object(f),
                    &by,
                    &mut out,
                )
                .await?;
            }
        }
        // business model and harmonise drafts are advisory; input / validate / package draft nothing
        _ => {}
    }
    Ok(out)
}

async fn journey(
    conn: &mut PgConnection,
    actor: &Actor,
    layer: &str,
    by: &str,
    j: &IndustryJourney,
    mine: &[&ItemFull],
    out: &mut Outcome,
) -> ApiResult<()> {
    for it in mine.iter().filter(|i| i.kind == "journey_theme") {
        let Some(key) = key_of(it) else {
            out.skipped
                .push(format!("journey_theme “{}”: no key", it.title));
            continue;
        };
        let p = payload(it);
        let mut f = pick(p, &["order", "escalation_weight"]);
        f.insert(
            "label".into(),
            json!(text(p, "label").unwrap_or_else(|| it.title.clone())),
        );
        put(
            conn,
            actor,
            layer,
            Kind::JourneyTheme,
            &key,
            Value::Object(f),
            by,
            out,
        )
        .await?;
    }
    for it in mine.iter().filter(|i| i.kind == "journey_stage") {
        let Some(key) = key_of(it) else {
            out.skipped
                .push(format!("journey_stage “{}”: no key", it.title));
            continue;
        };
        let p = payload(it);
        let mut f = pick(p, &["order", "description", "synonyms", "themes"]);
        f.insert(
            "label".into(),
            json!(text(p, "label").unwrap_or_else(|| it.title.clone())),
        );
        put(
            conn,
            actor,
            layer,
            Kind::JourneyStage,
            &key,
            Value::Object(f),
            by,
            out,
        )
        .await?;
    }
    for it in mine.iter().filter(|i| i.kind == "activity") {
        let p = payload(it);
        let Some(key) = activity_key(
            &it.decision,
            &it.title,
            p.get("key").and_then(Value::as_str),
        ) else {
            out.skipped
                .push(format!("activity “{}”: no stage:activity key", it.title));
            continue;
        };
        // the whole item: an activity the grid already has keeps its objects and operations
        let mut f = match j.activities.iter().find(|a| a.key == key) {
            Some(a) => activity_fields(a),
            None => Map::new(),
        };
        f.insert(
            "label".into(),
            json!(text(p, "label").unwrap_or_else(|| it.title.trim().to_owned())),
        );
        for (k, v) in pick(p, &["trigger", "actor", "also_in_stages", "area"]) {
            f.insert(k, v);
        }
        put(
            conn,
            actor,
            layer,
            Kind::Activity,
            &key,
            Value::Object(f),
            by,
            out,
        )
        .await?;
    }
    Ok(())
}

/// `object` of an `activity_object` item: the payload's, else the title's part after ` × `.
fn object_of(it: &ItemFull) -> Option<String> {
    text(payload(it), "object").or_else(|| {
        it.title
            .split_once(" × ")
            .map(|(_, o)| o.trim().to_owned())
            .filter(|o| !o.is_empty())
    })
}

/// `activity` of an `activity_object` / `operation_expectation` item: the payload's, else the title's part before ` × `.
fn activity_of(it: &ItemFull) -> String {
    text(payload(it), "activity").unwrap_or_else(|| {
        it.title
            .split(" × ")
            .next()
            .unwrap_or_default()
            .trim()
            .to_owned()
    })
}

fn group<'a>(mine: &[&'a ItemFull], kind: &str) -> BTreeMap<String, Vec<&'a ItemFull>> {
    let mut g: BTreeMap<String, Vec<&ItemFull>> = BTreeMap::new();
    for it in mine.iter().filter(|i| i.kind == kind) {
        g.entry(activity_of(it)).or_default().push(it);
    }
    g
}

async fn objects(
    conn: &mut PgConnection,
    actor: &Actor,
    layer: &str,
    by: &str,
    j: &IndustryJourney,
    mine: &[&ItemFull],
    out: &mut Outcome,
) -> ApiResult<()> {
    for (act, its) in group(mine, "activity_object") {
        let Some(a) = j.activities.iter().find(|a| a.key == act) else {
            out.skipped.push(format!(
                "activity_object: activity {act} is not in the journey"
            ));
            continue;
        };
        let mut objs: Vec<ActivityObject> = a.objects.clone();
        let mut added = false;
        for it in its {
            if it.new_object.is_some() && it.proposal != "approved" {
                out.skipped.push(format!(
                    "{}: object {} is not approved",
                    it.item_id,
                    it.new_object.as_deref().unwrap_or_default()
                ));
                continue;
            }
            let Some(object) = object_of(it) else {
                out.skipped.push(format!("{}: no object", it.item_id));
                continue;
            };
            let p = payload(it);
            let mut raw = pick(p, &["role", "fields", "required_inputs"]);
            raw.insert("object".into(), json!(object));
            raw.entry("role").or_insert(json!("subject"));
            raw.entry("fields").or_insert(json!([]));
            raw.entry("required_inputs").or_insert(json!([]));
            let Ok(bound) = serde_json::from_value::<ActivityObject>(Value::Object(raw)) else {
                out.skipped
                    .push(format!("{}: not an object binding", it.item_id));
                continue;
            };
            match objs.iter().position(|o| o.object == bound.object) {
                Some(i) => objs[i] = bound,
                None => objs.push(bound),
            }
            added = true;
        }
        if added {
            let mut f = activity_fields(a);
            f.insert(
                "objects".into(),
                serde_json::to_value(&objs).unwrap_or(Value::Null),
            );
            put(
                conn,
                actor,
                layer,
                Kind::Activity,
                &act,
                Value::Object(f),
                by,
                out,
            )
            .await?;
        }
    }
    Ok(())
}

async fn operations(
    conn: &mut PgConnection,
    actor: &Actor,
    layer: &str,
    by: &str,
    j: &IndustryJourney,
    mine: &[&ItemFull],
    out: &mut Outcome,
) -> ApiResult<()> {
    for (act, its) in group(mine, "operation_expectation") {
        let Some(a) = j.activities.iter().find(|a| a.key == act) else {
            out.skipped.push(format!(
                "operation_expectation: activity {act} is not in the journey"
            ));
            continue;
        };
        let mut ops: Vec<OperationExpectation> = a.operations.clone();
        let mut added = false;
        for it in its {
            if it.new_operation.is_some() && it.proposal != "approved" {
                out.skipped
                    .push(format!("{}: the new operation is not approved", it.item_id));
                continue;
            }
            let p = payload(it);
            let Some(op) = text(p, "op") else {
                out.skipped.push(format!("{}: no op", it.item_id));
                continue;
            };
            let variant = text(p, "variant");
            let required = p.get("required").and_then(Value::as_bool).unwrap_or(false);
            let e = OperationExpectation {
                op,
                variant,
                required,
            };
            match ops
                .iter()
                .position(|o| o.op == e.op && o.variant == e.variant)
            {
                Some(i) => ops[i] = e,
                None => ops.push(e),
            }
            added = true;
        }
        if added {
            let mut f = activity_fields(a);
            f.insert(
                "operations".into(),
                serde_json::to_value(&ops).unwrap_or(Value::Null),
            );
            put(
                conn,
                actor,
                layer,
                Kind::Activity,
                &act,
                Value::Object(f),
                by,
                out,
            )
            .await?;
        }
    }
    Ok(())
}

/// Make the department's `stage_scope` rows agree with the run's stages (a stage the run marked out of scope, or
/// back in scope): one row per stage whose grid state differs. A stage the grid does not have is left alone.
pub async fn sync_scopes(
    conn: &mut PgConnection,
    actor: &Actor,
    run: &RunRow,
    stages: &[StageRow],
) -> ApiResult<Outcome> {
    let j = grid::load(&mut *conn, &run.industry, &run.department).await?;
    let layer = dept_layer(run);
    let by = author(run.id);
    let mut out = Outcome::default();
    for s in stages {
        let Some(g) = j.stages.iter().find(|g| g.key.as_str() == s.stage.as_str()) else {
            continue;
        };
        if g.out_of_scope == s.out_of_scope {
            continue;
        }
        put(
            &mut *conn,
            actor,
            &layer,
            Kind::StageScope,
            &s.stage,
            json!({"out_of_scope": s.out_of_scope}),
            &by,
            &mut out,
        )
        .await?;
    }
    Ok(out)
}
