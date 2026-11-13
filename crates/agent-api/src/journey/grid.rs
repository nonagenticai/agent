//! B8 phase 2 T2b: the industry journey grid loader. One grid = one industry × one department (D115: a
//! multi-department job loads one grid per department), built from layer rows with THE fold order of D112/D115
//! (`agent_model::compile::fold_order`: base → industry → the department's archetype and its `base_dept/<a>` layer →
//! `industry_dept/<ind>/<dept>`; region sub-layers are ignored until the region selection reaches the grid).
//!
//! - [`load`]: the platform view — the CURRENT rows, untagged revisions included (what the designer is editing);
//!   404 `journey_not_found` for an unknown industry or department.
//! - [`load_at`]: the same shape from the members of PINNED tags (a job's template pins), so a later edit of the
//!   template never moves it.
//! - [`industries`]: one summary per industry (its departments, status, activity and red counts), not-started
//!   industries included (`journeyIndustries`).
//!
//! Folding. `activity` and `stage_knowledge_kind` go through `agent_model::compile::fold_knowledge` (the compile's
//! own fold: archetype activities, the sibling rule, `FoldedActivity` carrying objects / operations / actor /
//! trigger / `also_in_stages` since T2b). The other journey kinds (`journey_theme`, `journey_stage`,
//! `theme_stage_prior`, `journey_channel`, `stage_scope`) are catalogue kinds with the same semantics — a later
//! layer's add or patch sets the WHOLE item, a tombstone removes it — folded over the same steps by
//! [`fold_items`]. Every item carries the layer that set it last (`layer`).
//!
//! `findings` are [`rules::industry_findings`] over the grid and a [`Catalogue`] taken from the compile of the
//! industry's selection ([`catalogue_of`]); when that compile fails the catalogue-dependent rules (1, 2, 5, 6) are
//! skipped rather than answering false reds. An activity of an archetype that applies to every stage (`*:<name>`,
//! D107) has no stage column and is not part of the grid.
use std::collections::{BTreeMap, BTreeSet};

use agent_model::catalog::OperationRow;
use agent_model::compile::{
    self, Compiled, FoldStep, FoldedActivity, FoldedKind, LayerMeta, Mode, Row, Selection,
    department_key, fold_knowledge, fold_order,
};
use agent_model::kinds::catalog::ObjectFields;
use agent_model::kinds::journey::{
    self as jk, ActivityActor, CHANNEL_KEYS, ChannelStatusField, JourneyChannelFields,
    JourneyStageFields, JourneyThemeFields, StageScopeFields, ThemeStagePriorFields,
};
use agent_model::kinds::{Kind, Op};
use agent_model::layer::LayerKey;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;

use super::model::{
    ActivityObject, Actor, ChannelKey, ChannelStatus, IndustryJourney, JourneyActivity,
    JourneyChannel, JourneyStage, JourneyStatus, JourneyTheme, ObjectRole, OperationExpectation,
    Severity, Slug, StageKnowledgeKind, StageMessage, ThemeStagePrior, ValidationFinding,
    journey_status,
};
use super::rules::{self, Catalogue, OpInfo};
use crate::error::{ApiError, ApiResult};
use crate::platform::db::layers::{self, Layer};
use crate::platform::db::rows::{KeyedRow, untagged_rows};
use crate::platform::db::tags as db_tags;
use crate::platform::templates::{self, TemplatePin};

/// The row kinds a grid shows: an untagged revision of one of them is a journey `draft_rev`.
pub const JOURNEY_KINDS: [Kind; 8] = [
    Kind::JourneyTheme,
    Kind::JourneyStage,
    Kind::ThemeStagePrior,
    Kind::JourneyChannel,
    Kind::StageScope,
    Kind::Activity,
    Kind::StageKnowledgeKind,
    Kind::UseCaseCard,
];

/// The kinds the loaders read: [`JOURNEY_KINDS`] plus what `fold_knowledge` needs (archetype definitions, packs).
const READ_KINDS: [Kind; 10] = [
    Kind::JourneyTheme,
    Kind::JourneyStage,
    Kind::ThemeStagePrior,
    Kind::JourneyChannel,
    Kind::StageScope,
    Kind::Activity,
    Kind::StageKnowledgeKind,
    Kind::UseCaseCard,
    Kind::KnowledgePack,
    Kind::DepartmentArchetype,
];

