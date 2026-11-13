//! Tenant members (B1 T12; P3 T11/T14): invite by email, add by Keycloak subject, role change, soft disable / enable,
//! member activity, and the super_admin re-home of a disabled user into another tenant.
//!
//! - `POST /api/tenant/invites` `{email, roles, display_name?}` — tenant_admin: 202 `InviteAccepted` (its `GET`, the
//!   invite list, and resend / revoke are B1b: crate::routes::invites).
//! - `PUT /api/tenant/users/{sub}` `{roles}` — tenant_admin: 201 `TenantUser` (add by sub).
//! - `PATCH /api/tenant/users/{sub}` `{roles}` — tenant_admin: `TenantUser` (the whole set; empty = observer).
//!
//! Roles (D88, plan §R.5): a body names a role SET (422 `invalid_role` / `duplicate_role`); the last-admin guard
//! counts the roles HELD (never a view-as).
//! - `POST /api/tenant/users/{sub}/disable|enable` — tenant admin: `TenantUser` (idempotent; a no-op writes no audit).
//! - `GET /api/tenant/users/activity` — any member: `MemberActivity[]`.
//! - `POST /api/users/{sub}/rehome` `{tenant_id}` — super_admin: `TenantUser` in the target tenant.
//!
//! Licences (D77 L5/L9/L10): a membership write leaves the member's entitlement `pending` (or `revoking`) inside its
//! transaction; the mint (crate::members::ensure_member_licence) or revoke (crate::members::revoke_member_licence)
//! runs after COMMIT, and a failure stays `pending` / `revoking` for the provisioning tick. An invite seats ONLY an
//! identity it created (minted on the invitee's first `GET /api/me`); a pre-existing portal account gets a pending
//! `member_invites` row bound to it and is seated when it signs in with the verified invited email (CP). No email address ever reaches an audit row: an
//! invite records `email_domain`.
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit::{self, Actor, Event, PlatformEvent};
use crate::auth::{Role, RoleSet};
use crate::error::{ApiError, ApiJson, ApiPath, ApiResult};
use crate::identity::{PersonNames, keycloak_error, person_name};
use crate::members::{self, entitle_pending, entitle_revoking};
use crate::platform_db::{begin_platform, enter, leave};
use crate::readmodel::tenant::{self as tenant_rm, UserRow, user_json};
use crate::state::AppState;
use crate::tenancy::{
    Member, Requires, SuperAdmin, TenantAdmin, begin_scoped, lock_tenant_row, set_tenant,
};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/tenant/invites",
            get(crate::routes::invites::list_invites).post(invite),
        )
        .route("/api/tenant/users/activity", get(activity))
        .route("/api/tenant/users/{sub}", put(add_by_sub).patch(set_role))
        .route("/api/tenant/users/{sub}/disable", post(disable))
        .route("/api/tenant/users/{sub}/enable", post(enable))
        .route("/api/users/{sub}/rehome", post(rehome))
}

#[derive(Deserialize)]
struct SubPath {
    sub: String,
}

/// A role set as stored (`users.roles`).
fn held(u: &UserRow) -> ApiResult<RoleSet> {
    RoleSet::from_db(&u.roles)
}

/// A lower-cased email address, else 422 `invalid_email`.
fn email(raw: &str) -> ApiResult<String> {
    let e = raw.trim().to_lowercase();
    if e.chars().count() > 254 || !audit::scrub::looks_like_email(&e) {
        return Err(ApiError::Unprocessable(
            "invalid_email",
            "email must be one email address".into(),
        ));
    }
    Ok(e)
}

/// A Keycloak subject from the path: 1 to 255 characters, no whitespace or control characters; else 422 `invalid_sub`.
fn subject(raw: &str) -> ApiResult<String> {
    if raw.is_empty()
        || raw.chars().count() > 255
        || raw.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ApiError::Unprocessable(
            "invalid_sub",
            "the subject must be a Keycloak user id".into(),
        ));
    }
    Ok(raw.to_string())
}

/// A display name: one line of 1 to 120 characters, else 422 `invalid_display_name`.
fn display_name(raw: &str) -> ApiResult<String> {
    let n = raw.trim();
    if n.is_empty() || n.chars().count() > 120 || n.chars().any(char::is_control) {
        return Err(ApiError::Unprocessable(
            "invalid_display_name",
            "display_name must be one line of 1 to 120 characters".into(),
        ));
    }
    Ok(n.to_string())
}

