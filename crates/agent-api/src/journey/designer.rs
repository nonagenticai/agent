//! B8 phase 2 T4 / T4a / T4b: the platform journey designer's writes (the routes are
//! `platform/routes/journeys.rs`, super_admin only). Every function runs in the caller's `agent_platform`
//! transaction, edits the journey of ONE industry × department through B6b's `write_authored` and answers the
//! refreshed grid ([`grid::load`], findings included). The caller commits.
//!
//! Where a row goes (B8-6, amended by D106/D107): themes, stages, priors and channels live in the industry layer
//! `industry/<ind>`; activities, cards and stage scope in the department layer `industry_dept/<ind>/<dept>`; a
//! knowledge kind in the layer the designer picks (default the department layer). The journey kinds are catalogue
//! kinds, so a row is always the WHOLE item: a patch writes every field of the item as it is effective now, with
//! the edit applied. The op of a write is decided by where the key is already declared:
//!
//! - the target layer holds a current row → that row's op, `Expect::Rev(its rev)`;
//! - an earlier layer of the fold declares the key → `patch` (an override), `Expect::New`;
//! - nobody declares it → `add`, `Expect::New`.
//!
//! A retire writes a tombstone the same way (`Expect::Rev` over an own row, `Expect::New` over an inherited one).
//! The bodies carry no `rev`: the current revision is read here and checked again under the family lock by
//! `write_authored`, so two designers editing one row concurrently get 409 `stale_revision` for the second.
//!
//! Every route leaves, besides the `layer.revision` row of each revision, one `journey.*` platform audit row naming
//! industry, department and the keys touched (never free text). A department layer that does not exist yet is
//! created (`insert_layer`, `extends` = the same-key archetype) by a WRITE addressed to it; a read answers 404.
use std::collections::{BTreeMap, BTreeSet};

use agent_model::catalog::{STAR, op_key, parse_op_key};
use agent_model::compile::{
    Row as ModelRow, Selection, department_key, fold_knowledge, fold_order,
};
use agent_model::kinds::catalog::ObjectFields;
use agent_model::kinds::journey::{
    self as jk, ActivityActor, ActivityFields, ActivityObjectRef, ChannelStatusField,
    JourneyChannelFields, JourneyStageFields, JourneyThemeFields, OperationRef, StageScopeFields,
    ThemeStagePriorFields,
};
use agent_model::kinds::stage_knowledge::StageKnowledgeKindFields;
use agent_model::kinds::{self, Kind, Op};
use agent_model::layer::LayerKey;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::grid;
use super::model::{
    ActivityObject, Actor, ChannelKey, ChannelStatus, IndustryJourney, JourneyActivity,
    JourneyChannel, JourneyStage, JourneyTheme, ObjectRole, OperationExpectation, Slug, is_slug,
    object_key,
};
use crate::audit::PlatformEvent;
use crate::error::{ApiError, ApiResult};
use crate::platform::db::authored::{
    Expect, LayerMeta as NewLayerMeta, insert_layer, write_authored,
};
use crate::platform::db::layers::{self, Layer};
use crate::platform::routes::{Who, audit, conflict, missing, unprocessable};
use crate::platform::templates::{self, HeadModel};

/// Most utterances of one stage × channel (F-48).
pub const MAX_UTTERANCES: usize = 50;
/// Longest utterance, in characters (F-48).
pub const MAX_UTTERANCE_CHARS: usize = 500;

// --- bodies (the routes deserialise these) ---------------------------------------------------------------------------

/// `updateJourneyStage`: any of the four; `out_of_scope` goes to the department's `stage_scope` row.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageEdit {
    pub label: Option<String>,
    pub description: Option<String>,
    pub synonyms: Option<Vec<String>>,
    pub out_of_scope: Option<bool>,
}

/// `setThemeStagePrior`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PriorBody {
    pub weight: f64,
}

/// `addJourneyActivity`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewActivity {
    pub stage: String,
    pub slug: String,
    pub label: String,
    pub actor: Actor,
    #[serde(default)]
    pub trigger: String,
}

/// `moveJourneyActivity`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveBody {
    pub stage: String,
}

/// `bindActivityObject` (the web `ActivityObject`; `object`, when sent, must be the path's).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindBody {
    #[serde(default)]
    pub object: Option<String>,
    pub role: ObjectRole,
    #[serde(default)]
    pub fields: Vec<String>,
    #[serde(default)]
    pub required_inputs: Vec<String>,
}

/// `updateJourneyActivity`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityEdit {
    pub label: Option<String>,
    pub trigger: Option<String>,
    pub actor: Option<Actor>,
    pub also_in_stages: Option<Vec<String>>,
    /// `Some("")` clears it; `None` keeps what the row has.
    pub summary: Option<String>,
    pub description: Option<String>,
}

/// `addJourneyStage`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewStage {
    pub key: String,
    pub label: String,
    pub description: String,
    #[serde(default)]
    pub after: Option<String>,
}

/// `reorderJourneyStages`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OrderBody {
    pub stages: Vec<String>,
}

/// `addJourneyTheme`; `escalation_weight` is an integer 0..100 (F-46).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTheme {
    pub key: String,
    pub label: String,
    pub escalation_weight: f64,
}

/// `updateJourneyTheme`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThemeEdit {
    pub label: Option<String>,
    pub order: Option<i32>,
    pub escalation_weight: Option<f64>,
}

/// `setStageKnowledgeKind`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KindBody {
    #[serde(default)]
    pub store_key: Option<String>,
    pub required: bool,
    #[serde(default)]
    pub tenant_data: Option<bool>,
    /// A layer key; default the department layer.
    #[serde(default)]
    pub layer: Option<String>,
}

/// `setStageMessages`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MessagesBody {
    pub utterances: Vec<String>,
}

/// `setJourneyChannelStatus` (`v1` | `later` | `out_of_scope`, checked here for 422 `invalid_status`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelBody {
    pub status: String,
}

/// `setActivityOperation` (`R` | `O`, checked here for 422 `invalid_expectation`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperationBody {
    pub expectation: String,
}

// --- which journey -----------------------------------------------------------------------------------------------------

/// The journey a request addresses: an industry and one of its departments (B8-6 amendment).
#[derive(Debug, Clone, Serialize)]
pub struct Target {
    pub industry: String,
    pub department: String,
    pub ind_layer: String,
    pub dept_layer: String,
    /// The department layer's archetype (`extends`).
    pub extends: Option<String>,
}

fn journey_not_found(industry: &str, department: Option<&str>) -> ApiError {
    missing(
        "journey_not_found",
        match department {
            Some(d) => format!("No journey for industry {industry}, department {d}."),
            None => format!("No journey for industry {industry}."),
        },
    )
}

