//! D125 (tenant job design): the staff side is configured per job (migration 0213).
//!
//! - **Team** (`job_staff`): the subs that work a job's conversations. No rows = every conversation worker of the
//!   tenant (today's behaviour). A `support_agent` (without `support_manager`) works a job only when the team is empty
//!   or they are on it ([`eligible`]): the inbox, the stream, Take, reply and auto-assignment all apply it. A
//!   `support_manager` works every job; a manager assigns only to a member eligible for the job (422 `not_job_staff`,
//!   [`require_assignable`]). Every other role (`operator`, `job_designer`, ...) is not restricted.
//! - **Routing** (`job_routing`): `inherit` (the tenant's `support_routing`), `off` or `round_robin`, with an optional
//!   own reassign window.
//! - `GET /api/jobs/{job}/staff` (any member; the `candidates` list with emails and roles only for a caller who may
//!   edit the team) and `PUT /api/jobs/{job}/staff` (`tenant_admin` or `support_manager`, checked before the body is
//!   decoded; tests/roles.rs ANY_OF), audited `job.staff_update`.
use std::collections::BTreeSet;

use axum::extract::State;
use axum::response::Json;
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

use crate::audit::{self, Event};
use crate::auth::{CONVERSATION_WORKERS_SQL, Role, RoleSet};
use crate::conversations::store;
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::state::AppState;
use crate::tenancy::{JobCtx, JobMember, missing_role};

/// SQL predicate: job `{job}` (an expression naming `jobs.id`/`conversation.job_id` with `tenant_id` expression
/// `{tenant}`) has an empty team or has `{sub}` (a bind or column) on it.
pub fn team_open_sql(tenant: &str, job: &str, sub: &str) -> String {
    format!(
        "(NOT EXISTS (SELECT 1 FROM job_staff js WHERE js.tenant_id = {tenant} AND js.job_id = {job}) \
          OR EXISTS (SELECT 1 FROM job_staff js WHERE js.tenant_id = {tenant} AND js.job_id = {job} \
                      AND js.sub = {sub}))"
    )
}

/// Is a caller holding `roles` limited to the jobs whose team they are on? A support_agent without support_manager.
pub fn restricted(roles: RoleSet) -> bool {
    roles.has(Role::SupportAgent) && !roles.has(Role::SupportManager)
}

fn not_job_staff(msg: &str) -> ApiError {
    ApiError::Forbidden("not_job_staff", msg.to_string())
}

/// May `sub`, holding `effective_roles`, work job `job_id`'s conversations? A manager (or any non-agent role): yes; an
/// agent: when the job's team is empty or they are on it.
pub async fn eligible(
    conn: &mut PgConnection,
    job_id: Uuid,
    sub: &str,
    effective_roles: RoleSet,
) -> ApiResult<bool> {
    if !restricted(effective_roles) {
        return Ok(true);
    }
    on_team_or_open(conn, job_id, sub).await
}

async fn on_team_or_open(conn: &mut PgConnection, job_id: Uuid, sub: &str) -> ApiResult<bool> {
    let sql = format!(
        "SELECT {}",
        team_open_sql("app_current_tenant()", "$1", "$2")
    );
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(job_id)
        .bind(sub)
        .fetch_one(conn)
        .await?)
}

/// 403 `not_job_staff` unless the caller may work this job's conversations (opens its own transaction).
pub async fn require_staff(st: &AppState, j: &JobCtx) -> ApiResult<()> {
    if !restricted(j.t.effective) {
        return Ok(());
    }
    let mut tx = j.begin(&st.db).await?;
    let ok = on_team_or_open(&mut tx, j.job_id, &j.t.caller.sub).await?;
    tx.commit().await?;
    if ok {
        Ok(())
    } else {
        Err(not_job_staff("You are not on this job's staff team."))
    }
}

/// Which conversation a staff write names, for [`require_staff_or_owner`].
#[derive(Clone, Copy)]
pub enum Target<'a> {
    Conversation(&'a str),
    /// A handover item id.
    Item(&'a str),
    /// A case id.
    Case(&'a str),
}

impl Target<'_> {
    /// SQL (job `$1`, target `$3`) selecting the target's conversation id.
    fn conversation_sql(self) -> &'static str {
        match self {
            Target::Conversation(_) => "SELECT $3::text",
            Target::Item(_) => {
                "SELECT i.conversation_id FROM agent_handover_items i WHERE i.job_id = $1 AND i.id = $3"
            }
            Target::Case(_) => {
                "SELECT a.conversation_id FROM agent_case a WHERE a.job_id = $1 AND a.case_id = $3"
            }
        }
    }
}

fn target_id(t: Target<'_>) -> &str {
    match t {
        Target::Conversation(s) | Target::Item(s) | Target::Case(s) => s,
    }
}