fn not_found(industry: &str, department: &str) -> ApiError {
    ApiError::Missing(
        "journey_not_found",
        format!("No journey for industry {industry}, department {department}."),
    )
}

/// `family@vN`, the display form of a tag.
fn tag_name(family: &str, version: i32) -> String {
    format!("{family}@v{version}")
}

// --- the fold -----------------------------------------------------------------------------------------------------

/// One folded catalogue item with the layer whose row set it last.
struct Item {
    key: String,
    fields: Value,
    layer: String,
}

/// Rows by `layer_id`, in the order they were given.
type ByLayer<'r> = BTreeMap<&'r str, Vec<&'r Row>>;

fn by_layer(rows: &[Row]) -> ByLayer<'_> {
    let mut m: ByLayer<'_> = BTreeMap::new();
    for r in rows {
        m.entry(r.layer_id.as_str()).or_default().push(r);
    }
    m
}

/// The rows of `kind` over the fold `steps`, in first-set order: per layer the adds and patches set the whole item,
/// then the tombstones remove (the compile's order within a layer). An archetype step has no layer rows here (its
/// activities and kinds are `fold_knowledge`'s).
fn fold_items(steps: &[FoldStep<'_>], rows: &ByLayer<'_>, kind: Kind) -> Vec<Item> {
    let mut order: Vec<String> = Vec::new();
    let mut items: BTreeMap<String, Item> = BTreeMap::new();
    for s in steps {
        let Some(layer_rows) = rows.get(s.row_layer_id().as_str()) else {
            continue;
        };
        let label = s.label();
        let of_kind = || layer_rows.iter().filter(|r| r.kind == kind);
        let sets = of_kind().filter(|r| r.op != Op::Tombstone);
        let removes = of_kind().filter(|r| r.op == Op::Tombstone);
        for r in sets.chain(removes) {
            if r.op == Op::Tombstone {
                if items.remove(&r.key).is_some() {
                    order.retain(|k| *k != r.key);
                }
            } else {
                let item = Item {
                    key: r.key.clone(),
                    fields: r.fields.clone(),
                    layer: label.clone(),
                };
                if items.insert(r.key.clone(), item).is_none() {
                    order.push(r.key.clone());
                }
            }
        }
    }
    order.into_iter().filter_map(|k| items.remove(&k)).collect()
}

/// Parse an item's fields into its kind's struct; a row that does not parse is logged and left out (the writers
/// validate, so this is stored data from before the kind's struct).
fn parse<T: serde::de::DeserializeOwned>(item: &Item, kind: Kind) -> Option<T> {
    match serde_json::from_value(item.fields.clone()) {
        Ok(f) => Some(f),
        Err(e) => {
            tracing::warn!(kind = %kind, key = %item.key, layer = %item.layer, error = %e, "journey row skipped");
            None
        }
    }
}

fn slug(s: &str) -> Option<Slug> {
    let r = Slug::new(s);
    if r.is_none() {
        tracing::debug!(key = s, "not a journey key; skipped");
    }
    r
}

/// The folded content of one grid (everything but the status and the findings).
#[derive(Default)]
struct Body {
    themes: Vec<JourneyTheme>,
    stages: Vec<JourneyStage>,
    priors: Vec<ThemeStagePrior>,
    channels: Vec<JourneyChannel>,
    activities: Vec<JourneyActivity>,
    messages: Vec<StageMessage>,
    knowledge_kinds: Vec<StageKnowledgeKind>,
}

fn object_of(o: &jk::ActivityObjectRef) -> ActivityObject {
    ActivityObject {
        object: o.object.clone(),
        role: match o.role {
            jk::ObjectRole::Subject => ObjectRole::Subject,
            jk::ObjectRole::Context => ObjectRole::Context,
        },
        fields: o.fields.clone(),
        required_inputs: o.required_inputs.clone(),
    }
}

fn activity_of(a: &FoldedActivity) -> Option<JourneyActivity> {
    Some(JourneyActivity {
        key: a.key.clone(),
        stage: slug(&a.stage)?,
        actor: match a.actor {
            Some(ActivityActor::Company) => Actor::Company,
            Some(ActivityActor::Employee) => Actor::Employee,
            _ => Actor::Customer,
        },
        label: a.label.clone(),
        trigger: a.trigger.clone(),
        also_in_stages: a.also_in_stages.iter().filter_map(|s| slug(s)).collect(),
        objects: a.objects.iter().map(object_of).collect(),
        operations: a
            .operations
            .iter()
            .map(|o| OperationExpectation {
                op: o.op.clone(),
                variant: o.variant.clone(),
                required: o.required,
            })
            .collect(),
        // the stored `summary` falls back to the label when none was authored (`kinds::summary`): that is no summary
        summary: (!a.summary.is_empty() && a.summary != a.label).then(|| a.summary.clone()),
        description: (!a.description.is_empty()).then(|| a.description.clone()),
        area: a.area.clone(),
        layer: Some(a.set_by.clone()),
        required: a.required,
    })
}

fn knowledge_kind_of(k: &FoldedKind) -> StageKnowledgeKind {
    StageKnowledgeKind {
        stage: k.stage.clone(),
        kind: k.kind.clone(),
        required: k.required,
        description: k.description.clone(),
        layer: Some(k.set_by.clone()),
    }
}

/// Fold the journey content of `dept` (`industry_dept/<i>/<d>`) of `sel` over `layers` + `rows`.
fn assemble(layers: &[LayerMeta], rows: &[Row], sel: &Selection, dept: &str) -> Body {
    let steps = fold_order(layers, sel, None, &[]);
    let rows_by_layer = by_layer(rows);
    let items = |kind: Kind| fold_items(&steps, &rows_by_layer, kind);

    // the stage scope: a stage the department (or its archetype, or the industry) puts out of scope
    let mut out_of_scope: BTreeMap<String, bool> = BTreeMap::new();
    for it in items(Kind::StageScope) {
        if let Some(f) = parse::<StageScopeFields>(&it, Kind::StageScope) {
            out_of_scope.insert(it.key, f.out_of_scope);
        }
    }

    let mut themes: Vec<(i32, JourneyTheme)> = Vec::new();
    for (i, it) in items(Kind::JourneyTheme).iter().enumerate() {
        let (Some(key), Some(f)) = (
            slug(&it.key),
            parse::<JourneyThemeFields>(it, Kind::JourneyTheme),
        ) else {
            continue;
        };
        let order = f.order.unwrap_or(i32::try_from(i).unwrap_or(i32::MAX) + 1);
        themes.push((
            order,
            JourneyTheme {
                key,
                label: f.label,
                order,
                escalation_weight: f.escalation_weight.unwrap_or(0.0),
                layer: Some(it.layer.clone()),
            },
        ));
    }
    themes.sort_by(|a, b| (a.0, a.1.key.as_str()).cmp(&(b.0, b.1.key.as_str())));

    let mut stages: Vec<(i32, JourneyStage, Vec<StageMessage>)> = Vec::new();
    for (i, it) in items(Kind::JourneyStage).iter().enumerate() {
        let (Some(key), Some(f)) = (
            slug(&it.key),
            parse::<JourneyStageFields>(it, Kind::JourneyStage),
        ) else {
            continue;
        };
        let order = f.order.unwrap_or(i32::try_from(i).unwrap_or(i32::MAX) + 1);
        let messages = f
            .messages
            .iter()
            .flatten()
            .filter(|(_, u)| !u.is_empty())
            .filter_map(|(channel, utterances)| {
                let channel: ChannelKey =
                    serde_json::from_value(Value::String(channel.clone())).ok()?;
                Some(StageMessage {
                    stage: key.clone(),
                    channel,
                    utterances: utterances.clone(),
                    layer: Some(it.layer.clone()),
                })
            })
            .collect();
        stages.push((
            order,
            JourneyStage {
                out_of_scope: out_of_scope.get(&it.key).copied().unwrap_or(false),
                label: f.label,
                order,
                description: f.description.unwrap_or_default(),
                synonyms: f.synonyms.unwrap_or_default(),
                themes: f.themes.iter().flatten().filter_map(|t| slug(t)).collect(),
                layer: Some(it.layer.clone()),
                key,
            },
            messages,
        ));
    }
    stages.sort_by(|a, b| (a.0, a.1.key.as_str()).cmp(&(b.0, b.1.key.as_str())));
    let stage_pos: BTreeMap<String, usize> = stages
        .iter()
        .enumerate()
        .map(|(i, (_, s, _))| (s.key.as_str().to_owned(), i))
        .collect();

    let mut priors: Vec<ThemeStagePrior> = Vec::new();
    for it in items(Kind::ThemeStagePrior) {
        let Some((theme, stage)) = it.key.split_once('|') else {
            continue;
        };
        let (Some(theme), Some(stage), Some(f)) = (
            slug(theme),
            slug(stage),
            parse::<ThemeStagePriorFields>(&it, Kind::ThemeStagePrior),
        ) else {
            continue;
        };
        if let Some(weight) = f.weight {
            priors.push(ThemeStagePrior {
                theme,
                stage,
                weight,
                layer: Some(it.layer.clone()),
            });
        }
    }

    let mut channels: Vec<(usize, JourneyChannel)> = Vec::new();
    for it in items(Kind::JourneyChannel) {
        let Some(pos) = CHANNEL_KEYS.iter().position(|c| *c == it.key) else {
            continue;
        };
        let (Ok(key), Some(f)) = (
            serde_json::from_value::<ChannelKey>(Value::String(it.key.clone())),
            parse::<JourneyChannelFields>(&it, Kind::JourneyChannel),
        ) else {
            continue;
        };
        channels.push((
            pos,
            JourneyChannel {
                key,
                label: f.label,
                status: match f.status {
                    None | Some(ChannelStatusField::V1) => ChannelStatus::V1,
                    Some(ChannelStatusField::Later) => ChannelStatus::Later,
                    Some(ChannelStatusField::OutOfScope) => ChannelStatus::OutOfScope,
                },
                note: f.note.unwrap_or_default(),
                layer: Some(it.layer.clone()),
            },
        ));
    }
    channels.sort_by_key(|(pos, _)| *pos);

    // activities and knowledge kinds: the compile's knowledge fold, of this department
    let (knowledge, errors) = fold_knowledge(layers, rows, sel, None);
    for e in &errors {
        tracing::warn!(code = %e.code, key = %e.key, "journey grid knowledge fold: {}", e.message);
    }
    let mut activities: Vec<JourneyActivity> = knowledge
        .activities
        .iter()
        .filter(|a| a.of_department(dept))
        .filter_map(activity_of)
        .collect();
    activities.sort_by(|a, b| {
        let pos = |s: &Slug| stage_pos.get(s.as_str()).copied().unwrap_or(usize::MAX);
        (pos(&a.stage), a.key.as_str()).cmp(&(pos(&b.stage), b.key.as_str()))
    });
    let knowledge_kinds = knowledge
        .kinds_of_department(dept)
        .into_iter()
        .map(knowledge_kind_of)
        .collect();

    Body {
        themes: themes.into_iter().map(|(_, t)| t).collect(),
        messages: stages
            .iter()
            .flat_map(|(_, _, m)| m.iter().cloned())
            .collect(),
        stages: stages.into_iter().map(|(_, s, _)| s).collect(),
        priors,
        channels: channels.into_iter().map(|(_, c)| c).collect(),
        activities,
        knowledge_kinds,
    }
}

// --- the catalogue the rules check against ------------------------------------------------------------------------

/// The rules' [`Catalogue`] and the industry's model objects from a compile of its selection (head or pinned).
/// Model objects are the compiled objects that a layer above `base` defines (what the industry, its profile and
/// its department ADD to the canonical model; rule 3 asks that each is used). Operations are the `*` rows
/// (variants are named in the activity's `variant`): level = `identity_level`, modality, and `lands` = the object
/// is `case` or declares a non-terminal state (a case type or a pending state, rule 6).
pub fn catalogue_of(c: &Compiled) -> (Catalogue, Vec<String>) {
    let mut cat = Catalogue::default();
    let mut lands: BTreeSet<String> = BTreeSet::new();
    let mut model = Vec::new();
    for it in &c.catalogue.objects {
        let parsed: Option<ObjectFields> =
            serde_json::from_value(Value::Object(it.fields.clone())).ok();
        let (fields, has_state) = match parsed {
            Some(f) => (
                f.fields.iter().map(|x| x.field.clone()).collect(),
                f.states.iter().any(|s| !s.terminal),
            ),
            None => (BTreeSet::new(), false),
        };
        if it.key == "case" || has_state {
            lands.insert(it.key.clone());
        }
        if it.layer != "base" {
            model.push(it.key.clone());
        }
        cat.objects.insert(it.key.clone(), fields);
    }
    for op in c
        .catalogue
        .operations
        .iter()
        .filter(|o| o.variant.is_empty())
    {
        cat.operations.insert(
            op.key.clone(),
            OpInfo {
                level: attr_text(op, "identity_level"),
                modality: attr_text(op, "modality"),
                lands: lands.contains(&op.object),
            },
        );
    }
    (cat, model)
}

fn attr_text(op: &OperationRow, name: &str) -> String {
    op.attrs
        .get(name)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Findings of `j`: all rules with a catalogue, else rules 3 and 4 only.
fn findings_of(j: &IndustryJourney, cat: Option<&Catalogue>) -> Vec<ValidationFinding> {
    match cat {
        Some(cat) => rules::industry_findings(j, cat),
        None => {
            let mut f = rules::rule4(j);
            f.extend(rules::rule3(j));
            f
        }
    }
}

// --- the selection -------------------------------------------------------------------------------------------------

/// The business-model profile of `industry` (the first current `profile` row naming it), by layer then key.
fn profile_of(rows: &[Row], industry: &str) -> Option<String> {
    rows.iter()
        .find(|r| {
            r.kind == Kind::Profile
                && r.op != Op::Tombstone
                && r.fields.get("industry").and_then(Value::as_str) == Some(industry)
        })
        .map(|r| r.key.clone())
}

fn selection_for(industry: &str, department: &str, profile: Option<String>) -> Selection {
    Selection {
        profile,
        industry: industry.to_owned(),
        departments: vec![department_key(industry, department)],
        ..Selection::default()
    }
}

// --- store → compile ----------------------------------------------------------------------------------------------

/// A layer as the compile reads it.
fn layer_meta(l: &Layer) -> ApiResult<LayerMeta> {
    let key = LayerKey::parse_label(&l.key).map_err(|e| {
        ApiError::Internal(anyhow::anyhow!(
            "platform store: layer {} has an unreadable key: {e}",
            l.key
        ))
    })?;
    Ok(LayerMeta {
        id: l.id.to_string(),
        key,
        extends: l.extends.clone(),
        tag: None,
        tagged_at: None,
    })
}

/// A stored row as the compile reads it; `None` (logged) for a kind or op this build does not know.
fn compile_row(r: &KeyedRow) -> Option<Row> {
    let (Some(kind), Some(op)) = (
        Kind::parse(&r.kind),
        match r.op.as_str() {
            "add" => Some(Op::Add),
            "patch" => Some(Op::Patch),
            "tombstone" => Some(Op::Tombstone),
            _ => None,
        },
    ) else {
        tracing::warn!(kind = %r.kind, op = %r.op, "a journey row has an unknown kind or op; skipped");
        return None;
    };
    Some(Row {
        id: r.id.to_string(),
        layer_id: r.layer_id.to_string(),
        kind,
        key: r.key.clone(),
        op,
        fields: r.fields.clone(),
        rev: r.rev,
    })
}

/// The compile's bytewise (layer key, kind, key) order, whatever the database collation.
fn sort_bytewise(rows: &mut [KeyedRow]) {
    rows.sort_by(|a, b| {
        (a.layer_key.as_str(), a.kind.as_str(), a.key.as_str()).cmp(&(
            b.layer_key.as_str(),
            b.kind.as_str(),
            b.key.as_str(),
        ))
    });
}

fn journey_kind_names() -> Vec<String> {
    JOURNEY_KINDS
        .iter()
        .map(|k| k.as_str().to_owned())
        .collect()
}

/// The platform model for the grid readers: layers, the journey and knowledge rows (current), and the compile
/// input pieces (`templates::head_model`).
struct Head {
    model: templates::HeadModel,
    layers: Vec<Layer>,
}

async fn read_head(conn: &mut PgConnection) -> ApiResult<Head> {
    let model = templates::head_model(&mut *conn).await?;
    let layers = layers::layers(&mut *conn).await?;
    Ok(Head { model, layers })
}

impl Head {
    fn layer(&self, key: &str) -> Option<&Layer> {
        self.layers.iter().find(|l| l.key == key)
    }
}

/// What [`build`] returns: the grid and how many journey rows the industry and department layers hold themselves
/// (0 = nobody started this journey).
struct Built {
    journey: IndustryJourney,
    own_rows: usize,
}

/// The grid of (`industry`, `department`) over the current model.
async fn build(
    conn: &mut PgConnection,
    head: &Head,
    industry: &str,
    department: &str,
) -> ApiResult<Built> {
    let ind_key = LayerKey::industry(industry).label();
    let dept_key = department_key(industry, department);
    let (Some(ind), Some(dept)) = (head.layer(&ind_key), head.layer(&dept_key)) else {
        return Err(not_found(industry, department));
    };
    let model = &head.model;
    let sel = selection_for(industry, department, profile_of(&model.rows, industry));

    let body = assemble(&model.layers, &model.rows, &sel, &dept_key);

    let catalogue = match compile::compile_as(&model.input(), &sel, None, Mode::Job) {
        Ok(c) => Some(catalogue_of(&c)),
        Err(errors) => {
            tracing::warn!(
                industry,
                department,
                first = ?errors.first().map(|e| (&e.code, &e.key)),
                "the industry's selection does not compile; journey rules 1, 2, 5 and 6 skipped"
            );
            None
        }
    };

    let tag = db_tags::latest_tag(&mut *conn, &ind.family).await?;
    let dept_tag = db_tags::latest_tag(&mut *conn, &dept.family).await?;
    let mut draft_rev = 0usize;
    let mut families = vec![ind.family.as_str()];
    if dept.family != ind.family {
        families.push(dept.family.as_str());
    }
    for f in families {
        draft_rev += untagged_rows(&mut *conn, f)
            .await?
            .iter()
            .filter(|r| JOURNEY_KINDS.iter().any(|k| k.as_str() == r.kind))
            .count();
    }
    let draft_rev = u32::try_from(draft_rev).unwrap_or(u32::MAX);
    let tag = tag.map(|t| tag_name(&t.family, t.version));
    let dept_tag = dept_tag.map(|t| tag_name(&t.family, t.version));

    // the newest journey revision of the two layers
    let newest: Option<(DateTime<Utc>, String)> = sqlx::query_as(
        "SELECT created_at, author FROM layer_head \
          WHERE layer_key = ANY($1) AND kind = ANY($2) \
          ORDER BY created_at DESC, id LIMIT 1",
    )
    .bind(vec![ind_key.clone(), dept_key.clone()])
    .bind(journey_kind_names())
    .fetch_optional(&mut *conn)
    .await?;
    let (updated_at, updated_by) = newest.unwrap_or_else(|| (ind.created_at, "system".to_owned()));
    let source_run = source_run_of(&updated_by);

    let own_ids = [ind.id.to_string(), dept.id.to_string()];
    let own_rows = model
        .rows
        .iter()
        .filter(|r| own_ids.contains(&r.layer_id) && JOURNEY_KINDS.contains(&r.kind))
        .count();

    let (cat, model_objects) = match catalogue {
        Some((c, m)) => (Some(c), m),
        None => (None, Vec::new()),
    };
    let mut journey = IndustryJourney {
        industry: industry.to_owned(),
        label: ind.label.clone().unwrap_or_else(|| industry.to_owned()),
        department: department.to_owned(),
        status: journey_status(tag.as_deref(), dept_tag.as_deref(), draft_rev),
        tag,
        dept_tag,
        draft_rev,
        updated_at,
        updated_by,
        themes: body.themes,
        stages: body.stages,
        priors: body.priors,
        channels: body.channels,
        activities: body.activities,
        messages: body.messages,
        knowledge_kinds: body.knowledge_kinds,
        model_objects,
        findings: Vec::new(),
        source_run,
    };
    journey.findings = findings_of(&journey, cat.as_ref());
    Ok(Built { journey, own_rows })
}

/// The generator run that wrote a revision: an author `generator:<run id>` (the packaging step's convention).
fn source_run_of(author: &str) -> Option<i64> {
    author.strip_prefix("generator:")?.parse().ok()
}

// --- public ---------------------------------------------------------------------------------------------------------

/// The grid of (`industry`, `department`) over the CURRENT rows (untagged revisions included). 404
/// `journey_not_found` for an unknown industry or department.
pub async fn load(
    conn: &mut PgConnection,
    industry: &str,
    department: &str,
) -> ApiResult<IndustryJourney> {
    let head = read_head(&mut *conn).await?;
    Ok(build(&mut *conn, &head, industry, department)
        .await?
        .journey)
}

/// The grid of `department` (bare key) for a job's template `pins` and `sel` (its industry): the same shape from
/// the members of the pinned tags only — layers of the pinned families that existed at their tag, nothing newer.
/// `status` is `tagged`, `draft_rev` 0, `tag` / `dept_tag` are the pins of the industry and the department layer,
/// `updated_*` the newest of those tags. 404 `tag_not_found` (a pin naming no tag), 404 `journey_not_found` (the
/// pins do not hold the industry or department layer).
pub async fn load_at(
    conn: &mut PgConnection,
    pins: &[TemplatePin],
    sel: &Selection,
    department: &str,
) -> ApiResult<IndustryJourney> {
    let industry = sel.industry.as_str();
    let ind_key = LayerKey::industry(industry).label();
    let dept_key = department_key(industry, department);

    let mut tags = BTreeMap::new();
    for p in pins {
        let t = db_tags::tag(&mut *conn, &p.layer, p.version).await?;
        tags.insert(p.layer.clone(), t);
    }
    let all = layers::layers(&mut *conn).await?;
    let mut pinned: Vec<&Layer> = all
        .iter()
        .filter(|l| {
            tags.get(&l.family)
                .is_some_and(|t| l.created_at <= t.created_at)
        })
        .collect();
    pinned.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
    let metas = pinned
        .iter()
        .map(|l| layer_meta(l))
        .collect::<ApiResult<Vec<_>>>()?;
    let (Some(ind), Some(dept)) = (
        pinned.iter().find(|l| l.key == ind_key),
        pinned.iter().find(|l| l.key == dept_key),
    ) else {
        return Err(not_found(industry, department));
    };

    let mut members: Vec<KeyedRow> = Vec::new();
    for t in tags.values() {
        members.extend(
            db_tags::tag_members(&mut *conn, &t.family, t.version)
                .await?
                .into_iter()
                .filter(|m| READ_KINDS.iter().any(|k| k.as_str() == m.kind)),
        );
    }
    sort_bytewise(&mut members);
    let rows: Vec<Row> = members.iter().filter_map(compile_row).collect();

    let one = Selection {
        departments: vec![dept_key.clone()],
        region: None,
        ..sel.clone()
    };
    let body = assemble(&metas, &rows, &one, &dept_key);

    let (cat, model_objects) = match templates::compile_pinned(&mut *conn, pins, &one, None).await {
        Ok(Ok(c)) => {
            let (c, m) = catalogue_of(&c);
            (Some(c), m)
        }
        _ => {
            tracing::warn!(
                industry,
                department,
                "the pinned selection does not compile; journey rules 1, 2, 5 and 6 skipped"
            );
            (None, Vec::new())
        }
    };

    let tag = tags.get(&ind.family);
    let dept_tag = tags.get(&dept.family);
    let newest = [tag, dept_tag]
        .into_iter()
        .flatten()
        .max_by_key(|t| t.tagged_at);
    let (updated_at, updated_by) = newest
        .map(|t| (t.tagged_at, t.tagged_by.clone()))
        .unwrap_or_else(|| (ind.created_at, "system".to_owned()));
    let tag = tag.map(|t| tag_name(&t.family, t.version));
    let dept_tag = dept_tag.map(|t| tag_name(&t.family, t.version));
    let mut journey = IndustryJourney {
        industry: industry.to_owned(),
        label: ind.label.clone().unwrap_or_else(|| industry.to_owned()),
        department: department.to_owned(),
        status: journey_status(tag.as_deref(), dept_tag.as_deref(), 0),
        tag,
        dept_tag,
        draft_rev: 0,
        updated_at,
        source_run: source_run_of(&updated_by),
        updated_by,
        themes: body.themes,
        stages: body.stages,
        priors: body.priors,
        channels: body.channels,
        activities: body.activities,
        messages: body.messages,
        knowledge_kinds: body.knowledge_kinds,
        model_objects,
        findings: Vec::new(),
    };
    journey.findings = findings_of(&journey, cat.as_ref());
    Ok(journey)
}

/// A summary status: a journey's, or `not_started` (nothing authored in the industry or department layer yet).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryStatus {
    Tagged,
    Partial,
    Draft,
    NotStarted,
}

impl From<JourneyStatus> for SummaryStatus {
    fn from(s: JourneyStatus) -> Self {
        match s {
            JourneyStatus::Tagged => Self::Tagged,
            JourneyStatus::Partial => Self::Partial,
            JourneyStatus::Draft => Self::Draft,
        }
    }
}

/// One department of an industry in [`IndustrySummary`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DepartmentSummary {
    /// The bare department key (`customer_service`).
    pub department: String,
    pub label: String,
    pub status: SummaryStatus,
    pub dept_tag: Option<String>,
    pub activities: u32,
    pub red: u32,
}