/// The (industry, department) a route addresses. `department` is optional while the industry has one department
/// layer, else 422 `department_required` (with `departments`); 404 `journey_not_found` for an unknown industry or
/// department. With `create` (a write), an unknown department that names a base archetype gets its layer
/// (`insert_layer`, `extends` = the archetype) and a `layer.create` audit row.
pub async fn resolve(
    conn: &mut PgConnection,
    industry: &str,
    department: Option<&str>,
    create: Option<&Who>,
) -> ApiResult<Target> {
    let all = layers::layers(&mut *conn).await?;
    let ind_layer = LayerKey::industry(industry).label();
    if !all.iter().any(|l| l.key == ind_layer) {
        return Err(journey_not_found(industry, None));
    }
    let depts: Vec<&Layer> = all
        .iter()
        .filter(|l| {
            l.scope == "industry_dept"
                && l.region.is_none()
                && l.industry.as_deref() == Some(industry)
        })
        .collect();
    let wanted = department.map(str::trim).filter(|d| !d.is_empty());
    let department = match wanted {
        Some(d) => {
            if !depts.iter().any(|l| l.department.as_deref() == Some(d)) {
                match create {
                    Some(who) => create_department(&mut *conn, who, industry, d).await?,
                    None => return Err(journey_not_found(industry, Some(d))),
                }
            }
            d.to_owned()
        }
        None => match depts.as_slice() {
            [] => return Err(journey_not_found(industry, None)),
            [one] => one
                .department
                .clone()
                .ok_or_else(|| journey_not_found(industry, None))?,
            many => {
                let names: Vec<String> = many.iter().filter_map(|l| l.department.clone()).collect();
                return Err(unprocessable(
                    "department_required",
                    format!("{industry} has several departments; pass ?department=<one of them>."),
                )
                .with("departments", names));
            }
        },
    };
    let dept_layer = department_key(industry, &department);
    let extends = match all.iter().find(|l| l.key == dept_layer) {
        Some(l) => l.extends.clone(),
        // just created
        None => Some(department.clone()),
    };
    Ok(Target {
        industry: industry.to_owned(),
        department,
        ind_layer,
        dept_layer,
        extends,
    })
}

/// A new `industry_dept/<ind>/<dept>` layer extending the archetype `<dept>`; 404 `journey_not_found` when base
/// defines no such archetype (or the industry layer is missing).
async fn create_department(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: &str,
) -> ApiResult<()> {
    let key = LayerKey::industry_dept(industry, department);
    let meta = NewLayerMeta {
        extends: Some(department.to_owned()),
        ..NewLayerMeta::default()
    };
    match insert_layer(&mut *conn, &key, None, &meta).await {
        Ok(layer) => {
            audit(
                &mut *conn,
                who,
                PlatformEvent::new("layer.create", layer.key.as_str())
                    .detail("layer", layer.key.as_str())
                    .detail("extends", department),
            )
            .await?;
            Ok(())
        }
        Err(e) if e.code() == "parent_layer_not_found" => {
            Err(journey_not_found(industry, Some(department)))
        }
        Err(e) => Err(e),
    }
}

// --- the working context -----------------------------------------------------------------------------------------------

/// The journey as it is now and the model rows the writes decide on.
struct Ctx {
    t: Target,
    journey: IndustryJourney,
    model: HeadModel,
    sel: Selection,
    /// Layer ids in fold order (base → … → department).
    order: Vec<String>,
}

/// The business-model profile of `industry` (as the grid reads it).
fn profile_of(rows: &[ModelRow], industry: &str) -> Option<String> {
    rows.iter()
        .find(|r| {
            r.kind == Kind::Profile
                && r.op != Op::Tombstone
                && r.fields.get("industry").and_then(Value::as_str) == Some(industry)
        })
        .map(|r| r.key.clone())
}

/// The (rev, op) of the current revision of (layer, kind, key).
async fn current(
    conn: &mut PgConnection,
    layer_id: Uuid,
    kind: Kind,
    key: &str,
) -> ApiResult<Option<(i32, Op)>> {
    let row: Option<(i32, String)> = sqlx::query_as(
        "SELECT rev, op FROM layer_row WHERE layer_id = $1 AND kind = $2 AND key = $3 AND is_current",
    )
    .bind(layer_id)
    .bind(kind.as_str())
    .bind(key)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(|(rev, op)| {
        let op = match op.as_str() {
            "add" => Op::Add,
            "patch" => Op::Patch,
            _ => Op::Tombstone,
        };
        (rev, op)
    }))
}

impl Ctx {
    async fn open(conn: &mut PgConnection, t: Target) -> ApiResult<Ctx> {
        // Read-modify-write uses the same family locks as the authored row writer. Lock before loading the
        // effective item so concurrent edits to different fields cannot overwrite each other with a stale copy.
        let mut families = [&t.ind_layer, &t.dept_layer];
        families.sort();
        for family in families {
            crate::platform::db::lock_family(&mut *conn, family).await?;
        }
        let journey = grid::load(&mut *conn, &t.industry, &t.department).await?;
        let model = templates::head_model(&mut *conn).await?;
        let sel = Selection {
            profile: profile_of(&model.rows, &t.industry),
            industry: t.industry.clone(),
            departments: vec![department_key(&t.industry, &t.department)],
            ..Selection::default()
        };
        let order: Vec<String> = fold_order(&model.layers, &sel, None, &[])
            .iter()
            .map(|s| s.row_layer_id())
            .collect();
        Ok(Ctx {
            t,
            journey,
            model,
            sel,
            order,
        })
    }

    fn layer_id(&self, layer_key: &str) -> Option<String> {
        self.model
            .layers
            .iter()
            .find(|l| l.key.label() == layer_key)
            .map(|l| l.id.clone())
    }

    /// Whether a layer before `layer_key` in the fold declares (kind, key) and nothing retired it since. A layer
    /// outside the fold (a job layer, a region sub-layer) sees every layer of the fold as earlier.
    fn inherited(&self, layer_key: &str, kind: Kind, key: &str) -> bool {
        let upto = self
            .layer_id(layer_key)
            .and_then(|id| self.order.iter().position(|o| *o == id))
            .unwrap_or(self.order.len());
        let mut present = false;
        for id in &self.order[..upto] {
            if let Some(r) = self
                .model
                .rows
                .iter()
                .find(|r| r.layer_id == *id && r.kind == kind && r.key == key)
            {
                present = r.op != Op::Tombstone;
            }
        }
        present
    }

    /// The layer's own current, non-retired row of (kind, key), as the model was read.
    fn own_row(&self, layer_key: &str, kind: Kind, key: &str) -> Option<&ModelRow> {
        let id = self.layer_id(layer_key)?;
        self.model
            .rows
            .iter()
            .find(|r| r.layer_id == id && r.kind == kind && r.key == key && r.op != Op::Tombstone)
    }

    /// The row of (kind, key) the fold leaves at the end, over the fold's layers in order (a whole-item fold).
    fn folded_row(&self, kind: Kind, key: &str) -> Option<&ModelRow> {
        let mut found: Option<&ModelRow> = None;
        for id in &self.order {
            if let Some(r) = self
                .model
                .rows
                .iter()
                .find(|r| r.layer_id == *id && r.kind == kind && r.key == key)
            {
                found = (r.op != Op::Tombstone).then_some(r);
            }
        }
        found
    }

