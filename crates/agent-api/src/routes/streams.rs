//! J24 streams and dead letters of one job's bus (B4 T14, ported from care-bus-agents@96c608c
//! `crates/carebus-api/src/routes/streams.rs`). Every key is read through the request's scoped bus
//! (crate::bus_admin), so another scope's streams are unreachable by construction. Without Redis: 503 `unavailable`.
//!
//! - `streamEntries` `GET /api/jobs/{job}/streams/{key}/entries?limit=&before=&env=` (member): newest first, K50
//!   console view of each envelope. Only an existing `bus:*` stream: else 404 `stream_not_found`.
//! - `deadLetters` `GET /api/jobs/{job}/dead-letters?limit=&before=` (member): both environments, newest first,
//!   each with its `env`. A Redis error is 503, never an empty list (unavailable is not zero).
//! - `claimStuck` `POST /api/jobs/{job}/streams/{key}/groups/{group}/claim?env=` (operator): moves the group's
//!   entries idle ≥ 30 s whose owner no longer beats (operator-triggered only; a slow live handler is skipped by its heartbeat) to `bus:dead`, reason `stuck` (bus_admin::claim says
//!   why not to a consumer); `{claimed}` is how many; 404 `group_not_found`.
//! - `replayDeadLetter` `POST /api/jobs/{job}/dead-letters/{id}/replay` (operator): 404 `dead_letter_not_found`, 409
//!   `scope_mismatch` (another scope's message: discard it), 409 `not_replayable` (no readable original).
//! - `discardDeadLetter` `DELETE /api/jobs/{job}/dead-letters/{id}` (operator): 404 `dead_letter_not_found`.
//!
//! The three writes are audited (`stream.claim_stuck`, `dead_letter.replay`, `dead_letter.discard`) after the Redis
//! change, which a Postgres transaction cannot hold.
use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::{delete, get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::audit::{self, Event};
use crate::bus_admin::{self, redis_failed};
use crate::error::{ApiError, ApiPath, ApiQuery, ApiResult};
use crate::page::{limit, paged};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, JobRequires, role};

/// Page size of `streamEntries` / `deadLetters` (default, max).
const PAGE: (i64, i64) = (20, 200);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/jobs/{job}/streams/{key}/entries", get(stream_entries))
        .route(
            "/api/jobs/{job}/streams/{key}/groups/{group}/claim",
            post(claim_stuck),
        )
        .route("/api/jobs/{job}/dead-letters", get(dead_letters))
        .route(
            "/api/jobs/{job}/dead-letters/{id}/replay",
            post(replay_dead_letter),
        )
        .route(
            "/api/jobs/{job}/dead-letters/{id}",
            delete(discard_dead_letter),
        )
}

#[derive(Deserialize)]
struct PageQuery {
    limit: Option<String>,
    before: Option<String>,
    env: Option<String>,
}

#[derive(Deserialize)]
struct EnvQuery {
    env: Option<String>,
}

/// `before` is a stream id (exclusive); anything else is 422 `invalid_cursor`.
fn stream_cursor(raw: Option<&str>) -> ApiResult<Option<String>> {
    match raw.map(str::trim) {
        None | Some("") => Ok(None),
        Some(b) if bus_admin::valid_stream_id(b) => Ok(Some(b.to_string())),
        Some(_) => Err(ApiError::Unprocessable(
            "invalid_cursor",
            "before must be a cursor returned as next by this list".into(),
        )),
    }
}

fn stream_not_found(key: &str) -> ApiError {
    ApiError::Missing("stream_not_found", format!("No stream {key}."))
}

fn group_not_found(key: &str, group: &str) -> ApiError {
    ApiError::Missing("group_not_found", format!("No group {group} on {key}."))
}

/// One audit row in the job's scope.
async fn audited(st: &AppState, j: &JobCtx, e: Event) -> ApiResult<()> {
    let mut tx = j.begin(&st.db).await?;
    audit::record(&mut tx, &j.t.actor(), e.job(&j.job_key)).await?;
    tx.commit().await?;
    Ok(())
}