/// `journeyIndustries` row (web `IndustryJourneySummary` + the departments). `activities` counts the distinct
/// activities over the departments, `red` the distinct red findings; `status` is `not_started` when no department
/// has been started, else `tagged` when every started one is, `draft` when none is tagged at all, else `partial`.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct IndustrySummary {
    pub industry: String,
    pub label: String,
    pub status: SummaryStatus,
    /// `industry/retail@v2`; null when never tagged.
    pub tag: Option<String>,
    pub activities: u32,
    pub red: u32,
    pub departments: Vec<DepartmentSummary>,
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// Every industry (the `industry` layers, region sub-layers left out), by key, with its departments (the
/// `industry_dept` layers) — industries nobody started included.
pub async fn industries(conn: &mut PgConnection) -> ApiResult<Vec<IndustrySummary>> {
    let head = read_head(&mut *conn).await?;
    let mut out = Vec::new();
    let mut inds: Vec<&Layer> = head
        .layers
        .iter()
        .filter(|l| l.scope == "industry" && l.region.is_none())
        .collect();
    inds.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
    for ind in inds {
        let Some(industry) = ind.industry.as_deref() else {
            continue;
        };
        let mut depts: Vec<&Layer> = head
            .layers
            .iter()
            .filter(|l| {
                l.scope == "industry_dept"
                    && l.region.is_none()
                    && l.industry.as_deref() == Some(industry)
            })
            .collect();
        depts.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
        let mut departments = Vec::new();
        let mut acts: BTreeSet<String> = BTreeSet::new();
        let mut reds: BTreeSet<(u8, String, String, String)> = BTreeSet::new();
        let mut statuses: Vec<JourneyStatus> = Vec::new();
        for d in depts {
            let Some(department) = d.department.as_deref() else {
                continue;
            };
            let built = build(&mut *conn, &head, industry, department).await?;
            let j = &built.journey;
            let red: Vec<_> = j
                .findings
                .iter()
                .filter(|f| f.severity == Severity::Red)
                .collect();
            let started = built.own_rows > 0;
            if started {
                statuses.push(j.status);
                acts.extend(j.activities.iter().map(|a| a.key.clone()));
                reds.extend(red.iter().map(|f| {
                    (
                        f.rule,
                        f.stage.clone().unwrap_or_default(),
                        f.activity.clone().unwrap_or_default(),
                        f.message.clone(),
                    )
                }));
            }
            departments.push(DepartmentSummary {
                department: department.to_owned(),
                label: d.label.clone().unwrap_or_else(|| department.to_owned()),
                status: if started {
                    j.status.into()
                } else {
                    SummaryStatus::NotStarted
                },
                dept_tag: j.dept_tag.clone(),
                activities: if started {
                    count(j.activities.len())
                } else {
                    0
                },
                red: if started { count(red.len()) } else { 0 },
            });
        }
        let status = if statuses.is_empty() {
            SummaryStatus::NotStarted
        } else if statuses.iter().all(|s| *s == JourneyStatus::Tagged) {
            SummaryStatus::Tagged
        } else if statuses.iter().all(|s| *s == JourneyStatus::Draft) {
            SummaryStatus::Draft
        } else {
            SummaryStatus::Partial
        };
        let tag = db_tags::latest_tag(&mut *conn, &ind.family)
            .await?
            .map(|t| tag_name(&t.family, t.version));
        out.push(IndustrySummary {
            industry: industry.to_owned(),
            label: ind.label.clone().unwrap_or_else(|| industry.to_owned()),
            status,
            tag,
            activities: count(acts.len()),
            red: count(reds.len()),
            departments,
        });
    }
    Ok(out)
}