    /// The fields of canonical object `object` as the fold sees it (retired fields left out); `None` for an object
    /// the catalogue does not define.
    fn object_fields(&self, object: &str) -> Option<BTreeSet<String>> {
        let row = self.folded_row(Kind::Object, object)?;
        let parsed: ObjectFields = serde_json::from_value(row.fields.clone()).ok()?;
        Some(
            parsed
                .fields
                .iter()
                .filter(|f| !f.retired)
                .map(|f| f.field.clone())
                .collect(),
        )
    }

    /// Whether the head catalogue (untagged revisions included, F-22) has operation `key`.
    fn operation_exists(&self, key: &str) -> bool {
        self.folded_row(Kind::Operation, key).is_some()
    }

    /// Write the whole item (op and expectation as in the module docs).
    async fn put(
        &self,
        conn: &mut PgConnection,
        who: &Who,
        layer_key: &str,
        kind: Kind,
        key: &str,
        fields: Value,
    ) -> ApiResult<()> {
        let layer = layers::layer_by_key(&mut *conn, layer_key).await?;
        let (op, expect) = match current(&mut *conn, layer.id, kind, key).await? {
            // a retired key is never added again: write_authored answers 409 add_tombstoned
            Some((_, Op::Tombstone)) => (Op::Add, Expect::New),
            Some((rev, op)) => (op, Expect::Rev(rev)),
            None if self.inherited(layer_key, kind, key) => (Op::Patch, Expect::New),
            None => (Op::Add, Expect::New),
        };
        write_authored(
            &mut *conn, &who.actor, layer_key, kind, key, op, fields, &who.by, expect,
        )
        .await?;
        Ok(())
    }

    /// Retire (kind, key) at `layer_key`: a tombstone over an own row, or over a row an earlier layer declares.
    /// `false` when there is nothing to retire from that layer.
    async fn retire(
        &self,
        conn: &mut PgConnection,
        who: &Who,
        layer_key: &str,
        kind: Kind,
        key: &str,
    ) -> ApiResult<bool> {
        let layer = layers::layer_by_key(&mut *conn, layer_key).await?;
        let expect = match current(&mut *conn, layer.id, kind, key).await? {
            Some((_, Op::Tombstone)) => return Ok(false),
            Some((rev, _)) => Expect::Rev(rev),
            None if self.inherited(layer_key, kind, key) => Expect::New,
            None => return Ok(false),
        };
        write_authored(
            &mut *conn,
            &who.actor,
            layer_key,
            kind,
            key,
            Op::Tombstone,
            json!({}),
            &who.by,
            expect,
        )
        .await?;
        Ok(true)
    }

    /// The layer a knowledge kind is written to: default the department layer; else the base, the industry, the
    /// department's archetype layer, the department, a region sub-layer of those, or a job layer (D107).
    async fn kind_layer(&self, conn: &mut PgConnection, layer: Option<&str>) -> ApiResult<Layer> {
        let key = layer
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .unwrap_or(&self.t.dept_layer);
        let found = layers::find_layer_by_key(&mut *conn, key)
            .await?
            .ok_or_else(|| missing("layer_not_found", format!("No layer “{key}”.")))?;
        let mut allowed = vec![
            "base".to_owned(),
            self.t.ind_layer.clone(),
            self.t.dept_layer.clone(),
        ];
        if let Some(a) = &self.t.extends {
            allowed.push(LayerKey::base_dept(a).label());
        }
        let ok = allowed.contains(&found.key)
            || (found.region.is_some() && allowed.contains(&found.family))
            || (found.scope == "job" && found.dimension_key.is_none());
        if !ok {
            return Err(unprocessable(
                "kind_not_allowed_in_layer",
                format!(
                    "{} is not a layer of the {}/{} journey (base, {}, its archetype, {} or a job).",
                    found.key,
                    self.t.industry,
                    self.t.department,
                    self.t.ind_layer,
                    self.t.dept_layer
                ),
            ));
        }
        Ok(found)
    }

    /// The one `journey.*` audit row of the route, then the refreshed grid.
    async fn finish(
        &self,
        conn: &mut PgConnection,
        who: &Who,
        action: &'static str,
        details: &[(&'static str, Value)],
    ) -> ApiResult<IndustryJourney> {
        let mut e =
            PlatformEvent::new(action, format!("{}/{}", self.t.industry, self.t.department))
                .detail("industry", self.t.industry.as_str())
                .detail("department", self.t.department.as_str());
        for (k, v) in details {
            e = e.detail(k, v.clone());
        }
        audit(&mut *conn, who, e).await?;
        grid::load(&mut *conn, &self.t.industry, &self.t.department).await
    }
}

// --- small helpers -------------------------------------------------------------------------------------------------------

fn to_value<T: Serialize>(t: &T) -> ApiResult<Value> {
    serde_json::to_value(t)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("journey row to JSON: {e}")))
}

fn required_text(v: &str, code: &'static str, msg: &str) -> ApiResult<String> {
    let t = v.trim();
    if t.is_empty() {
        Err(unprocessable(code, msg))
    } else {
        Ok(t.to_owned())
    }
}

/// Trimmed, non-empty, first occurrence wins.
fn dedupe(list: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in list.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !out.iter().any(|x| x == s) {
            out.push(s.to_owned());
        }
    }
    out
}

fn channel_str(c: ChannelKey) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn stage_of<'a>(j: &'a IndustryJourney, key: &str) -> ApiResult<&'a JourneyStage> {
    j.stages
        .iter()
        .find(|s| s.key.as_str() == key)
        .ok_or_else(|| missing("stage_not_found", format!("No stage {key}.")))
}

fn theme_of<'a>(j: &'a IndustryJourney, key: &str) -> ApiResult<&'a JourneyTheme> {
    j.themes
        .iter()
        .find(|t| t.key.as_str() == key)
        .ok_or_else(|| missing("theme_not_found", format!("No theme {key}.")))
}

fn activity_of<'a>(j: &'a IndustryJourney, key: &str) -> ApiResult<&'a JourneyActivity> {
    j.activities
        .iter()
        .find(|a| a.key == key)
        .ok_or_else(|| missing("activity_not_found", format!("No activity {key}.")))
}

fn channel_of(j: &IndustryJourney, key: &str) -> ApiResult<JourneyChannel> {
    let not_found = || missing("channel_not_found", format!("No channel {key}."));
    let ck: ChannelKey =
        serde_json::from_value(Value::String(key.to_owned())).map_err(|_| not_found())?;
    j.channels
        .iter()
        .find(|c| c.key == ck)
        .cloned()
        .ok_or_else(not_found)
}

/// An integer percentage 0..=100 as the 0–1 weight the rows hold (F-46).
fn percent(w: f64) -> ApiResult<f64> {
    if w.is_finite() && w.fract() == 0.0 && (0.0..=100.0).contains(&w) {
        Ok(w / 100.0)
    } else {
        Err(unprocessable(
            "invalid_weight",
            "An escalation weight is a whole number from 0 to 100.",
        ))
    }
}

