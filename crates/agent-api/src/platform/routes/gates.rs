//! B6b platform-model area `gates` (plan B6 §5): T12 gate library (`gateLibrary`, `updateGateQuestion`,
//! `addGateOption`) + agent roles ([`roles`]); T12d `createGateQuestion`, `retireGateQuestion`, `removeGateOption`.
//! `gateLibraryUsage` (the B5 bucket of Revision 2026-10-22): [`usage`], `GET /api/platform/gates/usage`.
//!
//! The library is the head (current, live) `gate_question` rows of every layer; a key can live in more than one
//! layer (`base` and a layer that patches it). A write names its row with `?layer=`; without
//! it the row that defines the key (its `add`, the least specific layer first) is the one edited. Every write is
//! one revision through `write_authored` in the row's own layer (`layer.revision` audit row by `write_revision`),
//! under the family lock, against the revision read after the lock; an edit that changes nothing writes nothing.
//! Every handler takes `SuperAdmin`; helpers in [`super`].
use agent_model::kinds::gate::{GateGroup, GateQuestionFields};
use agent_model::kinds::{Kind, Op};
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use super::{Who, blocked, conflict, missing, read_tx, write_tx};
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult};
use crate::page::paged;
use crate::platform::db::authored::{Expect, write_authored};
use crate::platform::db::layers;
use crate::platform::db::lock_family;
use crate::state::AppState;
use crate::tenancy::SuperAdmin;

mod library;
mod pins;
mod roles;
mod usage;

use library::{GateDraft, GateEdit};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/platform/gates", get(gate_library).post(create_gate))
        .route("/api/platform/gates/usage", get(usage::gate_library_usage))
        .route(
            "/api/platform/gates/{key}",
            axum::routing::patch(update_gate),
        )
        .route("/api/platform/gates/{key}/options", post(add_option))
        .route(
            "/api/platform/gates/{key}/options/{option}",
            delete(remove_option),
        )
        .route("/api/platform/gates/{key}/retire", post(retire_gate))
        .merge(roles::routes())
}

/// A current, live `gate_question` row with its layer.
#[derive(Debug, Clone, sqlx::FromRow)]
struct GateRow {
    layer_id: Uuid,
    layer_key: String,
    family: String,
    scope: String,
    key: String,
    op: String,
    fields: Value,
    rev: i32,
    author: String,
    created_at: DateTime<Utc>,
}

/// The live head rows of every `gate_question` (or of one key), by key then layer.
async fn gate_rows(conn: &mut PgConnection, key: Option<&str>) -> ApiResult<Vec<GateRow>> {
    Ok(sqlx::query_as::<_, GateRow>(
        "SELECT layer_id, layer_key, family, scope, key, op, fields, rev, author, created_at FROM layer_head \
          WHERE kind = 'gate_question' AND op <> 'tombstone' AND ($1::text IS NULL OR key = $1) \
          ORDER BY key, layer_key",
    )
    .bind(key)
    .fetch_all(conn)
    .await?)
}

fn scope_rank(scope: &str) -> u8 {
    match scope {
        "base" => 0,
        "industry" => 1,
        "base_dept" => 2,
        "industry_dept" => 3,
        "job" => 4,
        _ => 5,
    }
}

/// The row a write acts on: the one in `layer`, else the defining row (an `add` first, then the least specific
/// layer).
fn pick(rows: Vec<GateRow>, layer: Option<&str>) -> Option<GateRow> {
    match layer {
        Some(l) => rows.into_iter().find(|r| r.layer_key == l),
        None => rows.into_iter().min_by(|a, b| {
            (a.op != Op::Add.as_str(), scope_rank(&a.scope), &a.layer_key).cmp(&(
                b.op != Op::Add.as_str(),
                scope_rank(&b.scope),
                &b.layer_key,
            ))
        }),
    }
}

fn gate_not_found(key: &str, layer: Option<&str>) -> ApiError {
    match layer {
        Some(l) => missing("gate_not_found", format!("No gate {key} in {l}.")),
        None => missing("gate_not_found", format!("No gate {key}.")),
    }
}

fn unreadable(r: &GateRow, e: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!(
        "gate_question {} in {} is unreadable: {e}",
        r.key,
        r.layer_key
    ))
}

/// The gate row to write, read again under its family lock. 404 `gate_not_found`.
async fn locked_gate(
    conn: &mut PgConnection,
    key: &str,
    layer: Option<&str>,
) -> ApiResult<(GateRow, GateQuestionFields)> {
    let first = pick(gate_rows(&mut *conn, Some(key)).await?, layer)
        .ok_or_else(|| gate_not_found(key, layer))?;
    lock_family(&mut *conn, &first.family).await?;
    let row = pick(
        gate_rows(&mut *conn, Some(key)).await?,
        Some(first.layer_key.as_str()),
    )
    .ok_or_else(|| gate_not_found(key, layer))?;
    let q = library::parse(&row.fields).map_err(|e| unreadable(&row, e))?;
    Ok((row, q))
}

