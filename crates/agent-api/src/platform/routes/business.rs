//! B6b platform-model area `business` (plan B6 §5): business models (P12) and dimensions (P13).
//!
//! - T10: `businessAxes` GET `/api/axes`, `addAxisValue` POST `/api/axes/{key}/values`, `axisRules` GET
//!   `/api/axis-rules`, `createAxisRule` POST `/api/axis-rules`, `businessProfiles` GET `/api/profiles`,
//!   `saveBusinessProfile` PUT `/api/profiles/{key}`, `renewalPresets` GET `/api/axes/renewal-presets`,
//!   `platformDimensions` GET `/api/dimensions`, `addDimensionValue` POST `/api/dimensions/{key}/values`.
//! - T12e (F-25–F-27, migration 0160): `createAxis` POST `/api/axes`, `retireAxisValue` POST
//!   `/api/axes/{key}/values/{value}/retire`, `setAxisCardinality` PATCH `/api/axes/{key}`, `createDimension` POST
//!   `/api/dimensions`.
//! - T12g (D99): `createAxisRule` takes `layer` (`base`, default, or an industry layer) = the rule's tagging family.
//!
//! - KH bucket (Revision 2026-10-22): `departmentArchetypes` GET `/api/department-archetypes` (D60/D107: each base
//!   archetype, the industry departments that `extends` it, and per department the knowledge kinds and activities
//!   KH's fold compiles into it — the archetype's before the department's own), `platformDimensionUsage` GET
//!   `/api/dimensions/usage` (port of the fixture `dimensionUsage()`: per standard value its job × dimension layers,
//!   the context spaces of an area, and the tenant job selections (`job_target`) and releases that name it, read
//!   tenant by tenant through `compile::tenant_jobs`, R3; KH selection shape: `departments`, one `region`).
//!
//! Vocab writes are insert-only (409 on an existing key, retired or not) and record one platform audit row. Rev
//! writes (`createAxisRule`, `saveBusinessProfile`) go through `write_authored` (its `layer.revision` rows) plus one
//! `business_model.*` row. Every write that reads or changes which axis values are in use takes the business
//! lock ([`store::lock_business`]) first, so a retirement cannot race a profile or rule naming the value.
use std::collections::{BTreeMap, BTreeSet};

use agent_model::axis::{self, Condition};
use agent_model::kinds::profile::{ProfileFields, check_profile_axes};
use agent_model::kinds::{Kind, Op, Refusal};
use agent_model::layer::LayerKey;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, patch, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;

use super::{
    Expect, LayerMeta, Who, audit, blocked, check_label, conflict, missing, read_tx, unprocessable,
    write_tx,
};
use crate::audit::PlatformEvent;
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::platform::db::authored::{current_revision, insert_layer, write_authored};
use crate::platform::db::{layers, rows};
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

mod store;

use store::{AxisOut, DimensionOut, ProfileRow, RuleOut};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/axes", get(business_axes).post(create_axis))
        .route("/api/axes/renewal-presets", get(renewal_presets))
        .route("/api/axes/{key}", patch(set_axis_cardinality))
        .route("/api/axes/{key}/values", post(add_axis_value))
        .route(
            "/api/axes/{key}/values/{value}/retire",
            post(retire_axis_value),
        )
        .route("/api/axis-rules", get(axis_rules).post(create_axis_rule))
        .route("/api/profiles", get(business_profiles))
        .route("/api/profiles/{key}", put(save_business_profile))
        .route(
            "/api/dimensions",
            get(platform_dimensions).post(create_dimension),
        )
        .route("/api/dimensions/{key}/values", post(add_dimension_value))
        .route("/api/dimensions/usage", get(platform_dimension_usage))
        .route("/api/department-archetypes", get(department_archetypes))
}

// --- key grammars (the web fixture's, so fixture and live refuse alike) ------------------------------------------

/// `^[a-z][a-z0-9_]{0,40}$`: an axis value leaf.
fn is_leaf(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some('a'..='z'))
        && s.len() <= 41
        && c.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// An axis key: one or two dotted leaves (`order_type`, `offering.availability` — the seeded axes use the dotted
/// form, so the plan's single-leaf grammar is widened to it).
fn is_axis_key(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() <= 2 && parts.iter().all(|p| is_leaf(p))
}

/// `^[a-z][a-z0-9_]{1,62}$` (web `SLUG`): rule and profile keys.
fn is_slug(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some('a'..='z'))
        && (2..=63).contains(&s.len())
        && c.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `^[a-z][a-z0-9_-]{0,40}$` (web `DIM_SLUG`): dimension keys and values.
fn is_dim_slug(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some('a'..='z'))
        && s.len() <= 41
        && c.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// `^[A-Z][A-Za-z0-9]{1,40}$`: the object a rule adds.
fn is_entity(s: &str) -> bool {
    let mut c = s.chars();
    matches!(c.next(), Some('A'..='Z'))
        && (2..=41).contains(&s.len())
        && c.all(|c| c.is_ascii_alphanumeric())
}

fn label_of(raw: &str) -> ApiResult<String> {
    check_label(raw)?;
    Ok(raw.trim().to_string())
}

fn refusal(r: Refusal) -> ApiError {
    match r.status {
        404 => ApiError::Missing(r.code, r.message),
        409 => ApiError::Conflict(r.code, r.message),
        _ => ApiError::Unprocessable(r.code, r.message),
    }
}

fn to_json<T: Serialize>(v: &T) -> ApiResult<Value> {
    serde_json::to_value(v).map_err(|e| ApiError::Internal(e.into()))
}

