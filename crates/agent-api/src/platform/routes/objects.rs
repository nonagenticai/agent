//! B6b platform-model area `objects` (plan B6 §5): T11 object model + operations + error states; T12c saveOperation;
//! T12f relations + field retirement. Templates and export are KH bucket.
//! Every handler takes `SuperAdmin`; helpers in [`super`].
//!
//! Reads run in one [`read_tx`] snapshot over the head catalogue ([`catalogue`]: one row per key, the broadest
//! layer's). Writes read the catalogue row inside [`write_tx`], apply one pure edit ([`edit`]) and write the whole
//! item as that row's next revision in its own layer with `write_authored` (`Expect::Rev` of the row read: a
//! concurrent edit is 409 `stale_revision`). `write_revision` records the one `layer.revision` platform audit row.
//! The edit reaches a job only through a tag + cut (B6-6): the head shows it as pending.
pub mod catalogue;
mod context;
pub mod edit;

use std::collections::{BTreeMap, BTreeSet};

use agent_model::catalog::verb_states;
use agent_model::kinds::catalog::{
    Discriminator, ErrorStateFields, Modality, ObjectFields, OpKind,
};
use agent_model::kinds::{Kind, Op};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::PgConnection;

use self::catalogue::{Catalogue, HeadItem};
use self::edit::{
    FieldInput, OperationContext, OperationEdit, RelationInput, StateInput, TransitionInput,
};
use super::{Expect, Who, check_snake_key, conflict, missing, read_tx, unprocessable, write_tx};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::page::paged;
use crate::platform::db::authored::write_authored;
use crate::platform::db::corrupt;
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/platform/objects",
            get(object_catalog).post(create_object),
        )
        .route("/api/platform/objects/map", get(object_model_map))
        .route("/api/platform/objects/{object}", get(object_detail))
        .route(
            "/api/platform/objects/{object}/context",
            get(object_context),
        )
        .route(
            "/api/platform/objects/{object}/fields/{field}",
            put(save_object_field),
        )
        .route(
            "/api/platform/objects/{object}/fields/{field}/retire",
            post(retire_object_field),
        )
        .route(
            "/api/platform/objects/{object}/states/{status}",
            put(save_object_state),
        )
        .route(
            "/api/platform/objects/{object}/transitions/{verb}/{from}",
            put(save_object_transition).delete(delete_object_transition),
        )
        .route(
            "/api/platform/objects/{object}/relations/{predicate}/{target}",
            put(save_object_relation).delete(delete_object_relation),
        )
        .route(
            "/api/platform/operations",
            get(catalog_operations).post(create_operation),
        )
        .route(
            "/api/platform/operations/{object}/{verb}",
            put(save_operation),
        )
        .route("/api/platform/error-states", get(error_state_defs))
}

fn to_json<T: Serialize>(t: &T) -> ApiResult<Value> {
    serde_json::to_value(t)
        .map_err(|e| ApiError::Internal(anyhow::anyhow!("object model to JSON: {e}")))
}

fn object_not_found(object: &str) -> ApiError {
    missing(
        "object_not_found",
        format!("No object “{object}” in the catalogue."),
    )
}

/// The ops of the catalogue as the routes answer them: the resolved row plus the layer of its stored row and
/// `pending` (an untagged revision of that row, T12c).
fn op_json(
    cat: &Catalogue,
    r: &agent_model::catalog::OperationRow,
    untagged: &BTreeSet<(String, String)>,
) -> ApiResult<Value> {
    let mut v = to_json(r)?;
    if let Some((item, _)) = cat.op_row(&r.key) {
        v["layer_key"] = json!(item.layer_key);
        v["rev"] = json!(item.rev);
        v["pending"] = json!(untagged.contains(&(item.layer_key.clone(), item.key.clone())));
    }
    Ok(v)
}

fn str_attr<'a>(r: &'a agent_model::catalog::OperationRow, column: &str) -> Option<&'a str> {
    r.attrs.get(column).and_then(Value::as_str)
}

// --- reads -----------------------------------------------------------------------------------------------------