fn user_not_found(sub: &str) -> ApiError {
    ApiError::Missing(
        "user_not_found",
        format!("no member {sub:?} in this tenant"),
    )
}

fn is_unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "23505")
}

/// The member `sub` of the scoped tenant, else 404 `user_not_found`.
async fn member(conn: &mut PgConnection, sub: &str) -> ApiResult<UserRow> {
    tenant_rm::user(conn, sub)
        .await?
        .ok_or_else(|| user_not_found(sub))
}

/// The `TenantUser` of `sub` in `tenant`, read in its own scoped transaction.
async fn answer(st: &AppState, tenant: Uuid, sub: &str) -> ApiResult<Value> {
    let mut tx = begin_scoped(&st.db, tenant).await?;
    let u = member(&mut tx, sub).await?;
    tx.commit().await?;
    Ok(user_json(&u))
}

/// The home tenant of an (enabled) member, wherever it is (`agent_auth_user`, the one cross-tenant user read).
async fn home_of(st: &AppState, sub: &str) -> ApiResult<Option<Uuid>> {
    Ok(
        sqlx::query_scalar("SELECT tenant_id FROM agent_auth_user($1)")
            .bind(sub)
            .fetch_optional(&st.db)
            .await?,
    )
}

/// Lock the tenant's enabled admins (`FOR UPDATE`, CFG `users::lock_admins`) and count those other than `sub` that
/// accepted their invite: the last-admin guard (it applies to everyone, a super_admin included).
async fn other_admins(conn: &mut PgConnection, sub: &str) -> ApiResult<usize> {
    let admins: Vec<(String, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT sub, invited_at FROM users \
          WHERE 'tenant_admin' = ANY (roles) AND disabled_at IS NULL ORDER BY sub FOR UPDATE",
    )
    .fetch_all(conn)
    .await?;
    Ok(admins
        .iter()
        .filter(|(s, invited)| s != sub && invited.is_none())
        .count())
}

fn last_admin() -> ApiError {
    ApiError::Conflict(
        "last_tenant_admin",
        "a tenant keeps at least one active tenant_admin".into(),
    )
}

// ---- invite -----------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InviteBody {
    email: String,
    roles: Vec<String>,
    display_name: Option<String>,
    /// Explicit names (the invite form sends both); they win over splitting `display_name` (commonpractice
    /// `inviteMemberSchema`). With both, the invitee's first sign-in asks only for a password.
    first_name: Option<String>,
    last_name: Option<String>,
}

fn user_exists() -> ApiError {
    ApiError::Conflict(
        "user_exists",
        "that person is already a member of this tenant".into(),
    )
}

