//! The party routes (B13 §4): `parties`, `party` (support_agent), `caseParty` (any member), `linkCaseParty`
//! (support_agent). Every write is
//! audited (ids and codes only). Shapes: `Party` = [`super::full`], `PartyBrief` = [`super::brief`].
use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

use super::PartyId;
use crate::audit::{self, Event};
use crate::conversations::store;
use crate::error::{ApiError, ApiJson, ApiPath, ApiQuery, ApiResult, json_body};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, Member};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/parties", get(parties))
        .route("/api/parties/{id}", get(party))
        .route(
            "/api/jobs/{job}/cases/{id}/party",
            get(case_party).post(link_case_party),
        )
}

/// One page of `parties`.
pub const PAGE: i64 = 50;

#[derive(Deserialize)]
struct PartiesQuery {
    q: Option<String>,
    before: Option<String>,
}

/// `parties`: the tenant's parties (merged ones left out), newest first, paged by `before` (the `next` of the
/// previous page: a party id); `q` matches the name, the customer's subject or a channel subject (case-insensitive).
async fn parties(
    Member(t): Member,
    State(st): State<AppState>,
    ApiQuery(q): ApiQuery<PartiesQuery>,
) -> ApiResult<Json<Value>> {
    t.require_conversation_worker()?;
    let before = match q.before.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(s) => Some(Uuid::parse_str(s).map_err(|_| {
            ApiError::Unprocessable(
                "invalid_cursor",
                "before must be a cursor returned as next by this list".into(),
            )
        })?),
    };
    let needle =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    if let Some(n) = &needle {
        store::require_clean([n.as_str()])?;
        if n.chars().count() > 200 {
            return Err(ApiError::Unprocessable(
                "invalid_query",
                "q is at most 200 characters".into(),
            ));
        }
    }
    let mut tx = t.begin(&st.db).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT p.id FROM party p \
          WHERE p.merged_into IS NULL \
            AND ($1::uuid IS NULL OR p.id < $1) \
            AND ($2::text IS NULL \
                 OR position(lower($2) IN lower(COALESCE(p.display_name, ''))) > 0 \
                 OR position(lower($2) IN lower(COALESCE({subject}, ''))) > 0 \
                 OR EXISTS (SELECT 1 FROM party_identity i \
                             WHERE i.party_id = p.id AND position(lower($2) IN lower(i.subject)) > 0)) \
          ORDER BY p.id DESC LIMIT $3",
        subject = super::subject_sql("p"),
    )))
    .bind(before)
    .bind(needle.as_deref())
    .bind(PAGE + 1)
    .fetch_all(&mut *tx)
    .await?;
    let more = ids.len() as i64 > PAGE;
    let mut items = Vec::new();
    let mut next = None;
    for id in ids.iter().take(PAGE as usize) {
        if let Some(v) = super::full(&mut tx, *id, super::Cases::Omit).await? {
            items.push(v);
        }
        next = Some(id.to_string());
    }
    tx.commit().await?;
    Ok(Json(crate::page::paged(
        items,
        if more { next } else { None },
    )))
}

/// `party`: one party with its identities, its newest cases of every job and `merged_into`; 404 `party_not_found`.
async fn party(
    Member(t): Member,
    State(st): State<AppState>,
    ApiPath(id): ApiPath<String>,
) -> ApiResult<Json<Value>> {
    t.require_conversation_worker()?;
    let id = Uuid::parse_str(&id).map_err(|_| super::not_found())?;
    let mut tx = t.begin(&st.db).await?;
    let v = super::full(&mut tx, id, super::Cases::Tenant).await?;
    tx.commit().await?;
    v.map(Json).ok_or_else(super::not_found)
}

/// `{case_id, party: Party | null}` of a case; the party's `cases` are this job's only (a job member sees no other
/// job's cases, keys or conversation ids).
async fn case_party_json(
    conn: &mut sqlx::PgConnection,
    j: &JobCtx,
    case_id: &str,
) -> ApiResult<Value> {
    let row = sqlx::query("SELECT party_id FROM agent_case WHERE job_id = $1 AND case_id = $2")
        .bind(j.job_id)
        .bind(case_id)
        .fetch_optional(&mut *conn)
        .await?
        .ok_or_else(|| ApiError::Missing("case_not_found", "No such case in this job.".into()))?;
    let party = match row.get::<Option<Uuid>, _>("party_id") {
        Some(p) => super::full(conn, p, super::Cases::Job(j.job_id))
            .await?
            .unwrap_or(Value::Null),
        None => Value::Null,
    };
    Ok(json!({"case_id": case_id, "party": party}))
}

/// `caseParty`; 404 `case_not_found`.
async fn case_party(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    let mut tx = j.t.begin(&st.db).await?;
    let v = case_party_json(&mut tx, &j, &id).await?;
    tx.commit().await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
struct LinkBody {
    party_id: PartyId,
}

/// `linkCaseParty` (see [`super::link`]): answers as `caseParty`.
async fn link_case_party(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    // the role is checked before the body is read (tests/roles.rs ANY_OF)
    j.t.require_conversation_worker()?;
    let b: LinkBody = json_body(raw)?;
    // the job GUC stays empty: the merge checks whether other jobs still name the source party (rows are matched by
    // job_id; only this job's rows move)
    let mut tx = j.t.begin(&st.db).await?;
    crate::members::names::actor_seen(&mut tx, &j.t.caller).await?;
    // the conversations' control locks and owner rows before the case row (the commands' lock order)
    super::guard_link(&mut tx, j.job_id, &id).await?;
    let linked = super::link(&mut tx, j.job_id, &id, b.party_id).await?;
    if linked.changed {
        audit::record(
            &mut tx,
            &j.t.actor(),
            Event::new("case.party_link", "case", id.clone())
                .job(&j.job_key)
                .detail("party_id", b.party_id.to_string())
                .detail("from_party", linked.from.map(|f| f.to_string()))
                .detail("merged", linked.merged),
        )
        .await?;
    }
    let v = case_party_json(&mut tx, &j, &id).await?;
    tx.commit().await?;
    Ok(Json(v))
}