/// [`require_staff`], except that the caller who currently owns the target's conversation (a staff owner row naming
/// them) passes: removing someone from a team must not strand their own open conversations (D125). Hand-back,
/// resolve, hand-over, reply and notes use it; Take does not (a Take by a non-member is refused).
pub async fn require_staff_or_owner(st: &AppState, j: &JobCtx, t: Target<'_>) -> ApiResult<()> {
    if !restricted(j.t.effective) {
        return Ok(());
    }
    let sql = format!(
        "SELECT {} OR EXISTS (SELECT 1 FROM conversation_control cc \
                              WHERE cc.job_id = $1 AND cc.mode = 'staff' AND cc.by_sub = $2 \
                                AND cc.conversation_id IN ({}))",
        team_open_sql("app_current_tenant()", "$1", "$2"),
        t.conversation_sql()
    );
    let mut tx = j.begin(&st.db).await?;
    let ok: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(j.job_id)
        .bind(&j.t.caller.sub)
        .bind(target_id(t))
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    if ok {
        Ok(())
    } else {
        Err(not_job_staff("You are not on this job's staff team."))
    }
}

/// 422 `not_job_staff` unless `to` (a worker) may work job `job_id`: the team is empty or `to` is on it. A manager
/// assigning to another manager is allowed only when the team is empty or they are on it too: the team names who
/// works the job.
pub async fn require_assignable(conn: &mut PgConnection, job_id: Uuid, to: &str) -> ApiResult<()> {
    if on_team_or_open(conn, job_id, to).await? {
        Ok(())
    } else {
        Err(ApiError::Unprocessable(
            "not_job_staff",
            "That member is not on this job's staff team.".into(),
        ))
    }
}

fn invalid(msg: &str) -> ApiError {
    ApiError::Unprocessable("invalid_staff", msg.to_string())
}

/// `{members, routing, candidates}` of job `j`; `candidates` is empty unless `editor`.
async fn read(conn: &mut PgConnection, job_id: Uuid, editor: bool) -> ApiResult<Value> {
    let name = store::MEMBER_NAME_SQL;
    let members = sqlx::query(
        "SELECT js.sub, u.display_name, u.email FROM job_staff js \
           LEFT JOIN users u ON u.tenant_id = js.tenant_id AND u.sub = js.sub \
          WHERE js.job_id = $1 ORDER BY js.sub",
    )
    .bind(job_id)
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| {
        json!({
            "sub": r.get::<String, _>("sub"),
            "name": r.get::<Option<String>, _>("display_name"),
            "email": r.get::<Option<String>, _>("email"),
        })
    })
    .collect::<Vec<_>>();
    let routing =
        sqlx::query("SELECT mode, reassign_after_secs FROM job_routing WHERE job_id = $1")
            .bind(job_id)
            .fetch_optional(&mut *conn)
            .await?
            .map(|r| {
                json!({
                    "mode": r.get::<String, _>("mode"),
                    "reassign_after_secs": r.get::<Option<i32>, _>("reassign_after_secs"),
                })
            })
            .unwrap_or_else(|| json!({"mode": "inherit", "reassign_after_secs": null}));
    if !editor {
        return Ok(json!({"members": members, "routing": routing, "candidates": []}));
    }
    let candidates = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT sub, display_name, email, roles FROM users \
          WHERE disabled_at IS NULL AND {CONVERSATION_WORKERS_SQL} \
          ORDER BY lower({name}), sub"
    )))
    .fetch_all(&mut *conn)
    .await?
    .iter()
    .map(|r| {
        json!({
            "sub": r.get::<String, _>("sub"),
            "name": r.get::<Option<String>, _>("display_name"),
            "email": r.get::<Option<String>, _>("email"),
            "roles": r.get::<Vec<String>, _>("roles"),
        })
    })
    .collect::<Vec<_>>();
    Ok(json!({"members": members, "routing": routing, "candidates": candidates}))
}

/// `GET /api/jobs/{job}/staff`.
pub async fn get_staff(
    JobMember(j): JobMember,
    State(st): State<AppState>,
) -> ApiResult<Json<Value>> {
    let editor = can_edit(&j);
    let mut tx = j.begin(&st.db).await?;
    let v = read(&mut tx, j.job_id, editor).await?;
    tx.commit().await?;
    Ok(Json(v))
}

/// tenant_admin or support_manager: may change the team and see the candidate list.
fn can_edit(j: &JobCtx) -> bool {
    j.t.effective.has(Role::TenantAdmin) || j.t.effective.has(Role::SupportManager)
}

#[derive(Deserialize)]
struct RoutingIn {
    mode: String,
    #[serde(default)]
    reassign_after_secs: Option<i64>,
}