/// `streamEntries`.
async fn stream_entries(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, key)): ApiPath<(String, String)>,
    ApiQuery(q): ApiQuery<PageQuery>,
) -> ApiResult<Json<Value>> {
    let env = bus_admin::env_param(q.env.as_deref())?;
    let n = limit(q.limit.as_deref(), PAGE.0, PAGE.1)?;
    let before = stream_cursor(q.before.as_deref())?;
    let root = bus_admin::root(&st)?;
    let bus = root.scoped(&bus_admin::scope_of(&j, env)?);
    if !bus_admin::owns(&bus, &key).await.map_err(redis_failed)? {
        return Err(stream_not_found(&key));
    }
    let (items, next) = bus_admin::entries_page(
        &bus,
        &key,
        usize::try_from(n).unwrap_or(1),
        before.as_deref(),
    )
    .await
    .map_err(redis_failed)?;
    let items = items
        .iter()
        .map(|(id, e)| bus_admin::entry_json(&key, id, e))
        .collect();
    Ok(Json(paged(items, next)))
}

/// `deadLetters`.
async fn dead_letters(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<PageQuery>,
) -> ApiResult<Json<Value>> {
    let n = limit(q.limit.as_deref(), PAGE.0, PAGE.1)?;
    let before = stream_cursor(q.before.as_deref())?;
    let root = bus_admin::root(&st)?;
    let (items, next) =
        bus_admin::dead_page(root, &j, usize::try_from(n).unwrap_or(1), before.as_deref()).await?;
    Ok(Json(paged(items, next)))
}

/// `claimStuck`.
async fn claim_stuck(
    JobRequires(j, _): JobRequires<role::Operator>,
    State(st): State<AppState>,
    ApiPath((_, key, group)): ApiPath<(String, String, String)>,
    ApiQuery(q): ApiQuery<EnvQuery>,
) -> ApiResult<Json<Value>> {
    let env = bus_admin::env_param(q.env.as_deref())?;
    let root = bus_admin::root(&st)?;
    let bus = root.scoped(&bus_admin::scope_of(&j, env)?);
    if !bus_admin::owns(&bus, &key).await.map_err(redis_failed)? {
        return Err(group_not_found(&key, &group));
    }
    let Some(claimed) = bus_admin::claim(&bus, &key, &group).await? else {
        return Err(group_not_found(&key, &group));
    };
    audited(
        &st,
        &j,
        Event::new("stream.claim_stuck", "stream", format!("{key}#{group}"))
            .detail("env", env.as_str())
            .detail("claimed", claimed),
    )
    .await?;
    Ok(Json(json!({ "claimed": claimed })))
}

/// `replayDeadLetter`.
async fn replay_dead_letter(
    JobRequires(j, _): JobRequires<role::Operator>,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let root = bus_admin::root(&st)?;
    let (bus, env, dl) = bus_admin::find_dead(root, &j, &id)
        .await?
        .ok_or_else(bus_admin::dead_letter_not_found)?;
    let r = bus_admin::replay(&bus, &id, &dl).await?;
    audited(
        &st,
        &j,
        Event::new("dead_letter.replay", "dead_letter", id.clone())
            .detail("env", env.as_str())
            .detail("stream", r.stream.clone())
            .detail("new_entry_id", r.entry_id.clone()),
    )
    .await?;
    Ok(Json(json!({ "stream": r.stream, "entry_id": r.entry_id })))
}

/// `discardDeadLetter`.
async fn discard_dead_letter(
    JobRequires(j, _): JobRequires<role::Operator>,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let root = bus_admin::root(&st)?;
    let Some((bus, env, _)) = bus_admin::find_dead(root, &j, &id).await? else {
        return Err(bus_admin::dead_letter_not_found());
    };
    // XDEL is the claim: of a concurrent discard and replay only one removes it
    if bus_admin::delete(&bus, bus_admin::DEAD, &id)
        .await
        .map_err(redis_failed)?
        != 1
    {
        return Err(bus_admin::dead_letter_not_found());
    }
    audited(
        &st,
        &j,
        Event::new("dead_letter.discard", "dead_letter", id.clone()).detail("env", env.as_str()),
    )
    .await?;
    Ok(Json(json!({ "discarded": id })))
}