/// Every (axis, value) leaf a condition names.
fn condition_leaves(c: &Condition, out: &mut Vec<(String, String)>) {
    match c {
        Condition::Eq(a, v) => out.push((a.clone(), v.clone())),
        Condition::In(a, vs) => out.extend(vs.iter().map(|v| (a.clone(), v.clone()))),
        Condition::And(cs) | Condition::Or(cs) => cs.iter().for_each(|c| condition_leaves(c, out)),
        Condition::Not(c) => condition_leaves(c, out),
    }
}

/// The axes as the agent-model sees them for a new profile or rule: active (not retired) values only.
fn active_model(axes: &[AxisOut]) -> Vec<axis::Axis> {
    axes.iter()
        .map(|a| axis::Axis {
            key: a.key.clone(),
            cardinality: if a.cardinality == "one" {
                axis::Cardinality::One
            } else {
                axis::Cardinality::Many
            },
            values: a
                .values
                .iter()
                .filter(|v| v.active())
                .map(|v| axis::AxisValue {
                    key: v.key.clone(),
                    parent: v.parent.clone(),
                    label: None,
                    description: None,
                })
                .collect(),
            label: None,
            description: None,
        })
        .collect()
}

// --- axes -----------------------------------------------------------------------------------------------------

async fn business_axes(
    _sa: SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Vec<AxisOut>>> {
    let mut tx = read_tx(&st).await?;
    Ok(Json(store::axes(&mut tx).await?))
}

async fn renewal_presets(
    _sa: SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Vec<store::RenewalPresetOut>>> {
    let mut tx = read_tx(&st).await?;
    Ok(Json(store::renewal_presets(&mut tx).await?))
}

async fn axis_or_404(conn: &mut PgConnection, key: &str) -> ApiResult<AxisOut> {
    store::axis(conn, key)
        .await?
        .ok_or_else(|| missing("axis_not_found", format!("No axis {key}.")))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AxisIn {
    key: String,
    label: String,
    #[serde(default)]
    description: String,
    cardinality: String,
    object: String,
    #[serde(default)]
    milestone: Option<String>,
}

fn check_cardinality(c: &str) -> ApiResult<()> {
    if matches!(c, "one" | "many") {
        Ok(())
    } else {
        Err(unprocessable(
            "invalid_cardinality",
            format!("“{c}” is not a cardinality (one or many)."),
        ))
    }
}

/// `createAxis` (F-25): 422 `invalid_axis_key`, `invalid_label`, `invalid_cardinality`, `invalid_milestone`,
/// `unknown_object`; 409 `axis_exists`. Audit `business_model.axis_create`.
async fn create_axis(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<AxisIn>,
) -> ApiResult<(StatusCode, Json<AxisOut>)> {
    let who = Who::of(&sa);
    let key = b.key.trim();
    if !is_axis_key(key) {
        return Err(unprocessable(
            "invalid_axis_key",
            "An axis key is lower_snake_case, optionally one dotted part (offering.availability).",
        ));
    }
    let label = label_of(&b.label)?;
    check_cardinality(&b.cardinality)?;
    let milestone = b.milestone.as_deref().unwrap_or("M1");
    if !matches!(milestone, "M1" | "M4") {
        return Err(unprocessable(
            "invalid_milestone",
            "The milestone is M1 or M4.",
        ));
    }
    let mut tx = write_tx(&st).await?;
    let objects = store::catalogue_objects(&mut tx).await?;
    if !objects.contains(b.object.trim()) {
        return Err(unprocessable(
            "unknown_object",
            format!("“{}” is not a catalogue object.", b.object.trim()),
        )
        .with("objects", objects.into_iter().collect::<Vec<_>>()));
    }
    let inserted = store::insert_axis(
        &mut tx,
        &store::NewAxis {
            key,
            label: &label,
            description: b.description.trim(),
            cardinality: &b.cardinality,
            object: b.object.trim(),
            milestone,
        },
    )
    .await?;
    if !inserted {
        return Err(conflict(
            "axis_exists",
            format!("An axis {key} exists already."),
        ));
    }
    audit(
        &mut tx,
        &who,
        PlatformEvent::new("business_model.axis_create", key)
            .detail("axis", key)
            .detail("cardinality", b.cardinality.as_str())
            .detail("object", b.object.trim()),
    )
    .await?;
    let out = axis_or_404(&mut tx, key).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AxisValueIn {
    key: String,
    label: String,
    #[serde(default)]
    parent: Option<String>,
}

/// `addAxisValue`: 404 `axis_not_found`; 422 `invalid_value_key`, `unknown_parent` (absent or retired),
/// `invalid_label`; 409 `value_exists` (insert-only, retired values included). Audit `business_model.axis_value_add`.
async fn add_axis_value(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(axis_key): ApiPath<String>,
    ApiJson(b): ApiJson<AxisValueIn>,
) -> ApiResult<(StatusCode, Json<AxisOut>)> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    store::lock_business(&mut tx).await?;
    let a = axis_or_404(&mut tx, &axis_key).await?;
    let parent = b.parent.as_deref().map(str::trim).filter(|p| !p.is_empty());
    let leaf = b.key.trim();
    if !is_leaf(leaf) {
        return Err(unprocessable(
            "invalid_value_key",
            "A value key is lower_snake_case.",
        ));
    }
    if let Some(p) = parent
        && !a.values.iter().any(|v| v.key == p && v.active())
    {
        return Err(unprocessable(
            "unknown_parent",
            format!("No value {p} on {}.", a.key),
        ));
    }
    let key = match parent {
        Some(p) => format!("{p}.{leaf}"),
        None => leaf.to_string(),
    };
    if a.values.iter().any(|v| v.key == key) {
        return Err(value_exists(&a.key, &key));
    }
    let label = label_of(&b.label)?;
    if !store::insert_axis_value(&mut tx, &a.key, &key, &label, parent, &who.by).await? {
        return Err(value_exists(&a.key, &key));
    }
    audit(
        &mut tx,
        &who,
        PlatformEvent::new(
            "business_model.axis_value_add",
            format!("{} · {key}", a.key),
        )
        .detail("axis", a.key.as_str())
        .detail("value", key.as_str())
        .detail("parent", parent),
    )
    .await?;
    let out = axis_or_404(&mut tx, &axis_key).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

fn value_exists(axis: &str, key: &str) -> ApiError {
    conflict(
        "value_exists",
        format!("{axis} already has {key}; axis values are insert-only."),
    )
}

/// `retireAxisValue` (F-26): 404 `axis_not_found`, `value_not_found`; 409 `value_retired`, `value_in_use` with
/// `blockers[]: {kind: profile | axis_rule | child_value, key}`. Audit `business_model.axis_value_retire`.
async fn retire_axis_value(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((axis_key, value)): ApiPath<(String, String)>,
) -> ApiResult<Json<AxisOut>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    store::lock_business(&mut tx).await?;
    let a = axis_or_404(&mut tx, &axis_key).await?;
    let Some(v) = a.values.iter().find(|v| v.key == value) else {
        return Err(missing(
            "value_not_found",
            format!("{axis_key} has no value {value}."),
        ));
    };
    if !v.active() {
        return Err(conflict(
            "value_retired",
            format!("{axis_key} · {value} is retired already."),
        ));
    }
    let mut blockers: Vec<Value> = Vec::new();
    for p in store::profiles_holding(&mut tx, &axis_key, &value).await? {
        blockers.push(json!({"kind": "profile", "key": p.key, "layer": p.layer_key}));
    }
    let mut rule_keys = BTreeSet::new();
    for (rule, cond) in store::rule_conditions(&mut tx).await? {
        let Ok(c) = serde_json::from_value::<Condition>(cond) else {
            continue;
        };
        let mut leaves = Vec::new();
        condition_leaves(&c, &mut leaves);
        if leaves.iter().any(|(ax, v)| *ax == axis_key && *v == value) {
            rule_keys.insert(rule);
        }
    }
    blockers.extend(
        rule_keys
            .into_iter()
            .map(|k| json!({"kind": "axis_rule", "key": k})),
    );
    blockers.extend(
        a.values
            .iter()
            .filter(|c| c.active() && c.parent.as_deref() == Some(value.as_str()))
            .map(|c| json!({"kind": "child_value", "key": c.key})),
    );
    if !blockers.is_empty() {
        return Err(blocked(
            "value_in_use",
            format!("{axis_key} · {value} is still in use; retire or change what names it first."),
            "blockers",
            blockers,
        ));
    }
    store::retire_axis_value(&mut tx, &axis_key, &value, &who.by).await?;
    audit(
        &mut tx,
        &who,
        PlatformEvent::new(
            "business_model.axis_value_retire",
            format!("{axis_key} · {value}"),
        )
        .detail("axis", axis_key.as_str())
        .detail("value", value.as_str()),
    )
    .await?;
    let out = axis_or_404(&mut tx, &axis_key).await?;
    tx.commit().await?;
    Ok(Json(out))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CardinalityIn {
    cardinality: String,
}

/// `setAxisCardinality` (F-26): one → many always; many → one 409 `cardinality_in_use` (`blockers[]`: the head
/// profiles holding more than one value). 404 `axis_not_found`; 422 `invalid_cardinality`. Audit
/// `business_model.axis_cardinality`.
async fn set_axis_cardinality(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(axis_key): ApiPath<String>,
    ApiJson(b): ApiJson<CardinalityIn>,
) -> ApiResult<Json<AxisOut>> {
    let who = Who::of(&sa);
    check_cardinality(&b.cardinality)?;
    let mut tx = write_tx(&st).await?;
    store::lock_business(&mut tx).await?;
    let Some(from) = store::lock_axis(&mut tx, &axis_key).await? else {
        return Err(missing("axis_not_found", format!("No axis {axis_key}.")));
    };
    if b.cardinality == "one" && from != "one" {
        let many = store::profiles_with_many(&mut tx, &axis_key).await?;
        if !many.is_empty() {
            let blockers: Vec<Value> = many
                .into_iter()
                .map(|p| json!({"kind": "profile", "key": p.key, "layer": p.layer_key}))
                .collect();
            return Err(blocked(
                "cardinality_in_use",
                format!("Business models hold more than one {axis_key} value; narrow them first."),
                "blockers",
                blockers,
            ));
        }
    }
    store::set_cardinality(&mut tx, &axis_key, &b.cardinality).await?;
    audit(
        &mut tx,
        &who,
        PlatformEvent::new("business_model.axis_cardinality", axis_key.as_str())
            .detail("axis", axis_key.as_str())
            .detail("from", from.as_str())
            .detail("to", b.cardinality.as_str()),
    )
    .await?;
    let out = axis_or_404(&mut tx, &axis_key).await?;
    tx.commit().await?;
    Ok(Json(out))
}

// --- axis rules -----------------------------------------------------------------------------------------------

async fn axis_rules(_sa: SuperAdmin, State(st): State<AppState>) -> ApiResult<Json<Vec<RuleOut>>> {
    let mut tx = read_tx(&st).await?;
    Ok(Json(store::rules(&mut tx).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AddsIn {
    key: String,
    #[serde(default)]
    label: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleIn {
    key: String,
    priority: i32,
    condition: Condition,
    adds: AddsIn,
    /// D99: the tagging family — `base` (default) or an industry layer.
    #[serde(default)]
    layer: Option<String>,
}

/// The owner family of a new rule (D99/T12g): absent or `base` → `base`; else an existing industry layer. 422
/// `invalid_rule_layer` with `{layer}` otherwise.
async fn rule_owner(conn: &mut PgConnection, layer: Option<&str>) -> ApiResult<String> {
    let Some(l) = layer.map(str::trim).filter(|l| *l != "base") else {
        return Ok("base".into());
    };
    let bad = || {
        unprocessable(
            "invalid_rule_layer",
            format!("{l} cannot own a rule: pick base or an industry layer."),
        )
        .with("layer", l)
    };
    match layers::find_layer_by_key(conn, l).await? {
        Some(found) if found.scope == "industry" && found.is_head() => Ok(found.key),
        _ => Err(bad()),
    }
}

/// `createAxisRule` (T10 + T12g): one transaction — the `profile/<key>` layer (family = owner), the `axis_rule`
/// definition, the `entity` add in the profile layer, the `axis_rule` revision in the owner family. 422
/// `invalid_rule_key`, `invalid_rule_layer`, `unknown_axis_value` (absent or retired), `invalid_entity`; 409
/// `rule_exists`. Audit `business_model.rule_create` (with `layer`).
async fn create_axis_rule(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<RuleIn>,
) -> ApiResult<(StatusCode, Json<RuleOut>)> {
    let who = Who::of(&sa);
    let key = b.key.trim().to_string();
    if !is_slug(&key) {
        return Err(unprocessable(
            "invalid_rule_key",
            "A rule key is lower_snake_case.",
        ));
    }
    let mut tx = write_tx(&st).await?;
    store::lock_business(&mut tx).await?;
    let owner = rule_owner(&mut tx, b.layer.as_deref()).await?;
    let profile_key = LayerKey::profile(&key);
    let profile_label = profile_key.label();
    if store::rule_exists(&mut tx, &key).await?
        || layers::find_layer_by_key(&mut tx, &profile_label)
            .await?
            .is_some()
    {
        return Err(conflict(
            "rule_exists",
            format!("A rule {key} exists already."),
        ));
    }
    let axes = store::axes(&mut tx).await?;
    let mut leaves = Vec::new();
    condition_leaves(&b.condition, &mut leaves);
    if leaves.is_empty() {
        return Err(unprocessable(
            "unknown_axis_value",
            "The condition names no axis value.",
        ));
    }
    for (ax, v) in &leaves {
        let known = axes
            .iter()
            .find(|a| &a.key == ax)
            .is_some_and(|a| a.values.iter().any(|x| &x.key == v && x.active()));
        if !known {
            return Err(unprocessable(
                "unknown_axis_value",
                format!("{ax} = {v} is not an axis value."),
            ));
        }
    }
    let entity = b.adds.key.trim();
    if !is_entity(entity) {
        return Err(unprocessable(
            "invalid_entity",
            "The added object is PascalCase (MeterPoint).",
        ));
    }
    let entity_label = match b.adds.label.trim() {
        "" => entity.to_string(),
        l => l.to_string(),
    };
    let condition = to_json(&b.condition)?;

    let layer = insert_layer(
        &mut tx,
        &profile_key,
        Some(owner.as_str()),
        &LayerMeta::default(),
    )
    .await?;
    store::insert_rule(
        &mut tx, &key, b.priority, &condition, layer.id, &owner, &who.by,
    )
    .await?;
    write_authored(
        &mut tx,
        &who.actor,
        &profile_label,
        Kind::Entity,
        entity,
        Op::Add,
        json!({"label": entity_label, "synonyms": []}),
        &who.by,
        Expect::New,
    )
    .await?;
    write_authored(
        &mut tx,
        &who.actor,
        &owner,
        Kind::AxisRule,
        &key,
        Op::Add,
        json!({"priority": b.priority, "condition": condition, "layer": profile_label}),
        &who.by,
        Expect::New,
    )
    .await?;
    audit(
        &mut tx,
        &who,
        PlatformEvent::new("business_model.rule_create", key.as_str())
            .detail("rule", key.as_str())
            .detail("layer", owner.as_str())
            .detail("profile_layer", profile_label.as_str())
            .detail("adds", entity),
    )
    .await?;
    let out = store::rule(&mut tx, &key).await?.ok_or_else(|| {
        ApiError::Internal(anyhow::anyhow!(
            "rule {key} vanished in its own transaction"
        ))
    })?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

// --- business profiles ----------------------------------------------------------------------------------------

/// The web `BusinessProfile` plus where it lives.
#[derive(Debug, Serialize)]
struct ProfileOut {
    key: String,
    label: String,
    industry: Option<String>,
    description: String,
    selection: BTreeMap<String, Vec<String>>,
    notes: Vec<String>,
    updated_at: String,
    updated_by: String,
    /// The layer holding the row (`industry/<i>` or `base`) and its revision.
    layer: String,
    rev: i32,
}

fn profile_out(r: ProfileRow) -> ApiResult<ProfileOut> {
    let f: ProfileFields = serde_json::from_value(r.fields).map_err(|e| {
        ApiError::Internal(anyhow::anyhow!(
            "profile {} in {} is not a profile row: {e}",
            r.key,
            r.layer_key
        ))
    })?;
    Ok(ProfileOut {
        updated_at: f.updated_at.unwrap_or_else(|| r.created_at.to_rfc3339()),
        updated_by: f.updated_by.unwrap_or(r.author),
        key: r.key,
        label: f.label,
        industry: f.industry,
        description: f.description,
        selection: f.selection,
        notes: f.notes,
        layer: r.layer_key,
        rev: r.rev,
    })
}

async fn business_profiles(
    _sa: SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Vec<ProfileOut>>> {
    let mut tx = read_tx(&st).await?;
    let rows = store::profiles(&mut tx).await?;
    Ok(Json(
        rows.into_iter()
            .map(profile_out)
            .collect::<ApiResult<Vec<_>>>()?,
    ))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileIn {
    /// Optional echo of the path key (the web sends the whole profile); must match it.
    #[serde(default)]
    key: Option<String>,
    label: String,
    #[serde(default)]
    industry: Option<String>,
    #[serde(default)]
    description: String,
    selection: BTreeMap<String, Vec<String>>,
    /// Absent = keep the current notes.
    #[serde(default)]
    notes: Option<Vec<String>>,
}

/// `saveBusinessProfile`: create-or-revise the `profile` row in `industry/<industry>` (else `base`). Empty axes are
/// dropped, repeated values collapsed. 422 `invalid_profile_key`, `invalid_label`, `unknown_axis`,
/// `unknown_axis_value` (absent or retired), `empty_selection`; 404 `industry_not_found`. A profile whose industry
/// changed moves: tombstoned in its old layer, written in the new one (one current row per key). Audit
/// `business_model.save`.
async fn save_business_profile(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(key): ApiPath<String>,
    ApiJson(b): ApiJson<ProfileIn>,
) -> ApiResult<(StatusCode, Json<ProfileOut>)> {
    let who = Who::of(&sa);
    if !is_slug(&key) || b.key.as_deref().is_some_and(|k| k.trim() != key) {
        return Err(unprocessable(
            "invalid_profile_key",
            "A profile key is lower_snake_case and matches the path.",
        ));
    }
    let label = label_of(&b.label)
        .map_err(|_| unprocessable("invalid_label", "Give the business model a name."))?;
    let mut selection: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (ax, vals) in b.selection {
        let mut seen = BTreeSet::new();
        let vals: Vec<String> = vals
            .into_iter()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty() && seen.insert(v.clone()))
            .collect();
        if !vals.is_empty() {
            selection.insert(ax, vals);
        }
    }
    let industry = b
        .industry
        .map(|i| i.trim().to_string())
        .filter(|i| !i.is_empty());

    let mut tx = write_tx(&st).await?;
    store::lock_business(&mut tx).await?;
    let axes = store::axes(&mut tx).await?;
    let current = store::profiles_of(&mut tx, &key).await?;
    let mut fields = ProfileFields {
        label,
        industry: industry.clone(),
        description: b.description.trim().to_string(),
        selection,
        notes: match b.notes {
            Some(n) => n,
            None => current
                .first()
                .and_then(|r| serde_json::from_value::<ProfileFields>(r.fields.clone()).ok())
                .map(|f| f.notes)
                .unwrap_or_default(),
        },
        updated_at: None,
        updated_by: None,
        summary: None,
    };
    check_profile_axes(&fields, &active_model(&axes)).map_err(refusal)?;
    if fields.selection.is_empty() {
        return Err(unprocessable(
            "empty_selection",
            "Pick at least one axis value.",
        ));
    }
    let target = match &industry {
        Some(i) => {
            let k = LayerKey::industry(i).label();
            match layers::find_layer_by_key(&mut tx, &k).await? {
                Some(l) if l.scope == "industry" => l.key,
                _ => {
                    return Err(missing(
                        "industry_not_found",
                        format!("No industry layer {k}."),
                    ));
                }
            }
        }
        None => "base".to_string(),
    };
    fields.updated_at = Some(chrono::Utc::now().to_rfc3339());
    fields.updated_by = Some(who.by.clone());
    let value = to_json(&fields)?;

    // Settle every other head row of this key first (a moved profile): tombstone it where it was.
    for old in current.iter().filter(|r| r.layer_key != target) {
        write_authored(
            &mut tx,
            &who.actor,
            &old.layer_key,
            Kind::Profile,
            &key,
            Op::Tombstone,
            json!({"reason": format!("moved to {target}")}),
            &who.by,
            Expect::Rev(old.rev),
        )
        .await?;
    }
    let here = current.iter().find(|r| r.layer_key == target);
    let created = here.is_none();
    match here {
        Some(r) => {
            write_authored(
                &mut tx,
                &who.actor,
                &target,
                Kind::Profile,
                &key,
                Op::Patch,
                value,
                &who.by,
                Expect::Rev(r.rev),
            )
            .await?;
        }
        None => {
            let layer = layers::layer_by_key(&mut tx, &target).await?;
            let retired = current_revision(&mut tx, &layer, Kind::Profile, &key)
                .await?
                .is_some_and(|c| c.tombstone);
            if retired {
                // Moved back to a layer it once left: a deliberate re-add of the tombstoned key.
                rows::write_revision(
                    &mut tx,
                    &who.actor,
                    &target,
                    Kind::Profile,
                    &key,
                    Op::Add,
                    value,
                    &who.by,
                )
                .await?;
            } else {
                write_authored(
                    &mut tx,
                    &who.actor,
                    &target,
                    Kind::Profile,
                    &key,
                    Op::Add,
                    value,
                    &who.by,
                    Expect::New,
                )
                .await?;
            }
        }
    }
    audit(
        &mut tx,
        &who,
        PlatformEvent::new("business_model.save", key.as_str())
            .detail("profile", key.as_str())
            .detail("layer", target.as_str())
            .detail("created", created),
    )
    .await?;
    let saved = store::profiles_of(&mut tx, &key)
        .await?
        .into_iter()
        .find(|r| r.layer_key == target)
        .ok_or_else(|| {
            ApiError::Internal(anyhow::anyhow!(
                "profile {key} vanished in its own transaction"
            ))
        })?;
    let out = profile_out(saved)?;
    tx.commit().await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(out)))
}

// --- dimensions -----------------------------------------------------------------------------------------------

async fn platform_dimensions(
    _sa: SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Vec<DimensionOut>>> {
    let mut tx = read_tx(&st).await?;
    Ok(Json(store::dimensions(&mut tx).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DimensionIn {
    key: String,
    label: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    kind: Option<String>,
}

/// `createDimension` (F-27): 422 `invalid_dimension_key`, `invalid_label`, `invalid_kind`; 409 `dimension_exists`
/// (`region` included: it is seeded, and KH reads its values as the region vocabulary — R13). Audit
/// `dimension.create`.
async fn create_dimension(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<DimensionIn>,
) -> ApiResult<(StatusCode, Json<DimensionOut>)> {
    let who = Who::of(&sa);
    let key = b.key.trim();
    if !is_dim_slug(key) {
        return Err(unprocessable(
            "invalid_dimension_key",
            "A dimension key is lower-case with dashes or underscores (service_level).",
        ));
    }
    let label = label_of(&b.label)?;
    let kind = b.kind.as_deref().unwrap_or("dimension");
    if !matches!(kind, "dimension" | "grouping" | "placeholder") {
        return Err(unprocessable(
            "invalid_kind",
            format!("“{kind}” is not a dimension kind (dimension, grouping or placeholder)."),
        ));
    }
    let mut tx = write_tx(&st).await?;
    if !store::insert_dimension(&mut tx, key, &label, b.description.trim(), kind).await? {
        return Err(conflict(
            "dimension_exists",
            format!("A dimension {key} exists already."),
        ));
    }
    audit(
        &mut tx,
        &who,
        PlatformEvent::new("dimension.create", key)
            .detail("dimension", key)
            .detail("kind", kind),
    )
    .await?;
    let out = dimension_or_404(&mut tx, key).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

async fn dimension_or_404(conn: &mut PgConnection, key: &str) -> ApiResult<DimensionOut> {
    store::dimension(conn, key)
        .await?
        .ok_or_else(|| missing("dimension_not_found", "No such dimension."))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DimensionValueIn {
    key: String,
    label: String,
    #[serde(default)]
    note: Option<String>,
}

/// `addDimensionValue`: 404 `dimension_not_found`; 422 `invalid_value_key`, `invalid_label`; 409 `value_exists`.
/// On `region` this widens KH's region vocabulary (R13). Audit `dimension.value_add`.
async fn add_dimension_value(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(dim): ApiPath<String>,
    ApiJson(b): ApiJson<DimensionValueIn>,
) -> ApiResult<(StatusCode, Json<DimensionOut>)> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let d = dimension_or_404(&mut tx, &dim).await?;
    let key = b.key.trim();
    if !is_dim_slug(key) {
        return Err(unprocessable(
            "invalid_value_key",
            "A value key is lower-case with dashes (order-support).",
        ));
    }
    let exists = || {
        conflict(
            "value_exists",
            format!("{} already has “{key}”; values are insert-only.", d.label),
        )
    };
    if d.values.iter().any(|v| v.key == key) {
        return Err(exists());
    }
    let label = label_of(&b.label)?;
    let note = b.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    if !store::insert_dimension_value(&mut tx, &d.key, key, &label, note, &who.by).await? {
        return Err(exists());
    }
    audit(
        &mut tx,
        &who,
        PlatformEvent::new("dimension.value_add", format!("{} · {key}", d.label))
            .detail("dimension", d.key.as_str())
            .detail("value", key),
    )
    .await?;
    let out = dimension_or_404(&mut tx, &dim).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}

// --- department archetypes + dimension usage (KH bucket) -----------------------------------------------------------

/// One industry department that specialises an archetype, with what KH's fold compiles into it (D107/D115: the
/// archetype's kinds and activities fold in right before the department's own; inherited base/industry ones too).
#[derive(Debug, Serialize)]
struct SpecialisedBy {
    industry: String,
    /// The bare department key (`customer_service`).
    key: String,
    /// The department layer key (`industry_dept/<industry>/<department>`), the selection value.
    layer: String,
    label: String,
    layer_id: Option<String>,
    areas: Vec<String>,
    /// The department's effective knowledge kinds (`layer` = the declaring step: a layer key or
    /// `department_archetype/<key>`).
    kinds: Vec<agent_model::compile::FoldedKind>,
    activities: Vec<agent_model::compile::FoldedActivity>,
}

/// `departmentArchetypes`: every current base `department_archetype` row (by key) as the UI `DepartmentArchetype`
/// (its fields + `key`, `layer_id`, `fold_step` = `department_archetype/<key>`) with `specialised_by` = the
/// industry_dept layers (no region sub-layer) whose `extends` names it, by layer key.
async fn department_archetypes(
    _sa: SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Vec<Value>>> {
    let mut tx = read_tx(&st).await?;
    let head = crate::platform::templates::head_model(&mut tx).await?;
    let mut all = layers::layers(&mut tx).await?;
    all.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
    let mut defs: Vec<(String, uuid::Uuid, Value)> = sqlx::query_as(
        "SELECT key, layer_id, fields FROM layer_head \
          WHERE kind = 'department_archetype' AND layer_key = 'base' AND op <> 'tombstone'",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.rollback().await?;
    defs.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));

    let mut out = Vec::with_capacity(defs.len());
    for (key, layer_id, fields) in defs {
        let specialised_by: Vec<SpecialisedBy> = all
            .iter()
            .filter(|l| {
                l.scope == "industry_dept"
                    && l.region.is_none()
                    && l.extends.as_deref() == Some(key.as_str())
            })
            .filter_map(|l| {
                let industry = l.industry.clone()?;
                let department = l.department.clone()?;
                let dept = agent_model::compile::department_key(&industry, &department);
                let sel = agent_model::compile::Selection {
                    industry: industry.clone(),
                    departments: vec![dept.clone()],
                    ..agent_model::compile::Selection::default()
                };
                // the knowledge fold alone (no profile layers: they hold no kinds); its errors are the compile's
                let (knowledge, _) =
                    agent_model::compile::fold_knowledge(&head.layers, &head.rows, &sel, None);
                let kinds = knowledge
                    .kinds_of_department(&dept)
                    .into_iter()
                    .cloned()
                    .collect();
                let activities = knowledge
                    .activities
                    .iter()
                    .filter(|a| a.of_department(&dept))
                    .cloned()
                    .collect();
                Some(SpecialisedBy {
                    label: l.label.clone().unwrap_or_else(|| department.clone()),
                    layer: l.key.clone(),
                    layer_id: Some(l.id.to_string()),
                    areas: l.areas.clone(),
                    kinds,
                    activities,
                    industry,
                    key: department,
                })
            })
            .collect();
        let mut obj = fields.as_object().cloned().unwrap_or_default();
        obj.insert("key".to_owned(), Value::String(key.clone()));
        obj.insert("layer_id".to_owned(), Value::String(layer_id.to_string()));
        obj.insert(
            "fold_step".to_owned(),
            Value::String(agent_model::compile::archetype_layer_id(&key)),
        );
        obj.insert("specialised_by".to_owned(), to_json(&specialised_by)?);
        out.push(Value::Object(obj));
    }
    Ok(Json(out))
}

/// A dimension key → the values a selection names for it.
type DimValues = BTreeMap<String, Vec<String>>;

/// A current `layer_head` row: (layer_id, id, kind, key, op).
type HeadRow = (uuid::Uuid, uuid::Uuid, String, String, String);

/// A release whose frozen selection read: the index of its job in `tenant_jobs`, the release, the values it names.
type FrozenRelease<'a> = (usize, &'a super::compile::TenantRelease, DimValues);

/// Where a job selection stores a platform dimension (D65/D111, the fixture's `SELECTION_KEY`).
const SELECTION_KEYS: [(&str, &str); 3] = [
    ("area", "areas"),
    ("channel", "dimensions.channel"),
    ("region", "region"),
];

/// `^[a-z][a-z0-9]*(<sep>[a-z0-9]+)*$`: one part of a context-space key (web `context-space-key.ts`).
fn is_key_part(s: &str, sep: char) -> bool {
    s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && s.split(sep).all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// The area of a context-space key `<industry>:<area>:<slug>` (D61); `None` when the key breaks the scheme.
fn area_of_context_space(key: &str) -> Option<&str> {
    let mut parts = key.split(':');
    let (industry, area, slug) = (parts.next()?, parts.next()?, parts.next()?);
    (parts.next().is_none()
        && is_key_part(industry, '_')
        && is_key_part(area, '-')
        && is_key_part(slug, '_'))
    .then_some(area)
}

/// `platformDimensionUsage` (UI `DimensionUsage`): the port of the fixture `dimensionUsage()` over the live reads —
/// job × dimension layers and their current rows, current (not tombstoned) context spaces per area, and every tenant
/// job's working selection (`job_target`) and releases (any state), read through `compile::tenant_jobs` (R3). An
/// unreadable stored selection is skipped (logged), never a 500 of the page.
async fn platform_dimension_usage(
    _sa: SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let dims = store::dimensions(&mut tx).await?;
    let mut all = layers::layers(&mut tx).await?;
    all.sort_by(|a, b| a.key.as_str().cmp(b.key.as_str()));
    let head: Vec<HeadRow> = sqlx::query_as("SELECT layer_id, id, kind, key, op FROM layer_head")
        .fetch_all(&mut *tx)
        .await?;
    let jobs = super::compile::tenant_jobs(&mut tx).await?;
    tx.rollback().await?;

    let selection_of = |what: &str, v: &Value| match crate::design::read::selection_of_value(v) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!(what = what, error = %e, "dimension_usage_selection_unreadable");
            None
        }
    };
    let values_of = |s: &agent_model::compile::Selection| -> DimValues {
        dims.iter()
            .map(|d| (d.key.clone(), s.values(&d.key).to_vec()))
            .collect()
    };

    // job selections today
    let mut selections: Vec<Value> = Vec::new();
    let mut selection_values: Vec<(usize, DimValues)> = Vec::new();
    let mut unknown: Vec<Value> = Vec::new();
    for (n, j) in jobs.iter().enumerate() {
        let Some(raw) = &j.selection else { continue };
        let Some(sel) = selection_of("job_target", raw) else {
            continue;
        };
        let values = values_of(&sel);
        for d in &dims {
            for v in values.get(&d.key).into_iter().flatten() {
                if !d.values.iter().any(|x| x.key == *v) {
                    unknown.push(json!({
                        "dimension": d.key, "value": v, "tenant_name": j.tenant_name, "job": j.key,
                    }));
                }
            }
        }
        selections.push(json!({
            "tenant_id": j.tenant_id.to_string(),
            "tenant_name": j.tenant_name,
            "job": j.key,
            "job_label": j.label,
            "selection": to_json(&sel)?,
            "values": values,
            "updated_at": j.selection_updated_at,
        }));
        selection_values.push((n, values));
    }
    // releases (frozen selections, any state)
    let mut releases: Vec<FrozenRelease<'_>> = Vec::new();
    for (n, j) in jobs.iter().enumerate() {
        for r in &j.releases {
            if let Some(sel) = selection_of("job_release", &r.selection) {
                releases.push((n, r, values_of(&sel)));
            }
        }
    }

    let names = |values: &DimValues, dim: &str, value: &str| {
        values
            .get(dim)
            .is_some_and(|vs| vs.iter().any(|v| v == value))
    };
    let mut out_values: Vec<Value> = Vec::new();
    for d in &dims {
        for v in &d.values {
            let qualified: Vec<Value> = all
                .iter()
                .filter(|l| {
                    l.dimension_key.as_deref() == Some(d.key.as_str())
                        && l.dimension_value.as_deref() == Some(v.key.as_str())
                })
                .map(|l| {
                    let mut rows: Vec<&HeadRow> = head.iter().filter(|r| r.0 == l.id).collect();
                    rows.sort_by(|a, b| {
                        (a.2.as_str(), a.3.as_str()).cmp(&(b.2.as_str(), b.3.as_str()))
                    });
                    json!({
                        "id": l.id.to_string(),
                        "path": l.key,
                        "job": l.job,
                        "row_id": rows.first().map(|r| r.1.to_string()),
                        "adds": rows.iter().map(|r| r.3.clone()).collect::<Vec<_>>(),
                    })
                })
                .collect();
            let context_spaces: Vec<String> = if d.key == "area" {
                head.iter()
                    .filter(|r| {
                        r.2 == "context_space"
                            && r.4 != "tombstone"
                            && area_of_context_space(&r.3) == Some(v.key.as_str())
                    })
                    .map(|r| r.3.clone())
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            };
            let named_by: Vec<Value> = selection_values
                .iter()
                .filter(|(_, values)| names(values, d.key.as_str(), v.key.as_str()))
                .map(|(n, _)| {
                    let j = &jobs[*n];
                    json!({
                        "tenant_id": j.tenant_id.to_string(),
                        "tenant_name": j.tenant_name,
                        "job": j.key,
                        "job_label": j.label,
                    })
                })
                .collect();
            let frozen: Vec<Value> = releases
                .iter()
                .filter(|(_, _, values)| names(values, d.key.as_str(), v.key.as_str()))
                .map(|(n, r, _)| {
                    let j = &jobs[*n];
                    json!({
                        "id": r.release_id,
                        "tenant_id": j.tenant_id.to_string(),
                        "tenant_name": j.tenant_name,
                        "job": j.key,
                        "state": r.state,
                        "at": r.released_at.unwrap_or(r.approved_at),
                    })
                })
                .collect();
            out_values.push(json!({
                "dimension": d.key,
                "value": v.key,
                "layers": qualified,
                "context_spaces": context_spaces,
                "selections": named_by,
                "releases": frozen,
            }));
        }
    }
    let selection_keys: BTreeMap<&str, &str> = SELECTION_KEYS.into_iter().collect();
    Ok(Json(json!({
        "selection_keys": selection_keys,
        "values": out_values,
        "selections": selections,
        "unknown": unknown,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_space_area() {
        assert_eq!(
            area_of_context_space("retail:order-support:track_parcel"),
            Some("order-support")
        );
        assert_eq!(area_of_context_space("retail:faq:shipping"), Some("faq"));
        for bad in [
            "retail:faq",
            "retail:faq:x:y",
            "Retail:faq:x",
            "retail:Faq:x",
            "retail:faq-:x",
            "retail::x",
        ] {
            assert_eq!(area_of_context_space(bad), None, "{bad}");
        }
    }

    #[test]
    fn grammars() {
        assert!(is_leaf("debit") && is_leaf("a") && !is_leaf("Debit") && !is_leaf("a.b"));
        assert!(!is_leaf(&"a".repeat(42)));
        assert!(is_axis_key("order_type") && is_axis_key("offering.availability"));
        assert!(!is_axis_key("a.b.c") && !is_axis_key("a.") && !is_axis_key("A"));
        assert!(is_slug("bundles") && !is_slug("b") && !is_slug("2x") && !is_slug("a-b"));
        assert!(is_dim_slug("order-support") && is_dim_slug("web_chat") && !is_dim_slug("Eu"));
        assert!(
            is_entity("MeterPoint")
                && !is_entity("meterPoint")
                && !is_entity("M")
                && !is_entity("Meter_Point")
        );
    }

    #[test]
    fn leaves_of_conditions() {
        let c: Condition = serde_json::from_value(json!({
            "and": [{"eq": ["order_type", "continuous"]},
                    {"not": {"in": ["payment_type", ["balancing", "prepaid"]]}}]
        }))
        .unwrap();
        let mut out = Vec::new();
        condition_leaves(&c, &mut out);
        assert_eq!(
            out,
            vec![
                ("order_type".into(), "continuous".into()),
                ("payment_type".into(), "balancing".into()),
                ("payment_type".into(), "prepaid".into()),
            ]
        );
    }

    #[test]
    fn refusals_keep_status() {
        assert_eq!(
            refusal(Refusal::new(422, "unknown_axis", "x")).code(),
            "unknown_axis"
        );
        assert!(matches!(
            refusal(Refusal::new(404, "industry_not_found", "x")),
            ApiError::Missing(..)
        ));
    }
}