/// `GET /api/platform/objects` — every catalogue object with its counts, plus the folded concepts (P9).
async fn object_catalog(_sa: SuperAdmin, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let cat = catalogue::load(&mut tx).await?;
    let objects: Vec<Value> = cat
        .objects
        .iter()
        .map(|(_, o)| {
            let d = &o.def;
            json!({
                "object": o.object,
                "label": d.label,
                "tier": d.tier,
                "storage_shape_of": d.storage_shape_of,
                "origin": d.origin,
                "description": d.description,
                "in_retail_job": d.in_retail_job,
                "field_count": d.fields.len(),
                "pii_count": d.fields.iter().filter(|f| f.pii).count(),
                "state_count": d.states.len(),
                "transition_count": d.transitions.len(),
                "operation_count": cat.ops_of(&o.object).len(),
            })
        })
        .collect();
    let folded: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT concept, becomes, object, cite FROM folded_concept ORDER BY concept",
    )
    .fetch_all(&mut *tx)
    .await?;
    let folded: Vec<Value> = folded
        .into_iter()
        .map(|(concept, becomes, object, cite)| {
            json!({"concept": concept, "becomes": becomes, "object": object, "cite": cite})
        })
        .collect();
    Ok(Json(json!({"objects": objects, "folded": folded})))
}

/// `GET /api/platform/objects/map` — P9: per object its states, operations and carrying layers; every relation.
async fn object_model_map(_sa: SuperAdmin, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let items = catalogue::head_items(&mut tx, &[Kind::Object, Kind::Operation]).await?;
    let cat = Catalogue::from_items(&items)?;
    let first = catalogue::first_tags(&mut tx, Kind::Object).await?;
    let latest = crate::platform::db::tags::latest_tags(&mut tx).await?;
    let mut carriers: Vec<&HeadItem> = items
        .iter()
        .filter(|i| i.kind == Kind::Object.as_str() && i.op != "tombstone" && i.scope != "profile")
        .collect();
    carriers.sort_by(|a, b| {
        (catalogue::scope_rank(&a.scope), &a.layer_key)
            .cmp(&(catalogue::scope_rank(&b.scope), &b.layer_key))
    });
    let nodes: Vec<Value> = cat
        .objects
        .iter()
        .map(|(_, o)| {
            let d = &o.def;
            let states: Vec<Value> = d
                .states
                .iter()
                .map(|s| json!({"status": s.status, "initial": s.initial, "terminal": s.terminal}))
                .collect();
            let layers: Vec<Value> = carriers
                .iter()
                .filter(|i| i.key == o.object)
                .map(|i| {
                    let since = first
                        .get(&(i.layer_key.clone(), i.key.clone()))
                        .copied()
                        .unwrap_or_else(|| latest.get(&i.family).copied().unwrap_or(0) + 1);
                    json!({
                        "layer_key": i.layer_key,
                        "scope": i.scope,
                        "label": i.layer_label.clone().unwrap_or_else(|| i.layer_key.clone()),
                        "since": since,
                    })
                })
                .collect();
            let operations: Vec<Value> = cat
                .ops_of(&o.object)
                .into_iter()
                .map(|r| {
                    json!({
                        "key": r.key,
                        "modality": str_attr(r, "modality"),
                        "side_effect": str_attr(r, "side_effect"),
                    })
                })
                .collect();
            json!({
                "object": o.object,
                "label": d.label,
                "tier": d.tier,
                "origin": d.origin,
                "description": d.description,
                "in_retail_job": d.in_retail_job,
                "storage_shape_of": d.storage_shape_of,
                "states": states,
                "transition_count": d.transitions.len(),
                "operations": operations,
                "layers": layers,
                "pii_count": d.fields.iter().filter(|f| f.pii).count(),
                "field_count": d.fields.len(),
            })
        })
        .collect();
    let mut relations: Vec<Value> = Vec::new();
    for (_, o) in &cat.objects {
        for r in &o.def.relations {
            relations.push(to_json(r)?);
        }
    }
    Ok(Json(json!({"nodes": nodes, "relations": relations})))
}