#[derive(Deserialize)]
struct StaffBody {
    #[serde(default)]
    members: Vec<String>,
    routing: RoutingIn,
}

/// `PUT /api/jobs/{job}/staff` `{members:[sub], routing:{mode, reassign_after_secs?}}`: replaces the team and the
/// routing setting. 403 `missing_role` (`support_manager`) unless tenant_admin or support_manager; 422
/// `invalid_staff`, `not_a_worker`.
pub async fn put_staff(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiJson(raw): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    if !can_edit(&j) {
        return Err(missing_role(Role::SupportManager));
    }
    let b: StaffBody = serde_json::from_value(raw).map_err(|e| {
        ApiError::BadRequest(
            "invalid_body",
            format!("The request body is not valid: {e}"),
        )
    })?;
    let members: BTreeSet<String> = b.members.iter().map(|s| s.trim().to_string()).collect();
    if members.len() > 200 || members.iter().any(String::is_empty) {
        return Err(invalid("members is at most 200 non-empty subs."));
    }
    store::require_clean(members.iter().map(String::as_str))?;
    if !matches!(b.routing.mode.as_str(), "inherit" | "off" | "round_robin") {
        return Err(invalid("routing.mode is inherit, off or round_robin."));
    }
    let secs = match b.routing.reassign_after_secs {
        None => None,
        Some(n) if (60..=86_400).contains(&n) => Some(n as i32),
        Some(_) => {
            return Err(invalid(
                "reassign_after_secs is between 60 seconds and 1 day.",
            ));
        }
    };
    let mut tx = j.begin(&st.db).await?;
    if !members.is_empty() {
        let list: Vec<String> = members.iter().cloned().collect();
        let ok: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT sub FROM users WHERE sub = ANY($1) AND disabled_at IS NULL AND {CONVERSATION_WORKERS_SQL}"
        )))
        .bind(&list)
        .fetch_all(&mut *tx)
        .await?;
        if let Some(bad) = list.iter().find(|s| !ok.contains(s)) {
            return Err(ApiError::Unprocessable(
                "not_a_worker",
                format!(
                    "{bad} is not an enabled member holding the Support agent or Support manager role."
                ),
            ));
        }
    }
    let old: Vec<String> =
        sqlx::query_scalar("SELECT sub FROM job_staff WHERE job_id = $1 ORDER BY sub")
            .bind(j.job_id)
            .fetch_all(&mut *tx)
            .await?;
    let list: Vec<String> = members.iter().cloned().collect();
    sqlx::query("DELETE FROM job_staff WHERE job_id = $1 AND NOT (sub = ANY($2))")
        .bind(j.job_id)
        .bind(&list)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO job_staff (job_id, sub, added_by) SELECT $1, s, $3 FROM unnest($2::text[]) AS s \
         ON CONFLICT (tenant_id, job_id, sub) DO NOTHING",
    )
    .bind(j.job_id)
    .bind(&list)
    .bind(&j.t.caller.sub)
    .execute(&mut *tx)
    .await?;
    // Serialise the mode change with the assigners. `route_item` reads the job's mode and the online members inside
    // one transaction that holds this tenant's `support_routing` row lock; without the same lock here a mode change
    // can commit between those two reads, so an assignment that started while the mode was `inherit` lands after it
    // was turned off. Measured 2026-11-07 as a CI-only failure of
    // `job_staff::the_sweeper_assigns_only_to_the_team_and_honours_the_jobs_mode` (u-agent2 assigned where `queue`
    // was expected). Only the mode write is ordered by this lock; the team rows above are written before it.
    sqlx::query(
        "INSERT INTO support_routing (tenant_id) VALUES (app_current_tenant()) ON CONFLICT (tenant_id) DO NOTHING",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "SELECT mode FROM support_routing WHERE tenant_id = app_current_tenant() FOR UPDATE",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO job_routing (job_id, mode, reassign_after_secs, updated_by) VALUES ($1, $2, $3, $4) \
         ON CONFLICT (tenant_id, job_id) DO UPDATE \
            SET mode = $2, reassign_after_secs = $3, updated_at = now(), updated_by = $4",
    )
    .bind(j.job_id)
    .bind(&b.routing.mode)
    .bind(secs)
    .bind(&j.t.caller.sub)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &j.t.actor(),
        Event::new("job.staff_update", "job", j.job_id.to_string())
            .job(&j.job_key)
            .detail("members_old", old.len())
            .detail("members_new", list.len())
            .detail("mode", b.routing.mode.as_str())
            .detail("reassign_after_secs", secs),
    )
    .await?;
    let v = read(&mut tx, j.job_id, true).await?;
    tx.commit().await?;
    Ok(Json(v))
}
