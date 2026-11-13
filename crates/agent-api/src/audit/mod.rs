//! The audit helper every write calls (B1 X3; the contract B2–B8 consume). Two logs:
//!
//! - the tenant log `audit_event` (append-only, under RLS): [`record`] in the caller's scoped transaction, the row
//!   landing in the transaction's `app.current_tenant`; [`record_alone`] in its own transaction, for a failure
//!   whose write transaction rolled back;
//! - the platform log `platform_audit`: [`record_platform`] between `platform_db::enter` and `leave` (or in
//!   `begin_platform`), [`record_platform_alone`] in its own platform transaction.
//!
//! Routing (spec 04 §9b): tenant lifecycle (`tenant.create|import|suspend|resume|offboard`, `user.rehome`) writes
//! both logs in one mixed-role transaction; pre-tenant failures and `signup.*` the platform log only; everything
//! else the tenant log only.
//!
//! Fail loud (`ApiError::Internal`): an action not in [`vocab`], details that are not an object, larger than
//! 8 KiB or carrying a secret, a token, a password or an email address ([`scrub`]), a missing tenant GUC, or a
//! platform write outside the platform role. Details carry ids and codes; an email is recorded as
//! `email_domain`. Each write counts `agent_audit_writes_total{log,status}`; each refusal
//! `agent_audit_write_errors_total{log}`.
pub mod read;
pub mod scrub;
pub mod vocab;

use serde_json::{Map, Value};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::auth::Caller;
use crate::error::{ApiError, ApiResult};
use crate::platform_db::begin_platform;
use crate::tenancy::{TenantCtx, begin_scoped};

/// Details larger than this (serialized) are refused.
pub const MAX_DETAILS_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActorType {
    Human,
    AiAgent,
    System,
}

impl ActorType {
    pub fn as_str(self) -> &'static str {
        match self {
            ActorType::Human => "human",
            ActorType::AiAgent => "ai_agent",
            ActorType::System => "system",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Success,
    Failure,
    Denied,
    Error,
    Fallback,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Success => "success",
            Status::Failure => "failure",
            Status::Denied => "denied",
            Status::Error => "error",
            Status::Fallback => "fallback",
        }
    }
}

/// Who did it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Actor {
    pub id: String,
    pub kind: ActorType,
    /// A super_admin acting in a tenant that is not their home (the UI marks the row "platform").
    pub platform: bool,
    /// The role keys of a live view-as (D79): every tenant row this actor writes carries `details.view_as`.
    pub view_as: Option<Vec<&'static str>>,
}

impl Actor {
    /// The caller of a tenant-scoped request: id = sub, human, platform = the context's `platform_actor`.
    /// Under a live view-as the rows it writes carry `details.view_as`.
    pub fn of(ctx: &TenantCtx) -> Self {
        Self {
            view_as: ctx.view_as.map(|v| v.roles.keys()),
            ..Self::member(&ctx.caller.sub, ctx.platform_actor)
        }
    }

    /// A person on a platform route (super_admin): id = sub, human, platform = true.
    pub fn person(c: &Caller) -> Self {
        Self {
            id: c.sub.clone(),
            kind: ActorType::Human,
            platform: true,
            view_as: None,
        }
    }

    /// A person acting in their own tenant (or a super_admin, `platform`).
    pub fn member(sub: &str, platform: bool) -> Self {
        Self {
            id: sub.to_string(),
            kind: ActorType::Human,
            platform,
            view_as: None,
        }
    }

    /// This service, e.g. `service:agent-api-drainer`, `service:agent-api-signup-tick`.
    pub fn system(client: &'static str) -> Self {
        Self {
            id: client.to_string(),
            kind: ActorType::System,
            platform: false,
            view_as: None,
        }
    }

    /// An AI agent (B4 runtime): id = the agent role key.
    pub fn agent(role: &str) -> Self {
        Self {
            id: role.to_string(),
            kind: ActorType::AiAgent,
            platform: false,
            view_as: None,
        }
    }
}

/// One tenant-log row, built then [`record`]ed.
#[must_use]
#[derive(Clone, Debug)]
pub struct Event {
    action: &'static str,
    resource_type: &'static str,
    resource_id: String,
    job: Option<String>,
    status: Status,
    details: Map<String, Value>,
    once: bool,
}