async fn invite(
    Requires(t, _): TenantAdmin,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<InviteBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let email = email(&b.email)?;
    let roles = RoleSet::from_keys(&b.roles)?;
    let first = person_name("first_name", b.first_name.as_deref())?;
    let last = person_name("last_name", b.last_name.as_deref())?;
    let mut name = b.display_name.as_deref().map(display_name).transpose()?;
    let domain = email.rsplit_once('@').map(|(_, d)| d.to_string());
    let mut tx = t.begin(&st.db).await?;
    let taken: Option<String> = sqlx::query_scalar("SELECT sub FROM users WHERE lower(email) = $1")
        .bind(&email)
        .fetch_optional(&mut *tx)
        .await?;
    tx.commit().await?;
    if taken.is_some() {
        return Err(user_exists());
    }
    let local = email.split('@').next().unwrap_or_default().to_string();
    let explicit = first.is_some() || last.is_some();
    let names = PersonNames::resolve(first, last, name.as_deref().unwrap_or(&local));
    if name.is_none() && explicit {
        name = names.joined();
    }
    let shown = name.clone().unwrap_or(local);
    let (sub, existed) = st
        .identity
        .resolve_or_create(&email, &names, None)
        .await
        .map_err(|e| keycloak_error(&e))?;
    match home_of(&st, &sub).await? {
        Some(h) if h == t.tenant => return Err(user_exists()),
        Some(_) => {
            return Err(ApiError::Conflict(
                "user_exists_elsewhere",
                "that person is a member of another tenant".into(),
            ));
        }
        None => {}
    }
    let actor = t.actor();
    let mut tx = t.begin(&st.db).await?;
    lock_tenant_row(&mut tx, t.tenant).await?;
    if tenant_rm::user(&mut tx, &sub).await?.is_some() {
        return Err(user_exists());
    }
    // CP invite-service: ONLY an identity this call created is seated now. A pre-existing portal account (its email
    // possibly unverified) gets a pending invite bound to it: no member row, no roles, no licence until it signs in
    // with the verified invited email (crate::members::accept).
    if !existed {
        sqlx::query(
            "INSERT INTO users (sub, roles, display_name, email, origin, invited_at, added_by, updated_by) \
             VALUES ($1, $2, $3, $4, 'invite', now(), $5, $5)",
        )
        .bind(&sub)
        .bind(roles.keys())
        .bind(name.as_deref().unwrap_or(&shown))
        .bind(&email)
        .bind(&t.caller.sub)
        .execute(&mut *tx)
        .await
        .map_err(|e| {
            if is_unique_violation(&e) {
                ApiError::Conflict(
                    "user_exists_elsewhere",
                    "that person is a member of another tenant".into(),
                )
            } else {
                e.into()
            }
        })?;
        entitle_pending(&mut tx, t.tenant, &sub).await?;
    }
    // The invite record (0040, owner F-01): state, expiry, resend bookkeeping. Accepted on the invitee's first
    // sign-in (crate::members::seen); list / resend / revoke are B1b.
    sqlx::query(
        "INSERT INTO member_invites (tenant_id, email, roles, invitee_sub, invited_by) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
    )
    .bind(t.tenant)
    .bind(&email)
    .bind(roles.keys())
    .bind(&sub)
    .bind(&t.caller.sub)
    .execute(&mut *tx)
    .await?;
    audit::record(
        &mut tx,
        &actor,
        Event::new("user.invite", "user", sub.clone())
            .detail("roles", roles.keys())
            .detail("email_domain", domain)
            .detail("existing_identity", existed),
    )
    .await?;
    tx.commit().await?;
    // After COMMIT: the member row exists whatever the email does; a failure is audited and shown on the home page.
    let sent = st.identity.send_access(&sub, existed).await;
    members::access_email_outcome(&st, t.tenant, &actor, &sub, "invite", &sent).await;
    // A failure is logged and audited by access_email_outcome; the member row stays.
    if sent.is_ok() {
        let mut tx = t.begin(&st.db).await?;
        sqlx::query(
            "UPDATE member_invites SET send_count = send_count + 1, last_sent_at = now(), updated_at = now() \
              WHERE invitee_sub = $1 AND state = 'pending'",
        )
        .bind(&sub)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
    }
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({"status": "invite_sent", "sub": sub, "email": email, "seated": !existed})),
    ))
}

// ---- add by sub -------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleBody {
    roles: Vec<String>,
}

fn in_other_tenant() -> ApiError {
    ApiError::Conflict(
        "user_in_other_tenant",
        "that person is a member of another tenant".into(),
    )
}

async fn add_by_sub(
    Requires(t, _): TenantAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<SubPath>,
    ApiJson(b): ApiJson<RoleBody>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let sub = subject(&p.sub)?;
    let roles = RoleSet::from_keys(&b.roles)?;
    let mut tx = t.begin(&st.db).await?;
    let here = tenant_rm::user(&mut tx, &sub).await?;
    tx.commit().await?;
    if here.is_some() {
        return Err(user_exists());
    }
    let who = st
        .identity
        .get_user(&sub)
        .await
        .map_err(|e| keycloak_error(&e))?
        .ok_or_else(|| {
            ApiError::Missing(
                "unknown_subject",
                "no portal account has that subject".into(),
            )
        })?;
    let operator = st
        .identity
        .has_agent_role(&sub, &st.cfg.super_admin_role)
        .await
        .map_err(|e| keycloak_error(&e))?;
    if operator {
        return Err(ApiError::Conflict(
            "super_admin_not_member",
            "a super_admin acts in tenants without being a member of one".into(),
        ));
    }
    if home_of(&st, &sub).await?.is_some() {
        return Err(in_other_tenant());
    }
    let actor = t.actor();
    let mut tx = t.begin(&st.db).await?;
    lock_tenant_row(&mut tx, t.tenant).await?;
    sqlx::query(
        "INSERT INTO users (sub, roles, display_name, email, origin, added_by, updated_by) \
         VALUES ($1, $2, $3, $4, 'add_by_sub', $5, $5)",
    )
    .bind(&sub)
    .bind(roles.keys())
    .bind(who.name.as_deref())
    .bind(who.email.as_deref().map(str::to_lowercase))
    .bind(&t.caller.sub)
    .execute(&mut *tx)
    .await
    .map_err(|e| {
        if is_unique_violation(&e) {
            in_other_tenant()
        } else {
            e.into()
        }
    })?;
    entitle_pending(&mut tx, t.tenant, &sub).await?;
    audit::record(
        &mut tx,
        &actor,
        Event::new("user.add", "user", sub.clone()).detail("roles", roles.keys()),
    )
    .await?;
    tx.commit().await?;
    mint_after_commit(&st, t.tenant, &sub, &actor).await;
    Ok((
        StatusCode::CREATED,
        Json(answer(&st, t.tenant, &sub).await?),
    ))
}