/// The fields of stage `key` as it is effective now (messages included).
fn stage_fields(j: &IndustryJourney, key: &str) -> Option<JourneyStageFields> {
    let s = j.stages.iter().find(|s| s.key.as_str() == key)?;
    let mut messages: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for m in j.messages.iter().filter(|m| m.stage.as_str() == key) {
        messages.insert(channel_str(m.channel), m.utterances.clone());
    }
    Some(JourneyStageFields {
        label: s.label.clone(),
        summary: None,
        order: Some(s.order),
        description: Some(s.description.clone()),
        synonyms: Some(s.synonyms.clone()),
        themes: Some(s.themes.iter().map(|t| t.to_string()).collect()),
        messages: (!messages.is_empty()).then_some(messages),
    })
}

fn activity_fields(a: &JourneyActivity) -> ActivityFields {
    ActivityFields {
        label: a.label.clone(),
        summary: a.summary.clone(),
        description: a.description.clone(),
        objects: Some(
            a.objects
                .iter()
                .map(|o| ActivityObjectRef {
                    object: o.object.clone(),
                    role: match o.role {
                        ObjectRole::Subject => jk::ObjectRole::Subject,
                        ObjectRole::Context => jk::ObjectRole::Context,
                    },
                    fields: o.fields.clone(),
                    required_inputs: o.required_inputs.clone(),
                })
                .collect(),
        ),
        operations: Some(
            a.operations
                .iter()
                .map(|o| OperationRef {
                    op: o.op.clone(),
                    variant: o.variant.clone(),
                    required: o.required,
                })
                .collect(),
        ),
        area: a.area.clone(),
        trigger: Some(a.trigger.clone()),
        actor: Some(match a.actor {
            Actor::Customer => ActivityActor::Customer,
            Actor::Company => ActivityActor::Company,
            Actor::Employee => ActivityActor::Employee,
        }),
        also_in_stages: Some(a.also_in_stages.iter().map(|s| s.to_string()).collect()),
        required: a.required,
    }
}

fn channel_fields(c: &JourneyChannel, status: ChannelStatus) -> JourneyChannelFields {
    JourneyChannelFields {
        label: c.label.clone(),
        summary: None,
        status: Some(match status {
            ChannelStatus::V1 => ChannelStatusField::V1,
            ChannelStatus::Later => ChannelStatusField::Later,
            ChannelStatus::OutOfScope => ChannelStatusField::OutOfScope,
        }),
        note: (!c.note.is_empty()).then(|| c.note.clone()),
    }
}

fn theme_fields(label: &str, order: i32, weight: f64) -> JourneyThemeFields {
    JourneyThemeFields {
        label: label.to_owned(),
        summary: None,
        order: Some(order),
        escalation_weight: Some(weight),
    }
}

// --- T4: stages, priors, activities, objects -----------------------------------------------------------------------------

/// `updateJourneyStage` (F-45 adds `label`).
pub async fn update_stage(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    stage: &str,
    e: StageEdit,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let current_stage = stage_of(&cx.journey, stage)?;
    let was_out = current_stage.out_of_scope;
    let label = e
        .label
        .as_deref()
        .map(|l| required_text(l, "label_required", "A stage needs a label."))
        .transpose()?;
    let description = e
        .description
        .as_deref()
        .map(|d| {
            required_text(
                d,
                "description_required",
                "A stage needs a description: it is the GS option text.",
            )
        })
        .transpose()?;
    let synonyms = e.synonyms.as_deref().map(dedupe);
    if label.is_some() || description.is_some() || synonyms.is_some() {
        let mut f = stage_fields(&cx.journey, stage)
            .ok_or_else(|| missing("stage_not_found", format!("No stage {stage}.")))?;
        if let Some(l) = label {
            f.label = l;
        }
        if let Some(d) = description {
            f.description = Some(d);
        }
        if let Some(s) = synonyms {
            f.synonyms = Some(s);
        }
        cx.put(
            &mut *conn,
            who,
            &cx.t.ind_layer,
            Kind::JourneyStage,
            stage,
            to_value(&f)?,
        )
        .await?;
    }
    if let Some(out) = e.out_of_scope
        && out != was_out
    {
        let f = StageScopeFields {
            label: None,
            summary: None,
            out_of_scope: out,
        };
        cx.put(
            &mut *conn,
            who,
            &cx.t.dept_layer,
            Kind::StageScope,
            stage,
            to_value(&f)?,
        )
        .await?;
    }
    cx.finish(
        &mut *conn,
        who,
        "journey.stage_update",
        &[("stage", json!(stage))],
    )
    .await
}

/// `setThemeStagePrior`: the weight rounded to 0.01; 0 retires the row.
pub async fn set_prior(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    theme: &str,
    stage: &str,
    weight: f64,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    if !weight.is_finite() || !(0.0..=1.0).contains(&weight) {
        return Err(unprocessable(
            "invalid_weight",
            "A prior is a weight between 0 and 1.",
        ));
    }
    theme_of(&cx.journey, theme)?;
    stage_of(&cx.journey, stage)?;
    let w = (weight * 100.0).round() / 100.0;
    let key = format!("{theme}|{stage}");
    if w == 0.0 {
        cx.retire(
            &mut *conn,
            who,
            &cx.t.ind_layer,
            Kind::ThemeStagePrior,
            &key,
        )
        .await?;
    } else {
        let f = ThemeStagePriorFields {
            label: format!("{theme} → {stage}"),
            summary: None,
            weight: Some(w),
        };
        cx.put(
            &mut *conn,
            who,
            &cx.t.ind_layer,
            Kind::ThemeStagePrior,
            &key,
            to_value(&f)?,
        )
        .await?;
    }
    cx.finish(
        &mut *conn,
        who,
        "journey.prior_set",
        &[
            ("theme", json!(theme)),
            ("stage", json!(stage)),
            ("weight", json!(w)),
        ],
    )
    .await
}

/// `addJourneyActivity`: key `stage:slug`, no objects or operations yet.
pub async fn add_activity(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    b: NewActivity,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let slug = b.slug.trim();
    if !is_slug(slug) {
        return Err(unprocessable(
            "invalid_key",
            "Use lower_snake_case for the activity key (e.g. track_parcel).",
        ));
    }
    let label = required_text(&b.label, "label_required", "Give the activity a label.")?;
    let stage = b.stage.trim();
    stage_of_or_unknown(&cx.journey, stage)?;
    let key = format!("{stage}:{slug}");
    if cx.journey.activities.iter().any(|a| a.key == key) {
        return Err(conflict(
            "activity_exists",
            format!("{key} already exists."),
        ));
    }
    let f = ActivityFields {
        label,
        summary: None,
        description: None,
        objects: Some(Vec::new()),
        operations: Some(Vec::new()),
        area: None,
        trigger: Some(b.trigger.trim().to_owned()),
        actor: Some(match b.actor {
            Actor::Customer => ActivityActor::Customer,
            Actor::Company => ActivityActor::Company,
            Actor::Employee => ActivityActor::Employee,
        }),
        also_in_stages: Some(Vec::new()),
        required: false,
    };
    cx.put(
        &mut *conn,
        who,
        &cx.t.dept_layer,
        Kind::Activity,
        &key,
        to_value(&f)?,
    )
    .await?;
    cx.finish(
        &mut *conn,
        who,
        "journey.activity_add",
        &[("activity", json!(key))],
    )
    .await
}