impl Event {
    /// Status `success`, no job, no details.
    pub fn new(
        action: &'static str,
        resource_type: &'static str,
        resource_id: impl Into<String>,
    ) -> Self {
        Self {
            action,
            resource_type,
            resource_id: resource_id.into(),
            job: None,
            status: Status::Success,
            details: Map::new(),
            once: false,
        }
    }

    /// The job (its key) the event is about.
    pub fn job(mut self, key: &str) -> Self {
        self.job = Some(key.to_string());
        self
    }

    pub fn status(mut self, s: Status) -> Self {
        self.status = s;
        self
    }

    /// `failure` with `details.code = code`.
    pub fn failed(mut self, code: &'static str) -> Self {
        self.status = Status::Failure;
        self.details.insert("code".into(), Value::from(code));
        self
    }

    pub fn detail(mut self, key: &'static str, v: impl Into<Value>) -> Self {
        self.details.insert(key.into(), v.into());
        self
    }

    /// At most one row per request id (B4 `execute` rows): a second record of the same request is a no-op that
    /// returns the first row's id.
    pub fn once_per_request(mut self) -> Self {
        self.once = true;
        self
    }
}

/// One platform-log row, built then [`record_platform`]ed.
#[must_use]
#[derive(Clone, Debug)]
pub struct PlatformEvent {
    action: &'static str,
    subject_label: String,
    subject_tenant: Option<Uuid>,
    failed: bool,
    details: Map<String, Value>,
}

impl PlatformEvent {
    /// Status `ok`.
    pub fn new(action: &'static str, subject_label: impl Into<String>) -> Self {
        Self {
            action,
            subject_label: subject_label.into(),
            subject_tenant: None,
            failed: false,
            details: Map::new(),
        }
    }

    pub fn tenant(mut self, id: Uuid) -> Self {
        self.subject_tenant = Some(id);
        self
    }

    /// `failed` with `details.code = code`.
    pub fn failed(mut self, code: &'static str) -> Self {
        self.failed = true;
        self.details.insert("code".into(), Value::from(code));
        self
    }

    pub fn detail(mut self, key: &'static str, v: impl Into<Value>) -> Self {
        self.details.insert(key.into(), v.into());
        self
    }
}

fn refuse(what: String) -> ApiError {
    ApiError::Internal(anyhow::anyhow!("audit refused: {what}"))
}

/// The checks both logs share: action listed, details an object of at most 8 KiB with nothing secret-shaped.
fn checked(action: &str, vocab: &[&str], details: Map<String, Value>) -> ApiResult<Value> {
    if !vocab.contains(&action) {
        return Err(refuse(format!("action {action:?} is not in audit::vocab")));
    }
    let details = Value::Object(details);
    if let Err(field) = scrub::check(&details) {
        return Err(refuse(format!(
            "details field {field:?} of {action:?} looks like a secret or a person"
        )));
    }
    let size = serde_json::to_vec(&details)
        .map(|b| b.len())
        .unwrap_or(usize::MAX);
    if size > MAX_DETAILS_BYTES {
        return Err(refuse(format!("details of {action:?} are {size} bytes")));
    }
    Ok(details)
}

fn counted<T>(log: &'static str, status: &'static str, r: ApiResult<T>) -> ApiResult<T> {
    match &r {
        Ok(_) => metrics::counter!("agent_audit_writes_total", "log" => log, "status" => status)
            .increment(1),
        Err(e) => {
            metrics::counter!("agent_audit_write_errors_total", "log" => log).increment(1);
            tracing::error!(log, error = %e, "audit_write_failed");
        }
    }
    r
}

const INSERT_ROW: &str = "INSERT INTO audit_event \
        (actor_id, actor_type, platform_actor, action, resource_type, resource_id, job, status, details, request_id) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING id";

const INSERT_ONCE: &str = "INSERT INTO audit_event \
        (actor_id, actor_type, platform_actor, action, resource_type, resource_id, job, status, details, request_id) \
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
     ON CONFLICT (tenant_id, request_id) WHERE action = 'execute' DO NOTHING RETURNING id";