/// The member licence after COMMIT (D77 L10): a failure stays `pending` for the tick and never fails the request.
async fn mint_after_commit(st: &AppState, tenant: Uuid, sub: &str, actor: &Actor) {
    match members::ensure_member_licence(st, tenant, sub, actor).await {
        Ok(o) => tracing::debug!(outcome = ?o, "member_licence_after_commit"),
        Err(e) => tracing::warn!(error = %e, "member_licence_after_commit_failed"),
    }
}

/// The revoke after COMMIT: a failure stays `revoking` (audited) for the tick and never fails the request.
async fn revoke_after_commit(st: &AppState, tenant: Uuid, sub: &str, actor: &Actor) {
    match members::revoke_member_licence(st, tenant, sub, actor, true).await {
        Ok(o) => tracing::debug!(outcome = ?o, "member_revoke_after_commit"),
        Err(e) => tracing::warn!(error = %e, "member_revoke_after_commit_failed"),
    }
}

// ---- role, disable, enable --------------------------------------------------------------------------------------

async fn set_role(
    Requires(t, _): TenantAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<SubPath>,
    ApiJson(b): ApiJson<RoleBody>,
) -> ApiResult<Json<Value>> {
    let roles = RoleSet::from_keys(&b.roles)?;
    let mut tx = t.begin(&st.db).await?;
    // T0d: the tenants row before the admins' rows (the audit insert below reaches it), as an offboard takes them.
    lock_tenant_row(&mut tx, t.tenant).await?;
    let others = other_admins(&mut tx, &p.sub).await?;
    let u = member(&mut tx, &p.sub).await?;
    if u.disabled_at.is_some() {
        return Err(ApiError::Conflict(
            "user_disabled",
            "enable the member before changing their role".into(),
        ));
    }
    let from = held(&u)?;
    if from == roles {
        tx.commit().await?;
        return Ok(Json(user_json(&u)));
    }
    if from.has(Role::TenantAdmin) && !roles.has(Role::TenantAdmin) && others == 0 {
        return Err(last_admin());
    }
    sqlx::query("UPDATE users SET roles = $2, updated_at = now(), updated_by = $3 WHERE sub = $1")
        .bind(&u.sub)
        .bind(roles.keys())
        .bind(&t.caller.sub)
        .execute(&mut *tx)
        .await?;
    audit::record(
        &mut tx,
        &t.actor(),
        Event::new("user.role", "user", u.sub.clone())
            .detail("from", from.keys())
            .detail("to", roles.keys()),
    )
    .await?;
    tx.commit().await?;
    Ok(Json(answer(&st, t.tenant, &u.sub).await?))
}