/// 422 `unknown_stage` for a stage the journey does not have.
fn stage_of_or_unknown<'a>(j: &'a IndustryJourney, key: &str) -> ApiResult<&'a JourneyStage> {
    j.stages
        .iter()
        .find(|s| s.key.as_str() == key)
        .ok_or_else(|| {
            unprocessable(
                "unknown_stage",
                format!("{key} is not a stage of this journey."),
            )
        })
}

/// `moveJourneyActivity`: tombstone the old key, add the new one (same item, `stage` changed, the target out of
/// `also_in_stages`), and re-key the activity's use-case card with it.
pub async fn move_activity(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    key: &str,
    b: MoveBody,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let a = activity_of(&cx.journey, key)?.clone();
    let target = stage_of_or_unknown(&cx.journey, b.stage.trim())?
        .key
        .clone();
    if a.stage == target {
        return Err(unprocessable(
            "same_stage",
            format!("{key} is already in {target}."),
        ));
    }
    let name = key.split_once(':').map_or(key, |(_, n)| n);
    let new_key = format!("{target}:{name}");
    if cx.journey.activities.iter().any(|x| x.key == new_key) {
        return Err(conflict(
            "activity_exists",
            format!("{new_key} already exists."),
        ));
    }
    let mut moved = a.clone();
    moved.key = new_key.clone();
    moved.stage = target.clone();
    moved.also_in_stages.retain(|s| *s != target);
    cx.retire(&mut *conn, who, &cx.t.dept_layer, Kind::Activity, key)
        .await?;
    cx.put(
        &mut *conn,
        who,
        &cx.t.dept_layer,
        Kind::Activity,
        &new_key,
        to_value(&activity_fields(&moved))?,
    )
    .await?;
    for layer in [&cx.t.dept_layer, &cx.t.ind_layer] {
        if let Some(card) = cx.own_row(layer, Kind::UseCaseCard, key) {
            let fields = card.fields.clone();
            cx.retire(&mut *conn, who, layer, Kind::UseCaseCard, key)
                .await?;
            cx.put(&mut *conn, who, layer, Kind::UseCaseCard, &new_key, fields)
                .await?;
        }
    }
    cx.finish(
        &mut *conn,
        who,
        "journey.activity_move",
        &[("activity", json!(key)), ("to", json!(new_key))],
    )
    .await
}

/// `bindActivityObject`: replace or append the binding of `object`.
pub async fn bind_object(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    key: &str,
    object: &str,
    b: BindBody,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let mut a = activity_of(&cx.journey, key)?.clone();
    let object = object.trim();
    if let Some(o) = b.object.as_deref()
        && o.trim() != object
    {
        return Err(unprocessable(
            "unknown_object",
            format!("The body names {o}, the path {object}."),
        ));
    }
    let base = object_key(object);
    let known = cx.object_fields(base).ok_or_else(|| {
        unprocessable(
            "unknown_object",
            format!("{base} is not a canonical object (rule 1). Propose it from a generator run."),
        )
    })?;
    let fields = dedupe(&b.fields);
    let bad: Vec<String> = fields
        .iter()
        .filter(|f| !known.contains(*f))
        .cloned()
        .collect();
    if !bad.is_empty() {
        return Err(unprocessable(
            "unknown_field",
            format!("{base} has no field {} (rule 2).", bad.join(", ")),
        )
        .with("fields", bad));
    }
    if fields.is_empty() {
        return Err(unprocessable(
            "fields_required",
            "Pick at least one field of the object.",
        ));
    }
    let binding = ActivityObject {
        object: object.to_owned(),
        role: b.role,
        fields,
        required_inputs: dedupe(&b.required_inputs),
    };
    match a.objects.iter_mut().find(|o| o.object == object) {
        Some(slot) => *slot = binding,
        None => a.objects.push(binding),
    }
    cx.put(
        &mut *conn,
        who,
        &cx.t.dept_layer,
        Kind::Activity,
        key,
        to_value(&activity_fields(&a))?,
    )
    .await?;
    cx.finish(
        &mut *conn,
        who,
        "journey.object_bind",
        &[("activity", json!(key)), ("object", json!(object))],
    )
    .await
}

/// `unbindActivityObject`: 404 `object_not_bound` when it was not bound.
pub async fn unbind_object(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    key: &str,
    object: &str,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let mut a = activity_of(&cx.journey, key)?.clone();
    let before = a.objects.len();
    a.objects.retain(|o| o.object != object);
    if a.objects.len() == before {
        return Err(missing(
            "object_not_bound",
            format!("{object} is not bound to {key}."),
        ));
    }
    cx.put(
        &mut *conn,
        who,
        &cx.t.dept_layer,
        Kind::Activity,
        key,
        to_value(&activity_fields(&a))?,
    )
    .await?;
    cx.finish(
        &mut *conn,
        who,
        "journey.object_unbind",
        &[("activity", json!(key)), ("object", json!(object))],
    )
    .await
}

// --- T4a: activities, stages, themes, kinds, messages, channels ------------------------------------------------------------

/// `updateJourneyActivity` (F-43): label, trigger, actor, `also_in_stages`.
pub async fn update_activity(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    key: &str,
    e: ActivityEdit,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let mut a = activity_of(&cx.journey, key)?.clone();
    if let Some(l) = e.label.as_deref() {
        a.label = required_text(l, "label_required", "Give the activity a label.")?;
    }
    if let Some(tr) = e.trigger.as_deref() {
        a.trigger = tr.trim().to_owned();
    }
    if let Some(actor) = e.actor {
        a.actor = actor;
    }
    if let Some(s) = e.summary.as_deref() {
        a.summary = Some(s.trim().to_owned()).filter(|s| !s.is_empty());
    }
    if let Some(d) = e.description.as_deref() {
        a.description = Some(d.trim().to_owned()).filter(|d| !d.is_empty());
    }
    if let Some(list) = e.also_in_stages.as_deref() {
        let mut also: Vec<Slug> = Vec::new();
        for s in dedupe(list) {
            let stage = stage_of_or_unknown(&cx.journey, &s)?;
            if stage.key == a.stage {
                return Err(unprocessable(
                    "unknown_stage",
                    format!("{s} is the activity's own stage."),
                ));
            }
            also.push(stage.key.clone());
        }
        a.also_in_stages = also;
    }
    cx.put(
        &mut *conn,
        who,
        &cx.t.dept_layer,
        Kind::Activity,
        key,
        to_value(&activity_fields(&a))?,
    )
    .await?;
    cx.finish(
        &mut *conn,
        who,
        "journey.activity_update",
        &[("activity", json!(key))],
    )
    .await
}