#[derive(Debug, Default, Deserialize)]
struct LayerQuery {
    #[serde(default)]
    layer: Option<String>,
}

/// One edit of a library question: `f` on its fields, then (when they changed) one `patch` revision in the row's
/// layer. Answers the `GateQuestion` after the edit.
async fn edit_gate(
    st: &AppState,
    sa: &SuperAdmin,
    key: &str,
    layer: Option<&str>,
    f: impl FnOnce(&mut GateQuestionFields) -> ApiResult<()>,
) -> ApiResult<Value> {
    let who = Who::of(sa);
    let mut tx = write_tx(st).await?;
    let (row, current) = locked_gate(&mut tx, key, layer).await?;
    let mut next = current.clone();
    f(&mut next)?;
    if next == current {
        return Ok(library::view(
            key,
            &row.layer_key,
            &current,
            &row.author,
            row.created_at,
        ));
    }
    next.updated_by = Some(who.by.clone());
    next.updated_at = Some(library::iso(Utc::now()));
    next.summary = None;
    let fields = serde_json::to_value(&next).map_err(|e| ApiError::Internal(e.into()))?;
    library::validate(&fields)?;
    let written = write_authored(
        &mut tx,
        &who.actor,
        &row.layer_key,
        Kind::GateQuestion,
        key,
        Op::Patch,
        fields,
        &who.by,
        Expect::Rev(row.rev),
    )
    .await?;
    tx.commit().await?;
    Ok(library::view(
        key,
        &row.layer_key,
        &next,
        &written.author,
        written.created_at,
    ))
}

// ---- reads --------------------------------------------------------------------------------------------------------

/// `GET /api/platform/gates`: every live head `gate_question`, by key then layer.
async fn gate_library(
    SuperAdmin(_): SuperAdmin,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let mut tx = read_tx(&st).await?;
    let rows = gate_rows(&mut tx, None).await?;
    let items = rows
        .iter()
        .filter_map(|r| match library::parse(&r.fields) {
            Ok(q) => Some(library::view(
                &r.key,
                &r.layer_key,
                &q,
                &r.author,
                r.created_at,
            )),
            Err(e) => {
                tracing::warn!(gate = %r.key, layer = %r.layer_key, error = %e, "gate_question_unreadable");
                None
            }
        })
        .collect();
    Ok(Json(paged(items, None)))
}

// ---- edits (T12) --------------------------------------------------------------------------------------------------

/// `PATCH /api/platform/gates/{key}[?layer=]` body `GateQuestionEdit` (+ `customer_reply`, F-16).
async fn update_gate(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(key): ApiPath<String>,
    ApiQuery(q): ApiQuery<LayerQuery>,
    ApiJson(edit): ApiJson<GateEdit>,
) -> ApiResult<Json<Value>> {
    let v = edit_gate(&st, &sa, &key, q.layer.as_deref(), |g| {
        library::apply_edit(&key, g, edit)
    })
    .await?;
    Ok(Json(v))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionIn {
    label: String,
    #[serde(default)]
    description: String,
}

/// `POST /api/platform/gates/{key}/options[?layer=]` body `{label, description}`.
async fn add_option(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(key): ApiPath<String>,
    ApiQuery(q): ApiQuery<LayerQuery>,
    ApiJson(b): ApiJson<OptionIn>,
) -> ApiResult<Json<Value>> {
    let v = edit_gate(&st, &sa, &key, q.layer.as_deref(), |g| {
        library::add_option(&key, g, &b.label, &b.description)
    })
    .await?;
    Ok(Json(v))
}

/// `DELETE /api/platform/gates/{key}/options/{option}[?layer=]` → 204.
async fn remove_option(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath((key, option)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<LayerQuery>,
) -> ApiResult<StatusCode> {
    edit_gate(&st, &sa, &key, q.layer.as_deref(), |g| {
        library::remove_option(&key, g, &option)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---- create / retire (T12d) ---------------------------------------------------------------------------------------

/// `POST /api/platform/gates` body `GateQuestionDraft` → 201 `GateQuestion`: 422 per [`library::draft_fields`];
/// 404 `layer_not_found`; 409 `gate_exists` (a live row of that key in any layer), `add_tombstoned` (the key was
/// retired in that layer); 422 `kind_not_allowed_in_layer` outside the template scopes.
async fn create_gate(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiJson(d): ApiJson<GateDraft>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let who = Who::of(&sa);
    let key = d.key.clone();
    let layer_key = d.layer.trim().to_owned();
    let q = library::draft_fields(d, &who.by, Utc::now())?;
    let mut tx = write_tx(&st).await?;
    let layer = layers::layer_by_key(&mut tx, &layer_key).await?;
    lock_family(&mut tx, &layer.family).await?;
    if let Some(live) = gate_rows(&mut tx, Some(key.as_str())).await?.first() {
        return Err(
            conflict("gate_exists", format!("The gate {key} already exists."))
                .with("layer", live.layer_key.as_str()),
        );
    }
    let fields = serde_json::to_value(&q).map_err(|e| ApiError::Internal(e.into()))?;
    let written = write_authored(
        &mut tx,
        &who.actor,
        &layer_key,
        Kind::GateQuestion,
        &key,
        Op::Add,
        fields,
        &who.by,
        Expect::New,
    )
    .await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(library::view(
            &key,
            &layer_key,
            &q,
            &written.author,
            written.created_at,
        )),
    ))
}

/// The job template rows that still use `key`: live `procedure` / `agent_role` rows of job heads with a string
/// value equal to the key anywhere in their fields.
async fn template_uses(conn: &mut PgConnection, key: &str) -> ApiResult<Vec<Value>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT h.layer_key, h.kind, h.key FROM layer_head h JOIN layer l ON l.id = h.layer_id \
          WHERE l.scope = 'job' AND l.dimension_key IS NULL \
            AND h.kind IN ('procedure', 'agent_role') AND h.op <> 'tombstone' \
            AND jsonb_path_exists(h.fields, '$.** ? (@ == $k)', jsonb_build_object('k', $1::text)) \
          ORDER BY h.layer_key, h.kind, h.key",
    )
    .bind(key)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(layer, kind, row)| {
            let job = layer.strip_prefix("job/").unwrap_or(&layer).to_owned();
            json!({"job": job, "layer": layer, "kind": kind, "key": row})
        })
        .collect())
}