async fn disable(
    Requires(t, _): TenantAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<SubPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    // T0d: the tenants row before any member row; locking the admins first deadlocked against an offboard.
    lock_tenant_row(&mut tx, t.tenant).await?;
    let others = other_admins(&mut tx, &p.sub).await?;
    let u = member(&mut tx, &p.sub).await?;
    if u.disabled_at.is_some() {
        tx.commit().await?;
        return Ok(Json(user_json(&u)));
    }
    if u.sub == t.caller.sub {
        return Err(ApiError::Conflict(
            "cannot_disable_self",
            "you cannot disable your own membership".into(),
        ));
    }
    if held(&u)?.has(Role::TenantAdmin) && others == 0 {
        return Err(last_admin());
    }
    sqlx::query(
        "UPDATE users SET disabled_at = now(), updated_at = now(), updated_by = $2 WHERE sub = $1",
    )
    .bind(&u.sub)
    .bind(&t.caller.sub)
    .execute(&mut *tx)
    .await?;
    entitle_revoking(&mut tx, &u.sub).await?;
    let actor = t.actor();
    audit::record(
        &mut tx,
        &actor,
        Event::new("user.disable", "user", u.sub.clone()),
    )
    .await?;
    tx.commit().await?;
    revoke_after_commit(&st, t.tenant, &u.sub, &actor).await;
    Ok(Json(answer(&st, t.tenant, &u.sub).await?))
}

async fn enable(
    Requires(t, _): TenantAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<SubPath>,
) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    lock_tenant_row(&mut tx, t.tenant).await?;
    let u = member(&mut tx, &p.sub).await?;
    if u.disabled_at.is_none() {
        tx.commit().await?;
        return Ok(Json(user_json(&u)));
    }
    sqlx::query(
        "UPDATE users SET disabled_at = NULL, updated_at = now(), updated_by = $2 WHERE sub = $1",
    )
    .bind(&u.sub)
    .bind(&t.caller.sub)
    .execute(&mut *tx)
    .await?;
    entitle_pending(&mut tx, t.tenant, &u.sub).await?;
    let actor = t.actor();
    audit::record(
        &mut tx,
        &actor,
        Event::new("user.enable", "user", u.sub.clone()),
    )
    .await?;
    tx.commit().await?;
    mint_after_commit(&st, t.tenant, &u.sub, &actor).await;
    Ok(Json(answer(&st, t.tenant, &u.sub).await?))
}

// ---- activity ---------------------------------------------------------------------------------------------------

type ActivityRow = (
    String,
    Option<DateTime<Utc>>,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    String,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
);

async fn activity(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let rows: Vec<ActivityRow> = sqlx::query_as(
        "SELECT u.sub, s.last_seen_at, s.last_surface, coalesce(s.n30, 0), u.added_by, a.display_name, \
                u.origin, u.created_at, u.invited_at \
           FROM users u \
           LEFT JOIN LATERAL ( \
                SELECT max(m.last_seen_at) AS last_seen_at, \
                       (array_agg(m.surface ORDER BY m.last_seen_at DESC))[1] AS last_surface, \
                       count(DISTINCT m.sid) FILTER (WHERE m.last_seen_at > now() - interval '30 days') AS n30 \
                  FROM member_session m WHERE m.sub = u.sub) s ON true \
           LEFT JOIN users a ON a.sub = u.added_by \
          ORDER BY u.sub",
    )
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let lifespan = i64::try_from(st.identity.cfg.invite_email_lifespan_seconds).unwrap_or(i64::MAX);
    let items: Vec<Value> = rows
        .into_iter()
        .map(
            |(sub, seen, surface, n30, added_by, added_by_name, origin, created_at, invited_at)| {
                let invited = invited_at.is_some();
                json!({
                    "sub": sub,
                    "last_seen_at": if invited { None } else { seen },
                    "last_surface": if invited { None } else { surface },
                    "sign_ins_30d": if invited { 0 } else { n30 },
                    "added_by": added_by,
                    "added_by_name": added_by_name,
                    "added_via": origin,
                    "added_at": created_at,
                    "invite_expires_at": invited_at
                        .map(|at| at + chrono::Duration::seconds(lifespan.min(10 * 365 * 86_400))),
                })
            },
        )
        .collect();
    Ok(Json(Value::Array(items)))
}

// ---- rehome -----------------------------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RehomeBody {
    tenant_id: String,
}

/// `agent_user_rehome` refusals: not-found codes are 404, the rest stay 409.
fn rehome_error(e: ApiError) -> ApiError {
    match e {
        ApiError::Conflict(c @ ("user_not_found" | "tenant_not_found"), m) => {
            ApiError::Missing(c, m)
        }
        other => other,
    }
}

/// `SELECT agent_user_rehome(sub, to)` in the open platform transaction: the tenant the user left.
async fn call_rehome(c: &mut PgConnection, sub: &str, to: Uuid) -> ApiResult<Uuid> {
    sqlx::query_scalar("SELECT agent_user_rehome($1, $2)")
        .bind(sub)
        .bind(to)
        .fetch_one(c)
        .await
        .map_err(|e| rehome_error(e.into()))
}