/// `retireJourneyActivity` (F-43): one transaction tombstones the activity and its use-case cards. A job pinned to
/// an earlier tag keeps it (its pins do not move).
pub async fn retire_activity(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    key: &str,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    activity_of(&cx.journey, key)?;
    cx.retire(&mut *conn, who, &cx.t.dept_layer, Kind::Activity, key)
        .await?;
    for layer in [&cx.t.dept_layer, &cx.t.ind_layer] {
        if cx.own_row(layer, Kind::UseCaseCard, key).is_some() {
            cx.retire(&mut *conn, who, layer, Kind::UseCaseCard, key)
                .await?;
        }
    }
    cx.finish(
        &mut *conn,
        who,
        "journey.activity_retire",
        &[("activity", json!(key))],
    )
    .await
}

/// `addJourneyStage` for an industry: the answer is the refreshed grid.
pub async fn add_stage(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    b: NewStage,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let key = b.key.trim();
    if !is_slug(key) {
        return Err(unprocessable(
            "invalid_key",
            "Use lower_snake_case for the stage key (2–41 characters).",
        ));
    }
    let label = required_text(&b.label, "label_required", "A stage needs a label.")?;
    let description = required_text(
        &b.description,
        "description_required",
        "A stage needs a description: it is the GS option text.",
    )?;
    if cx.journey.stages.iter().any(|s| s.key.as_str() == key) {
        return Err(conflict("stage_exists", format!("{key} already exists.")));
    }
    let new_order = match b.after.as_deref() {
        Some(a) => stage_of(&cx.journey, a.trim())?.order + 1,
        None => cx.journey.stages.iter().map(|s| s.order).max().unwrap_or(0) + 1,
    };
    if b.after.is_some() {
        for s in cx.journey.stages.iter().filter(|s| s.order >= new_order) {
            let mut f = stage_fields(&cx.journey, s.key.as_str())
                .ok_or_else(|| missing("stage_not_found", format!("No stage {}.", s.key)))?;
            f.order = Some(s.order + 1);
            cx.put(
                &mut *conn,
                who,
                &cx.t.ind_layer,
                Kind::JourneyStage,
                s.key.as_str(),
                to_value(&f)?,
            )
            .await?;
        }
    }
    let f = JourneyStageFields {
        label,
        summary: None,
        order: Some(new_order),
        description: Some(description),
        synonyms: Some(Vec::new()),
        themes: Some(Vec::new()),
        messages: None,
    };
    cx.put(
        &mut *conn,
        who,
        &cx.t.ind_layer,
        Kind::JourneyStage,
        key,
        to_value(&f)?,
    )
    .await?;
    cx.finish(
        &mut *conn,
        who,
        "journey.stage_add",
        &[("stage", json!(key))],
    )
    .await
}

/// `addJourneyStage` with `{industry}` = `base`: the stage goes into the base layer, every industry inherits it.
/// The answer names the stage written (there is no single grid to answer with).
/// One `journey_stage` row of the base layer: key, revision, op, fields.
type StageRow = (String, i32, String, Value);

pub async fn add_base_stage(conn: &mut PgConnection, who: &Who, b: NewStage) -> ApiResult<Value> {
    let key = b.key.trim();
    if !is_slug(key) {
        return Err(unprocessable(
            "invalid_key",
            "Use lower_snake_case for the stage key (2–41 characters).",
        ));
    }
    let label = required_text(&b.label, "label_required", "A stage needs a label.")?;
    let description = required_text(
        &b.description,
        "description_required",
        "A stage needs a description: it is the GS option text.",
    )?;
    let rows: Vec<StageRow> = sqlx::query_as(
        "SELECT key, rev, op, fields FROM layer_head \
          WHERE layer_key = 'base' AND kind = 'journey_stage' ORDER BY key COLLATE \"C\"",
    )
    .fetch_all(&mut *conn)
    .await?;
    let live: Vec<&StageRow> = rows.iter().filter(|r| r.2 != "tombstone").collect();
    if live.iter().any(|r| r.0 == key) {
        return Err(conflict("stage_exists", format!("{key} already exists.")));
    }
    // a row without an explicit `order` sits at its position in key order (as the grid reads it)
    let orders: Vec<(&StageRow, i32)> = live
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let implicit = i32::try_from(i).unwrap_or(i32::MAX).saturating_add(1);
            let order =
                r.3.get("order")
                    .and_then(Value::as_i64)
                    .and_then(|o| i32::try_from(o).ok())
                    .unwrap_or(implicit);
            (*r, order)
        })
        .collect();
    let new_order = match b.after.as_deref() {
        Some(a) => {
            let a = a.trim();
            orders
                .iter()
                .find(|(r, _)| r.0 == a)
                .map(|(_, o)| o + 1)
                .ok_or_else(|| missing("stage_not_found", format!("No stage {a}.")))?
        }
        None => orders.iter().map(|(_, o)| *o).max().unwrap_or(0) + 1,
    };
    if b.after.is_some() {
        for ((skey, rev, op, fields), order) in orders.iter().filter(|(_, o)| *o >= new_order) {
            let mut fields = fields.clone();
            if let Some(map) = fields.as_object_mut() {
                map.insert("order".to_owned(), json!(order + 1));
            }
            let op = if op == "patch" { Op::Patch } else { Op::Add };
            write_authored(
                &mut *conn,
                &who.actor,
                "base",
                Kind::JourneyStage,
                skey,
                op,
                fields,
                &who.by,
                Expect::Rev(*rev),
            )
            .await?;
        }
    }
    let f = JourneyStageFields {
        label,
        summary: None,
        order: Some(new_order),
        description: Some(description),
        synonyms: Some(Vec::new()),
        themes: Some(Vec::new()),
        messages: None,
    };
    write_authored(
        &mut *conn,
        &who.actor,
        "base",
        Kind::JourneyStage,
        key,
        Op::Add,
        to_value(&f)?,
        &who.by,
        Expect::New,
    )
    .await?;
    audit(
        &mut *conn,
        who,
        PlatformEvent::new("journey.stage_add", "base")
            .detail("industry", "base")
            .detail("stage", key),
    )
    .await?;
    Ok(json!({"industry": "base", "layer": "base", "stage": key, "order": new_order}))
}

/// `reorderJourneyStages`: `stages` must be exactly the effective stage set; every stage whose position changes
/// gets its `order` patched, all in one transaction.
pub async fn reorder_stages(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    b: OrderBody,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let wanted: Vec<&str> = b.stages.iter().map(|s| s.trim()).collect();
    let have: BTreeSet<&str> = cx.journey.stages.iter().map(|s| s.key.as_str()).collect();
    let given: BTreeSet<&str> = wanted.iter().copied().collect();
    if wanted.len() != have.len() || given != have {
        return Err(unprocessable(
            "order_mismatch",
            "The order must name every stage of the journey exactly once.",
        )
        .with(
            "stages",
            have.iter()
                .map(|s| (*s).to_owned())
                .collect::<Vec<String>>(),
        ));
    }
    for (i, key) in wanted.iter().enumerate() {
        let position = i32::try_from(i).unwrap_or(i32::MAX).saturating_add(1);
        let s = stage_of(&cx.journey, key)?;
        if s.order == position {
            continue;
        }
        let mut f = stage_fields(&cx.journey, key)
            .ok_or_else(|| missing("stage_not_found", format!("No stage {key}.")))?;
        f.order = Some(position);
        cx.put(
            &mut *conn,
            who,
            &cx.t.ind_layer,
            Kind::JourneyStage,
            key,
            to_value(&f)?,
        )
        .await?;
    }
    cx.finish(
        &mut *conn,
        who,
        "journey.stage_reorder",
        &[("stages", json!(wanted))],
    )
    .await
}