async fn insert_tenant_row(conn: &mut PgConnection, actor: &Actor, mut e: Event) -> ApiResult<i64> {
    if let Some(v) = &actor.view_as {
        e.details
            .entry("view_as")
            .or_insert_with(|| Value::from(v.clone()));
    }
    let details = checked(e.action, vocab::TENANT, e.details)?;
    let scoped: bool = sqlx::query_scalar("SELECT app_current_tenant() IS NOT NULL")
        .fetch_one(&mut *conn)
        .await?;
    if !scoped {
        return Err(refuse(
            "no app.current_tenant in this transaction (use begin_scoped)".into(),
        ));
    }
    let request_id = crate::observability::request_id();
    // The dedupe index covers `execute` rows only.
    let once = e.once && e.action == "execute" && request_id.is_some();
    let inserted: Option<i64> = sqlx::query_scalar(if once { INSERT_ONCE } else { INSERT_ROW })
        .bind(&actor.id)
        .bind(actor.kind.as_str())
        .bind(actor.platform)
        .bind(e.action)
        .bind(e.resource_type)
        .bind(&e.resource_id)
        .bind(&e.job)
        .bind(e.status.as_str())
        .bind(&details)
        .bind(&request_id)
        .fetch_optional(&mut *conn)
        .await?;
    match inserted {
        Some(id) => Ok(id),
        // Only an `ON CONFLICT DO NOTHING` can return no row: the request's row exists.
        None => Ok(sqlx::query_scalar(
            "SELECT id FROM audit_event \
              WHERE action = 'execute' AND request_id = $1 AND tenant_id = app_current_tenant()",
        )
        .bind(&request_id)
        .fetch_one(&mut *conn)
        .await?),
    }
}

/// Record `e` in the caller's scoped transaction (the tenant is the transaction's `app.current_tenant`).
pub async fn record(conn: &mut PgConnection, actor: &Actor, e: Event) -> ApiResult<i64> {
    let status = e.status.as_str();
    counted("tenant", status, insert_tenant_row(conn, actor, e).await)
}

/// Record `e` in its own `begin_scoped(tenant)` transaction: for a failure whose write transaction rolled back.
pub async fn record_alone(db: &PgPool, tenant: Uuid, actor: &Actor, e: Event) -> ApiResult<i64> {
    let mut tx = begin_scoped(db, tenant).await?;
    let id = record(&mut tx, actor, e).await?;
    tx.commit().await?;
    Ok(id)
}

async fn insert_platform_row(
    conn: &mut PgConnection,
    actor: &Actor,
    e: PlatformEvent,
) -> ApiResult<i64> {
    let details = checked(e.action, vocab::PLATFORM, e.details)?;
    let actor_type = match actor.kind {
        ActorType::Human => "user",
        ActorType::AiAgent | ActorType::System => "system",
    };
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO platform_audit \
            (actor_id, actor_type, action, subject_tenant, subject_label, status, details, request_id) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) RETURNING id",
    )
    .bind(&actor.id)
    .bind(actor_type)
    .bind(e.action)
    .bind(e.subject_tenant)
    .bind(&e.subject_label)
    .bind(if e.failed { "failed" } else { "ok" })
    .bind(&details)
    .bind(crate::observability::request_id())
    .fetch_one(&mut *conn)
    .await
    .map_err(|err| refuse(format!("platform audit write ({err}); is the transaction agent_platform?")))?;
    Ok(id)
}

/// Record `e` in the platform log. Must run as `agent_platform` (`begin_platform`, or between
/// `platform_db::enter` and `leave`); as `agent_app` it is refused (no grant).
pub async fn record_platform(
    conn: &mut PgConnection,
    actor: &Actor,
    e: PlatformEvent,
) -> ApiResult<i64> {
    let status = if e.failed { "failed" } else { "ok" };
    counted(
        "platform",
        status,
        insert_platform_row(conn, actor, e).await,
    )
}

/// Record `e` in its own platform transaction.
pub async fn record_platform_alone(db: &PgPool, actor: &Actor, e: PlatformEvent) -> ApiResult<i64> {
    let mut tx = begin_platform(db).await?;
    let id = record_platform(&mut tx, actor, e).await?;
    tx.commit().await?;
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builder_and_checks() {
        let e = Event::new("user.add", "user", "u-1")
            .job("faq_agent")
            .failed("user_exists")
            .detail("email_domain", "b.eu");
        assert_eq!(e.status, Status::Failure);
        assert_eq!(e.details["code"], json!("user_exists"));
        assert!(checked(e.action, vocab::TENANT, e.details.clone()).is_ok());
        assert!(checked("nope.nope", vocab::TENANT, Map::new()).is_err());
        let mut big = Map::new();
        big.insert("ids".into(), json!(vec!["x".repeat(100); 100]));
        assert!(checked("user.add", vocab::TENANT, big).is_err());
        assert_eq!(ActorType::AiAgent.as_str(), "ai_agent");
    }
}