#[derive(Debug, Deserialize)]
struct ObjectPath {
    object: String,
}

#[derive(Debug, sqlx::FromRow)]
struct ObjectRevision {
    layer_key: String,
    rev: i32,
    op: String,
    fields: serde_json::Value,
    author: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// P10 `pending_edits`: every revision of the object's rows newer than the newest revision any non-provisional tag
/// holds of that (layer, key), newest first, each described against the revision before it.
async fn pending_edits(conn: &mut PgConnection, object: &str) -> ApiResult<Vec<Value>> {
    let revs: Vec<ObjectRevision> = sqlx::query_as(
        "SELECT l.key AS layer_key, r.rev, r.op, r.fields, r.author, r.created_at \
           FROM layer_row r JOIN layer l ON l.id = r.layer_id \
          WHERE r.kind = 'object' AND r.key = $1 \
          ORDER BY l.key, r.rev",
    )
    .bind(object)
    .fetch_all(&mut *conn)
    .await?;
    let tagged: BTreeMap<String, i32> = sqlx::query_as::<_, (String, i32)>(
        "SELECT l.key, max(r.rev) \
           FROM layer_tag_row m \
           JOIN layer_tag t ON t.family = m.family AND t.version = m.version \
           JOIN layer_row r ON r.id = m.row_id \
           JOIN layer l ON l.id = r.layer_id \
          WHERE r.kind = 'object' AND r.key = $1 AND NOT t.provisional \
          GROUP BY l.key",
    )
    .bind(object)
    .fetch_all(&mut *conn)
    .await?
    .into_iter()
    .collect();
    let parse = |r: &ObjectRevision| -> Option<ObjectFields> {
        if r.op == "tombstone" {
            None
        } else {
            serde_json::from_value(r.fields.clone()).ok()
        }
    };
    let mut pending: Vec<(&ObjectRevision, String)> = Vec::new();
    for (i, r) in revs.iter().enumerate() {
        if r.rev <= tagged.get(&r.layer_key).copied().unwrap_or(0) {
            continue;
        }
        let prev = i
            .checked_sub(1)
            .map(|j| &revs[j])
            .filter(|p| p.layer_key == r.layer_key);
        let what = edit::describe_change(prev.and_then(parse).as_ref(), parse(r).as_ref());
        pending.push((r, what));
    }
    pending.sort_by(|a, b| {
        b.0.created_at
            .cmp(&a.0.created_at)
            .then(b.0.rev.cmp(&a.0.rev))
    });
    let mut subs: Vec<String> = pending.iter().map(|(r, _)| r.author.clone()).collect();
    subs.sort();
    subs.dedup();
    let names = catalogue::names(&mut *conn, &subs).await?;
    Ok(pending
        .into_iter()
        .map(|(r, what)| {
            json!({
                "object": object,
                "what": what,
                "by": r.author,
                "by_name": names.get(&r.author).cloned().unwrap_or_else(|| r.author.clone()),
                "at": r.created_at,
                "layer_key": r.layer_key,
                "rev": r.rev,
            })
        })
        .collect())
}

/// `GET /api/platform/objects/{object}` — fields, relations, state machine, resolved operations, derived verbs,
/// pending (untagged) edits. 404 `object_not_found`.
async fn object_detail(
    _sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<ObjectPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let cat = catalogue::load(&mut tx).await?;
    let (item, def) = cat
        .object(&p.object)
        .ok_or_else(|| object_not_found(&p.object))?;
    let untagged = catalogue::untagged(&mut tx, Kind::Operation).await?;
    let mut operations = Vec::new();
    for r in cat.ops_of(&p.object) {
        operations.push(op_json(&cat, r, &untagged)?);
    }
    let mut v = to_json(def)?;
    v["operations"] = Value::Array(operations);
    v["verbs"] = to_json(&verb_states(&def.def))?;
    v["pending_edits"] = Value::Array(pending_edits(&mut tx, &p.object).await?);
    v["layer_key"] = json!(item.layer_key);
    v["rev"] = json!(item.rev);
    Ok(Json(v))
}

/// `GET /api/platform/objects/{object}/context` — layers that define or reference the object, the connector
/// templates that hold it (B7, R9), journey activities (B8 hook). 404 `object_not_found`.
async fn object_context(
    _sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<ObjectPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let cat = catalogue::load(&mut tx).await?;
    if cat.object(&p.object).is_none() {
        return Err(object_not_found(&p.object));
    }
    let layers = context::layer_touches(&mut tx, &p.object).await?;
    let activities = st.object_context.activities(&mut tx, &p.object).await?;
    // last: the tenant reads of the instances leave the transaction as agent_app
    let systems = context::systems(&st, &mut tx, &p.object).await?;
    Ok(Json(json!({
        "object": p.object,
        "layers": layers,
        "systems": systems,
        "activities": activities,
    })))
}

#[derive(Debug, Default, Deserialize)]
struct OperationFilter {
    object: Option<String>,
    modality: Option<String>,
    side_effect: Option<String>,
    identity_level: Option<String>,
    builtin: Option<bool>,
    retail_only: Option<bool>,
    q: Option<String>,
}

fn set(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// `GET /api/platform/operations` — the resolved head operations (variants merged with `*`), filtered as the
/// fixture filters; one page (the catalogue is short).
async fn catalog_operations(
    _sa: SuperAdmin,
    State(st): State<AppState>,
    ApiQuery(f): ApiQuery<OperationFilter>,
) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let cat = catalogue::load(&mut tx).await?;
    let untagged = catalogue::untagged(&mut tx, Kind::Operation).await?;
    let q = set(&f.q).map(str::to_lowercase);
    let mut items = Vec::new();
    for r in &cat.resolved {
        let keep = set(&f.object).is_none_or(|o| r.object == o)
            && set(&f.modality).is_none_or(|m| str_attr(r, "modality") == Some(m))
            && set(&f.side_effect).is_none_or(|s| str_attr(r, "side_effect") == Some(s))
            && set(&f.identity_level).is_none_or(|i| str_attr(r, "identity_level") == Some(i))
            && f.builtin
                .is_none_or(|b| r.attrs.get("builtin").and_then(Value::as_bool) == Some(b))
            && (f.retail_only != Some(true) || r.retail.is_some())
            && q.as_deref().is_none_or(|q| {
                r.key.to_lowercase().contains(q) || r.description.to_lowercase().contains(q)
            });
        if keep {
            items.push(op_json(&cat, r, &untagged)?);
        }
    }
    Ok(Json(paged(items, None)))
}

/// `GET /api/platform/error-states` — the head `error_state` rows (the taxonomy, D57).
async fn error_state_defs(_sa: SuperAdmin, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let items = catalogue::head_items(&mut tx, &[Kind::ErrorState]).await?;
    let mut out = Vec::new();
    for it in catalogue::catalogue_rows(&items, Kind::ErrorState) {
        let e: ErrorStateFields = serde_json::from_value(it.fields.clone())
            .map_err(|e| corrupt(format!("error_state {}: {e}", it.key)))?;
        out.push(json!({
            "code": it.key,
            "user_facing": e.user_facing,
            "retryable": e.retryable,
            "escalate": e.escalate,
            "description": e.description,
        }));
    }
    Ok(Json(Value::Array(out)))
}

// --- writes ----------------------------------------------------------------------------------------------------

/// Read the catalogue row of `object` in `tx`, apply `f`, write the whole item as the row's next revision (same op,
/// same layer, `Expect::Rev`). 404 `object_not_found`; then the edit's refusals; then `write_authored`'s.
async fn edit_object<T>(
    st: &AppState,
    who: &Who,
    object: &str,
    f: impl FnOnce(&mut ObjectFields, &Catalogue) -> ApiResult<T>,
) -> ApiResult<T> {
    let mut tx = write_tx(st).await?;
    let cat = catalogue::load(&mut tx).await?;
    let (item, def) = cat.object(object).ok_or_else(|| object_not_found(object))?;
    let mut fields = def.def.clone();
    let out = f(&mut fields, &cat)?;
    let op = if item.op == Op::Patch.as_str() {
        Op::Patch
    } else {
        Op::Add
    };
    write_authored(
        &mut tx,
        &who.actor,
        &item.layer_key,
        Kind::Object,
        object,
        op,
        to_json(&fields)?,
        &who.by,
        Expect::Rev(item.rev),
    )
    .await?;
    tx.commit().await?;
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct FieldPath {
    object: String,
    field: String,
}

/// `PUT /api/platform/objects/{object}/fields/{field}` — add or change a field. 404 `object_not_found`; 422
/// `invalid_field`, `enum_values_required`.
async fn save_object_field(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<FieldPath>,
    ApiJson(body): ApiJson<FieldInput>,
) -> ApiResult<Json<Value>> {
    let who = Who::of(&sa);
    let f = edit_object(&st, &who, &p.object, |o, _| {
        edit::save_field(o, &p.field, body)
    })
    .await?;
    Ok(Json(to_json(&f)?))
}

/// `POST /api/platform/objects/{object}/fields/{field}/retire` (F-29). 404 `object_not_found`, `field_not_found`;
/// 409 `field_retired`, `field_in_use{operations}`.
async fn retire_object_field(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<FieldPath>,
) -> ApiResult<Json<Value>> {
    let who = Who::of(&sa);
    let f = edit_object(&st, &who, &p.object, |o, cat| {
        edit::retire_field(o, &p.object, &p.field, &cat.ops_of(&p.object))
    })
    .await?;
    Ok(Json(to_json(&f)?))
}

#[derive(Debug, Deserialize)]
struct StatePath {
    object: String,
    status: String,
}

/// `PUT /api/platform/objects/{object}/states/{status}`. 404; 422 `invalid_state_name`.
async fn save_object_state(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<StatePath>,
    ApiJson(body): ApiJson<StateInput>,
) -> ApiResult<Json<Value>> {
    let who = Who::of(&sa);
    let s = edit_object(&st, &who, &p.object, |o, _| {
        edit::save_state(o, &p.status, body)
    })
    .await?;
    Ok(Json(to_json(&s)?))
}

#[derive(Debug, Deserialize)]
struct TransitionPath {
    object: String,
    verb: String,
    from: String,
}

/// `PUT /api/platform/objects/{object}/transitions/{verb}/{from}`. 404; 422 `invalid_verb`, `unknown_state`.
async fn save_object_transition(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TransitionPath>,
    ApiJson(body): ApiJson<TransitionInput>,
) -> ApiResult<Json<Value>> {
    let who = Who::of(&sa);
    let t = edit_object(&st, &who, &p.object, |o, _| {
        edit::save_transition(o, &p.object, &p.verb, &p.from, body)
    })
    .await?;
    Ok(Json(to_json(&t)?))
}

/// `DELETE /api/platform/objects/{object}/transitions/{verb}/{from}` → 204. 404 `object_not_found`,
/// `transition_not_found`.
async fn delete_object_transition(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<TransitionPath>,
) -> ApiResult<StatusCode> {
    let who = Who::of(&sa);
    edit_object(&st, &who, &p.object, |o, _| {
        edit::delete_transition(o, &p.verb, &p.from)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct RelationPath {
    object: String,
    predicate: String,
    target: String,
}

/// `PUT /api/platform/objects/{object}/relations/{predicate}/{target}` body `{cardinality}` (F-28). 404
/// `object_not_found`, `target_not_found`; 422 `invalid_predicate`, `invalid_cardinality`.
async fn save_object_relation(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<RelationPath>,
    ApiJson(body): ApiJson<RelationInput>,
) -> ApiResult<Json<Value>> {
    let who = Who::of(&sa);
    let r = edit_object(&st, &who, &p.object, |o, cat| {
        edit::check_predicate(&p.predicate)?;
        if cat.object(&p.target).is_none() {
            return Err(missing(
                "target_not_found",
                format!("No object “{}” in the catalogue.", p.target),
            ));
        }
        edit::save_relation(o, &p.object, &p.predicate, &p.target, body)
    })
    .await?;
    Ok(Json(to_json(&r)?))
}

/// `DELETE /api/platform/objects/{object}/relations/{predicate}/{target}` → 204. 404 `object_not_found`,
/// `relation_not_found`.
async fn delete_object_relation(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<RelationPath>,
) -> ApiResult<StatusCode> {
    let who = Who::of(&sa);
    edit_object(&st, &who, &p.object, |o, _| {
        edit::delete_relation(o, &p.object, &p.predicate, &p.target)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
struct OperationPath {
    object: String,
    verb: String,
}

#[derive(Debug, Default, Deserialize)]
struct VariantQuery {
    variant: Option<String>,
}

/// `PUT /api/platform/operations/{object}/{verb}?variant=` body `OperationEdit` (T12c, F-22): the next revision of
/// the operation's own row, in its own layer. 404 `operation_not_found`; 409 `builtin_operation`; 422
/// `invalid_operation`, `unknown_field`, `unknown_verify_read`, `invalid_identity_level`. Answers the resolved
/// head row (`pending: true` until the family is tagged).
async fn save_operation(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<OperationPath>,
    ApiQuery(vq): ApiQuery<VariantQuery>,
    ApiJson(body): ApiJson<OperationEdit>,
) -> ApiResult<Json<Value>> {
    let who = Who::of(&sa);
    let variant = set(&vq.variant)
        .unwrap_or(agent_model::catalog::STAR)
        .to_owned();
    let key = agent_model::catalog::op_key(&p.object, &p.verb, &variant);
    let not_found = || {
        missing(
            "operation_not_found",
            format!("No operation {key} in the catalogue."),
        )
    };
    let mut tx = write_tx(&st).await?;
    let cat = catalogue::load(&mut tx).await?;
    let (item, row) = cat.op_row(&key).ok_or_else(not_found)?;
    let current = cat.resolved_op(&key).ok_or_else(not_found)?;
    let builtin = current.attrs.get("builtin").and_then(Value::as_bool) == Some(true)
        || str_attr(current, "op_kind") == Some("builtin");
    if builtin {
        return Err(ApiError::Conflict(
            "builtin_operation",
            format!("{key} is a built-in operation; it runs in the agent and is not edited here."),
        ));
    }
    let no_fields = Vec::new();
    let fields = cat
        .object(&p.object)
        .map(|(_, d)| &d.def.fields)
        .unwrap_or(&no_fields);
    let read_ops: BTreeSet<String> = cat
        .resolved
        .iter()
        .filter(|r| str_attr(r, "side_effect") == Some("R"))
        .map(|r| r.key.clone())
        .collect();
    let star_key = agent_model::catalog::op_key(&p.object, &p.verb, agent_model::catalog::STAR);
    let star = if variant == agent_model::catalog::STAR {
        None
    } else {
        cat.op_row(&star_key)
            .map(|(_, s)| (&s.raw_attrs, s.extra_errors.as_slice()))
    };
    let cx = OperationContext {
        row_fields: &item.fields,
        star,
        verb: &row.verb,
        current,
        fields,
        read_ops: &read_ops,
    };
    let new_fields = edit::apply_operation_edit(&cx, body)?;
    let op = if item.op == Op::Patch.as_str() {
        Op::Patch
    } else {
        Op::Add
    };
    write_authored(
        &mut tx,
        &who.actor,
        &item.layer_key,
        Kind::Operation,
        &key,
        op,
        new_fields,
        &who.by,
        Expect::Rev(item.rev),
    )
    .await?;
    let after = catalogue::load(&mut tx).await?;
    let untagged = catalogue::untagged(&mut tx, Kind::Operation).await?;
    let saved = after
        .resolved_op(&key)
        .ok_or_else(|| corrupt(format!("operation {key} vanished on save")))?;
    let out = op_json(&after, saved, &untagged)?;
    tx.commit().await?;
    Ok(Json(out))
}

// --- creates (E2) ----------------------------------------------------------------------------------------------

/// The layer a create writes to when the body names none: the base layer.
const DEFAULT_CREATE_LAYER: &str = "base";

/// `POST /api/platform/objects` body: the key `object`, the layer the row is added to (`layer_key`, default
/// `base`) and the `object` row fields (`ObjectFields`: `label`, `tier` required).
#[derive(Debug, Deserialize)]
struct CreateObject {
    object: String,
    #[serde(default)]
    layer_key: Option<String>,
    #[serde(flatten)]
    def: ObjectFields,
}

/// `POST /api/platform/objects` (E2): add an object as an `add` row of `layer_key` (first revision). 201 with the
/// object as `GET` answers it (without operations). 404 `layer_not_found`; 409 `object_exists` (the catalogue has
/// the key), `duplicate_add`, `add_tombstoned`; 422 `invalid_object_key`, `invalid_row`, `invalid_key`,
/// `kind_not_allowed_in_layer`.
async fn create_object(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<CreateObject>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let who = Who::of(&sa);
    check_snake_key("invalid_object_key", &b.object)?;
    let layer = set(&b.layer_key).unwrap_or(DEFAULT_CREATE_LAYER).to_owned();
    let mut tx = write_tx(&st).await?;
    let cat = catalogue::load(&mut tx).await?;
    if let Some((item, _)) = cat.object(&b.object) {
        return Err(conflict(
            "object_exists",
            format!("The object “{}” already exists; edit it instead.", b.object),
        )
        .with("layer_key", item.layer_key.clone()));
    }
    let row = write_authored(
        &mut tx,
        &who.actor,
        &layer,
        Kind::Object,
        &b.object,
        Op::Add,
        to_json(&b.def)?,
        &who.by,
        Expect::New,
    )
    .await?;
    tx.commit().await?;
    let mut v = row.fields;
    v["object"] = json!(b.object);
    v["layer_key"] = json!(layer);
    v["rev"] = json!(row.rev);
    Ok((StatusCode::CREATED, Json(v)))
}

/// `POST /api/platform/operations` body: the key parts (`object`, bare `verb`, `variant` default `*`), the layer
/// (`layer_key`, default `base`), the columns `OperationEdit` takes, and the ones only a create sets: `modality`
/// and `op_kind` (required on a `*` row; `builtin` is never set here), `is_derive`, `guard_rules`,
/// `discriminator` (variant). A variant row keeps only what it overrides.
#[derive(Debug, Deserialize)]
struct CreateOperation {
    object: String,
    verb: String,
    #[serde(default)]
    variant: Option<String>,
    #[serde(default)]
    layer_key: Option<String>,
    #[serde(default)]
    modality: Option<Modality>,
    #[serde(default)]
    op_kind: Option<OpKind>,
    #[serde(default)]
    is_derive: Option<bool>,
    #[serde(default)]
    guard_rules: Option<Vec<String>>,
    #[serde(default)]
    discriminator: Option<Discriminator>,
    #[serde(flatten)]
    edit: OperationEdit,
}

/// `POST /api/platform/operations` (E2): add an operation as an `add` row of `layer_key`. 201 with the resolved head
/// row, as `PUT` answers (`pending: true` until the family is tagged). 404 `layer_not_found`; 409 `operation_exists`,
/// `duplicate_add`, `add_tombstoned`; 422 `invalid_operation_key`, `unknown_object`, `star_operation_required`
/// (a variant needs its `*` row), `invalid_operation` (incl. a built-in or a `*` row without `modality` / `op_kind`),
/// `identity_level_required` (none sent; no default), `identity_level_below_contract` (a contract operation below
/// its contract level; `minimum`), `invalid_identity_level`, `unknown_field`,
/// `unknown_verify_read`, `invalid_row`.
async fn create_operation(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<CreateOperation>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let who = Who::of(&sa);
    let variant = set(&b.variant)
        .unwrap_or(agent_model::catalog::STAR)
        .to_owned();
    let is_star = variant == agent_model::catalog::STAR;
    let key = agent_model::catalog::op_key(&b.object, &b.verb, &variant);
    if agent_model::kinds::split_operation_key(&key).is_none() {
        return Err(unprocessable(
            "invalid_operation_key",
            format!(
                "“{key}” is not an operation key: object and verb are snake_case, a variant is letters, digits, _ or -."
            ),
        ));
    }
    if b.op_kind == Some(OpKind::Builtin) {
        return Err(unprocessable(
            "invalid_operation",
            "A built-in operation runs in the agent and is not created here.",
        ));
    }
    if is_star && (b.modality.is_none() || b.op_kind.is_none()) {
        return Err(unprocessable(
            "invalid_operation",
            "A * operation needs modality and op_kind.",
        ));
    }
    if is_star && b.discriminator.is_some() {
        return Err(unprocessable(
            "invalid_operation",
            "Only a variant has a discriminator.",
        ));
    }
    let layer = set(&b.layer_key).unwrap_or(DEFAULT_CREATE_LAYER).to_owned();
    let mut tx = write_tx(&st).await?;
    let cat = catalogue::load(&mut tx).await?;
    if cat.op_row(&key).is_some() {
        return Err(conflict(
            "operation_exists",
            format!("The operation {key} already exists; edit it instead."),
        ));
    }
    let (_, def) = cat.object(&b.object).ok_or_else(|| {
        unprocessable(
            "unknown_object",
            format!("No object “{}” in the catalogue.", b.object),
        )
    })?;
    let star_key = agent_model::catalog::op_key(&b.object, &b.verb, agent_model::catalog::STAR);
    let star = if is_star {
        None
    } else {
        let (_, s) = cat.op_row(&star_key).ok_or_else(|| {
            unprocessable(
                "star_operation_required",
                format!("{star_key} must exist before a variant of it is created."),
            )
        })?;
        Some((&s.raw_attrs, s.extra_errors.as_slice()))
    };
    let mut attrs = serde_json::Map::new();
    if is_star {
        attrs.insert("is_derive".into(), json!(b.is_derive.unwrap_or(false)));
        attrs.insert("builtin".into(), json!(false));
        attrs.insert(
            "guard_rules".into(),
            json!(b.guard_rules.clone().unwrap_or_default()),
        );
    } else {
        if let Some(d) = b.is_derive {
            attrs.insert("is_derive".into(), json!(d));
        }
        if let Some(g) = &b.guard_rules {
            attrs.insert("guard_rules".into(), json!(g));
        }
    }
    if let Some(m) = b.modality {
        attrs.insert("modality".into(), json!(m));
    }
    if let Some(k) = b.op_kind {
        attrs.insert("op_kind".into(), json!(k));
    }
    let mut row_fields = json!({"attrs": attrs});
    if let Some(d) = &b.discriminator {
        row_fields["discriminator"] = to_json(d)?;
    }
    let read_ops: BTreeSet<String> = cat
        .resolved
        .iter()
        .filter(|r| str_attr(r, "side_effect") == Some("R"))
        .map(|r| r.key.clone())
        .collect();
    let blank = agent_model::catalog::OperationRow {
        attrs: serde_json::Map::new(),
        key: key.clone(),
        object: b.object.clone(),
        verb: b.verb.clone(),
        variant: variant.clone(),
        discriminator: b.discriminator.clone(),
        overridden: Vec::new(),
        inherited: Vec::new(),
        description: String::new(),
        retail: None,
        stages: Vec::new(),
        error_states: Vec::new(),
    };
    let cx = OperationContext {
        row_fields: &row_fields,
        star,
        verb: &b.verb,
        current: &blank,
        fields: &def.def.fields,
        read_ops: &read_ops,
    };
    let new_fields = edit::apply_operation_edit(&cx, b.edit)?;
    write_authored(
        &mut tx,
        &who.actor,
        &layer,
        Kind::Operation,
        &key,
        Op::Add,
        new_fields,
        &who.by,
        Expect::New,
    )
    .await?;
    let after = catalogue::load(&mut tx).await?;
    let untagged = catalogue::untagged(&mut tx, Kind::Operation).await?;
    let saved = after
        .resolved_op(&key)
        .ok_or_else(|| corrupt(format!("operation {key} vanished on create")))?;
    let out = op_json(&after, saved, &untagged)?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(out)))
}