/// `addJourneyTheme` (F-46).
pub async fn add_theme(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    b: NewTheme,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let key = b.key.trim();
    if !is_slug(key) {
        return Err(unprocessable(
            "invalid_key",
            "Use lower_snake_case for the theme key (2–41 characters).",
        ));
    }
    let label = required_text(&b.label, "label_required", "A theme needs a label.")?;
    let weight = percent(b.escalation_weight)?;
    if cx.journey.themes.iter().any(|t| t.key.as_str() == key) {
        return Err(conflict("theme_exists", format!("{key} already exists.")));
    }
    let order = cx.journey.themes.iter().map(|t| t.order).max().unwrap_or(0) + 1;
    cx.put(
        &mut *conn,
        who,
        &cx.t.ind_layer,
        Kind::JourneyTheme,
        key,
        to_value(&theme_fields(&label, order, weight))?,
    )
    .await?;
    cx.finish(
        &mut *conn,
        who,
        "journey.theme_add",
        &[("theme", json!(key))],
    )
    .await
}

/// `updateJourneyTheme` (F-46).
pub async fn update_theme(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    theme: &str,
    e: ThemeEdit,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let cur = theme_of(&cx.journey, theme)?;
    let label = match e.label.as_deref() {
        Some(l) => required_text(l, "label_required", "A theme needs a label.")?,
        None => cur.label.clone(),
    };
    let weight = match e.escalation_weight {
        Some(w) => percent(w)?,
        None => cur.escalation_weight,
    };
    let order = e.order.unwrap_or(cur.order);
    cx.put(
        &mut *conn,
        who,
        &cx.t.ind_layer,
        Kind::JourneyTheme,
        theme,
        to_value(&theme_fields(&label, order, weight))?,
    )
    .await?;
    cx.finish(
        &mut *conn,
        who,
        "journey.theme_update",
        &[("theme", json!(theme))],
    )
    .await
}