/// The one lock order (B2 T0d, `tenancy::lock_tenant_row`): the `tenants` rows of BOTH tenants before the user row.
/// `agent_user_rehome` locks the user row first, and the user's tenant (`from`) cannot be read before it (RLS, and
/// `agent_auth_user` skips a disabled user). So the function runs once inside a savepoint to learn `from` (and to
/// refuse early), the savepoint is rolled back (releasing its row locks), both tenants rows are locked
/// `FOR KEY SHARE` in id order, and the function runs again. Returns `from`; 409 `conflict` when the user moved
/// in between.
async fn rehome_locked(c: &mut PgConnection, sub: &str, to: Uuid) -> ApiResult<Uuid> {
    sqlx::query("SAVEPOINT rehome_probe")
        .execute(&mut *c)
        .await?;
    let from = call_rehome(&mut *c, sub, to).await?;
    sqlx::query("ROLLBACK TO SAVEPOINT rehome_probe")
        .execute(&mut *c)
        .await?;
    sqlx::query("RELEASE SAVEPOINT rehome_probe")
        .execute(&mut *c)
        .await?;
    // `lock_tenant_row` reads `tenants` as agent_app (RLS `tenant_self`): one tenant context per row.
    leave(&mut *c).await?;
    let mut ids = [from, to];
    ids.sort();
    for t in ids {
        set_tenant(&mut *c, t).await?;
        lock_tenant_row(&mut *c, t).await?;
    }
    enter(&mut *c).await?;
    let moved = call_rehome(&mut *c, sub, to).await?;
    if moved != from {
        return Err(ApiError::Conflict(
            "conflict",
            "the user moved to another tenant meanwhile; try again".into(),
        ));
    }
    Ok(from)
}

async fn rehome(
    SuperAdmin(c): SuperAdmin,
    State(st): State<AppState>,
    ApiPath(p): ApiPath<SubPath>,
    ApiJson(b): ApiJson<RehomeBody>,
) -> ApiResult<Json<Value>> {
    let to = match Uuid::parse_str(b.tenant_id.trim()) {
        Ok(u) if !u.is_nil() => u,
        _ => {
            return Err(ApiError::Unprocessable(
                "invalid_tenant_id",
                "tenant_id must be a tenant uuid".into(),
            ));
        }
    };
    let sub = p.sub;
    let actor = Actor::person(&c);
    let mut tx = begin_platform(&st.db).await?;
    let from = rehome_locked(&mut tx, &sub, to).await?;
    leave(&mut tx).await?;
    // The tenant it left: the member licence there stops.
    set_tenant(&mut tx, from).await?;
    entitle_revoking(&mut tx, &sub).await?;
    audit::record(
        &mut tx,
        &actor,
        Event::new("user.rehome", "user", sub.clone())
            .detail("from", from.to_string())
            .detail("to", to.to_string()),
    )
    .await?;
    // The tenant it joined: enabled as an observer (no roles), a pending entitlement.
    set_tenant(&mut tx, to).await?;
    sqlx::query("UPDATE users SET updated_by = $2 WHERE sub = $1")
        .bind(&sub)
        .bind(&c.sub)
        .execute(&mut *tx)
        .await?;
    entitle_pending(&mut tx, to, &sub).await?;
    audit::record(
        &mut tx,
        &actor,
        Event::new("user.rehome", "user", sub.clone())
            .detail("from", from.to_string())
            .detail("to", to.to_string()),
    )
    .await?;
    let label: String = sqlx::query_scalar("SELECT name FROM tenants WHERE id = $1")
        .bind(to)
        .fetch_one(&mut *tx)
        .await?;
    enter(&mut tx).await?;
    audit::record_platform(
        &mut tx,
        &actor,
        PlatformEvent::new("user.rehome", label)
            .tenant(to)
            .detail("from", from.to_string())
            .detail("user", sub.clone()),
    )
    .await?;
    tx.commit().await?;
    // After COMMIT (D77 L9/L10): revoke the licence it left, then mint the one it joined.
    revoke_after_commit(&st, from, &sub, &actor).await;
    mint_after_commit(&st, to, &sub, &actor).await;
    Ok(Json(answer(&st, to, &sub).await?))
}