/// `POST /api/platform/gates/{key}/retire[?layer=]`: a tombstone revision. 404 `gate_not_found`; 409
/// `safety_gate` (never retired, D29), `gate_in_use` (`jobs[]`: job template rows that still use it). Answers
/// `{key, layer, rev, retired, jobs}` where `jobs` are the tenant jobs whose pinned tags hold the question (a
/// warning: they keep it until their next release cut).
async fn retire_gate(
    sa: SuperAdmin,
    State(st): State<AppState>,
    ApiPath(key): ApiPath<String>,
    ApiQuery(q): ApiQuery<LayerQuery>,
) -> ApiResult<Json<Value>> {
    let who = Who::of(&sa);
    let mut tx = write_tx(&st).await?;
    let (row, gate) = locked_gate(&mut tx, &key, q.layer.as_deref()).await?;
    if gate.safety || gate.group == GateGroup::Safety {
        return Err(conflict(
            "safety_gate",
            format!("{key} is a safety gate; it stays in the library (D29)."),
        ));
    }
    let uses = template_uses(&mut tx, &key).await?;
    if !uses.is_empty() {
        return Err(blocked(
            "gate_in_use",
            format!("{key} is still used by a job template; remove it there first."),
            "jobs",
            uses,
        ));
    }
    let written = write_authored(
        &mut tx,
        &who.actor,
        &row.layer_key,
        Kind::GateQuestion,
        &key,
        Op::Tombstone,
        json!({}),
        &who.by,
        Expect::Rev(row.rev),
    )
    .await?;
    tx.commit().await?;
    let jobs = pins::pinned_jobs(&st, row.layer_id, Kind::GateQuestion.as_str(), &key).await?;
    Ok(Json(json!({
        "key": key,
        "layer": row.layer_key,
        "rev": written.rev,
        "retired": true,
        "jobs": jobs,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(layer: &str, scope: &str, op: &str) -> GateRow {
        GateRow {
            layer_id: Uuid::nil(),
            layer_key: layer.into(),
            family: layer.into(),
            scope: scope.into(),
            key: "GA".into(),
            op: op.into(),
            fields: json!({}),
            rev: 1,
            author: "owner".into(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn pick_prefers_the_defining_row() {
        let rows = vec![
            row("job/customer_care_agent", "job", "patch"),
            row("base", "base", "add"),
        ];
        assert_eq!(
            pick(rows.clone(), None).map(|r| r.layer_key),
            Some("base".to_owned())
        );
        assert_eq!(
            pick(rows.clone(), Some("job/customer_care_agent")).map(|r| r.op),
            Some("patch".to_owned())
        );
        assert!(pick(rows, Some("industry/retail")).is_none());
        let adds = vec![
            row("job/it_service_desk_agent", "job", "add"),
            row("industry/utilities", "industry", "add"),
        ];
        assert_eq!(
            pick(adds, None).map(|r| r.layer_key),
            Some("industry/utilities".to_owned())
        );
    }
}