/// `^[a-z][a-z0-9_]{1,62}$`.
fn store_key_ok(s: &str) -> bool {
    let b = s.as_bytes();
    (2..=63).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// `setStageKnowledgeKind` (F-47, D107/D110): the kind is written at the layer the designer picks; a layer below a
/// declaration of `required: true` may not say `false` (the compile's `kind_required_downgrade`, checked on the
/// fold after the write; the transaction is dropped by the caller on the 422).
pub async fn set_kind(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    stage: &str,
    kind: &str,
    b: KindBody,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    if stage != "*" {
        stage_of(&cx.journey, stage)?;
    }
    let key = format!("{stage}|{kind}");
    if kinds::split_stage_knowledge_key(&key).is_none() {
        return Err(unprocessable(
            "invalid_key",
            format!("“{kind}” is not a knowledge kind key (e.g. policy.returns)."),
        ));
    }
    let store = b.store_key.as_deref().map(str::trim).unwrap_or_default();
    if store.is_empty() {
        return Err(unprocessable(
            "store_key_required",
            "Name the knowledge store this kind is bound to.",
        ));
    }
    if !store_key_ok(store) {
        return Err(unprocessable(
            "invalid_key",
            format!(
                "“{store}” is not a store key: lowercase letters, digits and _, 2–63 characters."
            ),
        ));
    }
    let layer = cx.kind_layer(&mut *conn, b.layer.as_deref()).await?;
    // keep the label and description an own row already carries
    let (label, description) = match cx.own_row(&layer.key, Kind::StageKnowledgeKind, &key) {
        Some(r) => {
            let own: Option<StageKnowledgeKindFields> =
                serde_json::from_value(r.fields.clone()).ok();
            match own {
                Some(o) => (o.label, o.description),
                None => (kind.to_owned(), None),
            }
        }
        None => (kind.to_owned(), None),
    };
    let f = StageKnowledgeKindFields {
        label,
        summary: None,
        description,
        store_key: Some(store.to_owned()),
        required: Some(b.required),
        tenant_data: b.tenant_data,
    };
    cx.put(
        &mut *conn,
        who,
        &layer.key,
        Kind::StageKnowledgeKind,
        &key,
        to_value(&f)?,
    )
    .await?;
    let after = templates::head_model(&mut *conn).await?;
    let job = (layer.scope == "job").then(|| layer.job.clone()).flatten();
    let (_, errors) = fold_knowledge(&after.layers, &after.rows, &cx.sel, job.as_deref());
    if let Some(e) = errors
        .iter()
        .find(|e| e.code == "kind_required_downgrade" && e.key == key)
    {
        return Err(unprocessable("kind_required_downgrade", e.message.clone()));
    }
    cx.finish(
        &mut *conn,
        who,
        "journey.stage_kind_set",
        &[
            ("stage", json!(stage)),
            ("kind", json!(kind)),
            ("layer", json!(layer.key)),
            ("required", json!(b.required)),
        ],
    )
    .await
}

/// `removeStageKnowledgeKind` (F-47): a tombstone at `layer` (default the department layer), which hides a kind
/// declared there or above. 404 `kind_not_found` when the journey has no such kind or `layer` declares nothing to
/// remove.
pub async fn remove_kind(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    stage: &str,
    kind: &str,
    layer: Option<&str>,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let not_found = || {
        missing(
            "kind_not_found",
            format!("No knowledge kind {kind} at stage {stage} here."),
        )
    };
    if !cx
        .journey
        .knowledge_kinds
        .iter()
        .any(|k| k.stage == stage && k.kind == kind)
    {
        return Err(not_found());
    }
    let key = format!("{stage}|{kind}");
    let layer = cx.kind_layer(&mut *conn, layer).await?;
    if !cx
        .retire(&mut *conn, who, &layer.key, Kind::StageKnowledgeKind, &key)
        .await?
    {
        return Err(not_found());
    }
    cx.finish(
        &mut *conn,
        who,
        "journey.stage_kind_remove",
        &[
            ("stage", json!(stage)),
            ("kind", json!(kind)),
            ("layer", json!(layer.key)),
        ],
    )
    .await
}

/// `setStageMessages` (F-48): the utterances of one channel of one stage (an empty list clears the channel).
pub async fn set_messages(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    stage: &str,
    channel: &str,
    b: MessagesBody,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    stage_of(&cx.journey, stage)?;
    let ch = channel_of(&cx.journey, channel)?;
    let utterances = dedupe(&b.utterances);
    if utterances.len() > MAX_UTTERANCES {
        return Err(unprocessable(
            "too_many_utterances",
            format!("At most {MAX_UTTERANCES} example messages per channel."),
        ));
    }
    if utterances
        .iter()
        .any(|u| u.chars().count() > MAX_UTTERANCE_CHARS)
    {
        return Err(unprocessable(
            "utterance_too_long",
            format!("An example message is at most {MAX_UTTERANCE_CHARS} characters."),
        ));
    }
    let mut f = stage_fields(&cx.journey, stage)
        .ok_or_else(|| missing("stage_not_found", format!("No stage {stage}.")))?;
    let mut messages = f.messages.take().unwrap_or_default();
    if utterances.is_empty() {
        messages.remove(&channel_str(ch.key));
    } else {
        messages.insert(channel_str(ch.key), utterances.clone());
    }
    f.messages = (!messages.is_empty()).then_some(messages);
    cx.put(
        &mut *conn,
        who,
        &cx.t.ind_layer,
        Kind::JourneyStage,
        stage,
        to_value(&f)?,
    )
    .await?;
    cx.finish(
        &mut *conn,
        who,
        "journey.stage_messages_set",
        &[
            ("stage", json!(stage)),
            ("channel", json!(channel)),
            ("count", json!(utterances.len())),
        ],
    )
    .await
}

/// `setJourneyChannelStatus` (F-49): this industry's status of a channel; the base row stays the default.
pub async fn set_channel_status(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    channel: &str,
    b: ChannelBody,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let ch = channel_of(&cx.journey, channel)?;
    let status = match b.status.as_str() {
        "v1" => ChannelStatus::V1,
        "later" => ChannelStatus::Later,
        "out_of_scope" => ChannelStatus::OutOfScope,
        other => {
            return Err(unprocessable(
                "invalid_status",
                format!("“{other}” is not a status: v1, later or out_of_scope."),
            ));
        }
    };
    cx.put(
        &mut *conn,
        who,
        &cx.t.ind_layer,
        Kind::JourneyChannel,
        channel,
        to_value(&channel_fields(&ch, status))?,
    )
    .await?;
    cx.finish(
        &mut *conn,
        who,
        "journey.channel_status_set",
        &[("channel", json!(channel)), ("status", json!(b.status))],
    )
    .await
}

// --- T4b: operation expectations ---------------------------------------------------------------------------------------------

/// `object.verb{variant}` → (the canonical `object.verb`, the variant); `None` for a key that is not an operation.
fn split_op(raw: &str) -> Option<(String, Option<String>, String)> {
    let (object, verb, variant) = parse_op_key(raw)?;
    let key = op_key(&object, &verb, &variant);
    let variant = (variant != STAR).then_some(variant);
    Some((format!("{object}.{verb}"), variant, key))
}

/// `setActivityOperation` (F-42): the head catalogue (untagged F-22 edits included) must have the operation, and
/// its object must be bound to the activity; the entry `{op, variant, required}` replaces or joins the activity's
/// `operations`.
pub async fn set_operation(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    key: &str,
    op: &str,
    b: OperationBody,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let mut a = activity_of(&cx.journey, key)?.clone();
    let (name, variant, full) = split_op(op.trim()).ok_or_else(|| {
        unprocessable(
            "unknown_operation",
            format!("“{op}” is not an operation key (object.verb or object.verb{{variant}})."),
        )
    })?;
    if !cx.operation_exists(&full) {
        return Err(unprocessable(
            "unknown_operation",
            format!("{full} is not in the operation catalogue."),
        ));
    }
    let required = match b.expectation.as_str() {
        "R" => true,
        "O" => false,
        other => {
            return Err(unprocessable(
                "invalid_expectation",
                format!("“{other}” is not an expectation: R (required) or O (optional)."),
            ));
        }
    };
    let object = name.split_once('.').map_or(name.as_str(), |(o, _)| o);
    if !a.objects.iter().any(|o| object_key(&o.object) == object) {
        return Err(unprocessable(
            "object_not_bound",
            format!("{object} is not bound to {key}; bind it first."),
        ));
    }
    let entry = OperationExpectation {
        op: name.clone(),
        variant: variant.clone(),
        required,
    };
    match a
        .operations
        .iter_mut()
        .find(|o| o.op == name && o.variant == variant)
    {
        Some(slot) => *slot = entry,
        None => a.operations.push(entry),
    }
    cx.put(
        &mut *conn,
        who,
        &cx.t.dept_layer,
        Kind::Activity,
        key,
        to_value(&activity_fields(&a))?,
    )
    .await?;
    cx.finish(
        &mut *conn,
        who,
        "journey.operation_set",
        &[
            ("activity", json!(key)),
            ("operation", json!(full)),
            ("expectation", json!(b.expectation)),
        ],
    )
    .await
}

/// `removeActivityOperation` (F-42): 404 `operation_not_expected` when the activity did not expect it.
pub async fn remove_operation(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    key: &str,
    op: &str,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    let mut a = activity_of(&cx.journey, key)?.clone();
    let not_expected = || {
        missing(
            "operation_not_expected",
            format!("{key} does not expect {op}."),
        )
    };
    let (name, variant, full) = split_op(op.trim()).ok_or_else(not_expected)?;
    let before = a.operations.len();
    a.operations
        .retain(|o| !(o.op == name && o.variant == variant));
    if a.operations.len() == before {
        return Err(not_expected());
    }
    cx.put(
        &mut *conn,
        who,
        &cx.t.dept_layer,
        Kind::Activity,
        key,
        to_value(&activity_fields(&a))?,
    )
    .await?;
    cx.finish(
        &mut *conn,
        who,
        "journey.operation_remove",
        &[("activity", json!(key)), ("operation", json!(full))],
    )
    .await
}

/// A platform use-case card edit stays untagged; published jobs continue reading their pinned card.
pub async fn update_card(
    conn: &mut PgConnection,
    who: &Who,
    industry: &str,
    department: Option<&str>,
    key: &str,
    patch: serde_json::Map<String, Value>,
) -> ApiResult<IndustryJourney> {
    let t = resolve(&mut *conn, industry, department, Some(who)).await?;
    let cx = Ctx::open(&mut *conn, t).await?;
    activity_of(&cx.journey, key)?;
    if patch.is_empty() {
        return Err(unprocessable(
            "invalid_card",
            "Change at least one card field.",
        ));
    }
    let mut fields = cx
        .folded_row(Kind::UseCaseCard, key)
        .and_then(|r| r.fields.as_object())
        .cloned()
        .unwrap_or_default();
    for (key, value) in patch {
        if value.is_null() {
            fields.remove(&key);
        } else {
            fields.insert(key, value);
        }
    }
    let value = Value::Object(fields);
    agent_model::kinds::validate_fields(Kind::UseCaseCard, &value)
        .map_err(|_| unprocessable("invalid_card", "The use-case card fields are not valid."))?;
    cx.put(
        &mut *conn,
        who,
        &cx.t.dept_layer,
        Kind::UseCaseCard,
        key,
        value,
    )
    .await?;
    cx.finish(
        conn,
        who,
        "journey.card_update",
        &[("activity", json!(key))],
    )
    .await
}
