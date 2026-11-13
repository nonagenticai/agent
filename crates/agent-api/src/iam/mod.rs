//! Tenant IAM procedures (A3) and per-job declarations (A2): the store behind `iam::routes`.
//!
//! - `iam_procedure` (0232): tenant-wide, named (1..80, unique per tenant), `steps` = [`IamStep`]s that produce state
//!   keys. A job attaches one through the design ledger (kind `iam_attachment`, working rows `job_iam_attachment`).
//! - `job_tool_policy` (0232): the draft rows of ledger kind `tool_policy`, one per op key.
//! - [`job_index`]: the producer index of a job's draft (its policies plus attached procedures), optionally with one
//!   policy replaced, for the write-time cycle/depth check.
//!
//! Tenant-wide procedures cannot be checked against one job's catalogue: [`known_shape`] accepts any well-formed op
//! key; attaching to a job checks every step op against that job's catalogue (`iam::routes`).
pub mod routes;

use serde_json::{Value, json};
use sqlx::PgConnection;
use uuid::Uuid;

use crate::audit;
use crate::connectors::opkey::{op_key, parse_op_key};
use crate::conversations::iso;
use crate::error::{ApiError, ApiResult};
use crate::runtime::declared::{
    ChainError, DeclError, IamProcedure, IamStep, IdentityStrength, ProducerIndex, RESERVED_KEYS,
    ToolPolicy, plan_chain,
};

/// A stored procedure with its bookkeeping columns.
#[derive(Debug, sqlx::FromRow)]
pub struct ProcedureRow {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub strength: String,
    pub steps: Value,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub updated_by: String,
}

impl ProcedureRow {
    /// The wire shape; `jobs` = how many jobs attach it (`None` when not asked).
    pub fn json(&self, jobs: Option<i64>) -> Value {
        let mut v = json!({
            "id": self.id,
            "name": self.name,
            "description": self.description,
            "strength": self.strength,
            "steps": self.steps,
            "created_at": iso(self.created_at),
            "updated_at": iso(self.updated_at),
            "updated_by": self.updated_by,
        });
        if let Some(n) = jobs {
            v["jobs"] = json!(n);
        }
        v
    }

    /// The typed procedure (a stored row always decodes: it was validated on write).
    pub fn procedure(&self) -> ApiResult<IamProcedure> {
        let steps: Vec<IamStep> = serde_json::from_value(self.steps.clone()).map_err(|e| {
            ApiError::Internal(anyhow::anyhow!(
                "iam_procedure {} has unreadable steps: {e}",
                self.id
            ))
        })?;
        Ok(IamProcedure {
            id: self.id,
            name: self.name.clone(),
            description: self.description.clone(),
            strength: strength_of(&self.strength),
            steps,
        })
    }
}

/// The stored `strength` text as its type; an unknown value reads as `identified` (the column's CHECK allows only
/// the two, so this is a drift guard, never a silent third level).
fn strength_of(text: &str) -> IdentityStrength {
    if text == "verified" {
        IdentityStrength::Verified
    } else {
        IdentityStrength::Identified
    }
}

pub fn not_found() -> ApiError {
    ApiError::Missing(
        "procedure_not_found",
        "That identity procedure does not exist.".into(),
    )
}

/// A [`DeclError`] as its 422.
pub fn refusal(e: DeclError) -> ApiError {
    ApiError::Unprocessable(e.code, e.detail)
}

/// A [`ChainError`] as its 422 (`cycle`, `depth_exceeded`, `no_producer`).
pub fn chain_refusal(e: &ChainError) -> ApiError {
    let detail = match e {
        ChainError::Cycle { op } => format!("`{op}` would need its own output to run."),
        ChainError::DepthExceeded => format!(
            "A chain of more than {} dependent tools cannot be resolved.",
            crate::runtime::declared::MAX_DEPTH
        ),
        ChainError::NoProducer { key } => format!("Nothing produces `{key}`."),
    };
    ApiError::Unprocessable(e.code(), detail)
}

/// Is `raw` a well-formed op key (`object.verb` / `object.verb{variant}`)?
pub fn known_shape(raw: &str) -> bool {
    !parse_op_key(raw).object.is_empty()
}

/// The canonical form of an op key, `None` when malformed.
pub fn canonical_op(raw: &str) -> Option<String> {
    let o = parse_op_key(raw);
    (!o.object.is_empty()).then(|| op_key(&o))
}

pub async fn list(c: &mut PgConnection) -> ApiResult<Vec<(ProcedureRow, i64)>> {
    let rows: Vec<ProcedureRow> =
        sqlx::query_as("SELECT id, name, description, strength, steps, created_at, updated_at, updated_by FROM iam_procedure ORDER BY name")
            .fetch_all(&mut *c)
            .await?;
    let counts: Vec<(Uuid, i64)> = sqlx::query_as(
        "SELECT procedure_id, count(*) FROM job_iam_attachment GROUP BY procedure_id",
    )
    .fetch_all(&mut *c)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let n = counts
                .iter()
                .find(|(id, _)| *id == r.id)
                .map_or(0, |(_, n)| *n);
            (r, n)
        })
        .collect())
}

pub async fn get(c: &mut PgConnection, id: Uuid) -> ApiResult<ProcedureRow> {
    sqlx::query_as("SELECT id, name, description, strength, steps, created_at, updated_at, updated_by FROM iam_procedure WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(not_found)
}

pub async fn attached_jobs(c: &mut PgConnection, id: Uuid) -> ApiResult<i64> {
    Ok(
        sqlx::query_scalar("SELECT count(*) FROM job_iam_attachment WHERE procedure_id = $1")
            .bind(id)
            .fetch_one(&mut *c)
            .await?,
    )
}

fn name_taken(name: &str) -> ApiError {
    ApiError::Conflict(
        "name_taken",
        format!("An identity procedure named `{name}` already exists."),
    )
}

pub async fn create(
    c: &mut PgConnection,
    name: &str,
    description: &str,
    strength: IdentityStrength,
    steps: &[IamStep],
    by: &str,
) -> ApiResult<ProcedureRow> {
    let taken: Option<i32> = sqlx::query_scalar("SELECT 1 FROM iam_procedure WHERE name = $1")
        .bind(name)
        .fetch_optional(&mut *c)
        .await?;
    if taken.is_some() {
        return Err(name_taken(name));
    }
    Ok(sqlx::query_as(
        "INSERT INTO iam_procedure (name, description, strength, steps, updated_by) \
         VALUES ($1, $2, $3, $4, $5) \
         RETURNING id, name, description, strength, steps, created_at, updated_at, updated_by",
    )
    .bind(name)
    .bind(description)
    .bind(strength.as_str())
    .bind(json!(steps))
    .bind(by)
    .fetch_one(&mut *c)
    .await?)
}

pub async fn update(
    c: &mut PgConnection,
    id: Uuid,
    name: &str,
    description: &str,
    strength: IdentityStrength,
    steps: &[IamStep],
    by: &str,
) -> ApiResult<ProcedureRow> {
    let taken: Option<i32> =
        sqlx::query_scalar("SELECT 1 FROM iam_procedure WHERE name = $1 AND id <> $2")
            .bind(name)
            .bind(id)
            .fetch_optional(&mut *c)
            .await?;
    if taken.is_some() {
        return Err(name_taken(name));
    }
    sqlx::query_as("UPDATE iam_procedure SET name = $2, description = $3, strength = $4, steps = $5, \
                updated_by = $6, updated_at = clock_timestamp() WHERE id = $1 \
                RETURNING id, name, description, strength, steps, created_at, updated_at, updated_by")
    .bind(id)
    .bind(name)
    .bind(description)
    .bind(strength.as_str())
    .bind(json!(steps))
    .bind(by)
    .fetch_optional(&mut *c)
    .await?
    .ok_or_else(not_found)
}

pub async fn delete(c: &mut PgConnection, id: Uuid) -> ApiResult<()> {
    let n = attached_jobs(c, id).await?;
    if n > 0 {
        return Err(ApiError::Conflict(
            "procedure_attached",
            format!("This procedure is attached to {n} job(s); detach it first."),
        ));
    }
    let r = sqlx::query("DELETE FROM iam_procedure WHERE id = $1")
        .bind(id)
        .execute(&mut *c)
        .await?;
    if r.rows_affected() == 0 {
        return Err(not_found());
    }
    Ok(())
}

/// The draft policies of a job, by op key.
pub async fn job_policies(c: &mut PgConnection, job: Uuid) -> ApiResult<Vec<(String, ToolPolicy)>> {
    let rows: Vec<(String, Value)> =
        sqlx::query_as("SELECT op, policy FROM job_tool_policy WHERE job_id = $1 ORDER BY op")
            .bind(job)
            .fetch_all(&mut *c)
            .await?;
    rows.into_iter()
        .map(|(op, v)| {
            serde_json::from_value(v)
                .map(|p| (op.clone(), p))
                .map_err(|e| {
                    ApiError::Internal(anyhow::anyhow!(
                        "job_tool_policy {op} has another shape: {e}"
                    ))
                })
        })
        .collect()
}

/// The procedures a job (draft) has attached.
pub async fn attached(c: &mut PgConnection, job: Uuid) -> ApiResult<Vec<ProcedureRow>> {
    Ok(sqlx::query_as("SELECT id, name, description, strength, steps, created_at, updated_at, updated_by FROM iam_procedure WHERE id IN \
         (SELECT procedure_id FROM job_iam_attachment WHERE job_id = $1) ORDER BY name")
    .bind(job)
    .fetch_all(&mut *c)
    .await?)
}

/// The producer index of a job's draft; `replacing` swaps (or adds) one op's policy, `procedure` adds one more
/// procedure (a candidate attachment).
pub async fn job_index(
    c: &mut PgConnection,
    job: Uuid,
    replacing: Option<(&str, &ToolPolicy)>,
    extra: Option<&IamProcedure>,
) -> ApiResult<ProducerIndex> {
    let mut policies = job_policies(c, job).await?;
    if let Some((op, p)) = replacing {
        policies.retain(|(o, _)| o != op);
        policies.push((op.to_owned(), p.clone()));
    }
    let mut procs = Vec::new();
    for r in attached(c, job).await? {
        procs.push(r.procedure()?);
    }
    if let Some(p) = extra
        && !procs.iter().any(|x| x.id == p.id)
    {
        procs.push(p.clone());
    }
    Ok(ProducerIndex::build(&policies, &procs))
}

// ---- the identity a procedure establishes (owner 2026-11-01) ---------------------------------------------------------

/// The identity keys one call produced (absent = this call did not produce it; state keeps the existing value).
#[derive(Clone, Copy, Debug, Default)]
pub struct IdentityWrite<'a> {
    pub subject: Option<&'a str>,
    pub email: Option<&'a str>,
    pub name: Option<&'a str>,
}

/// One identity upgrade: the conversation and what the procedure established on it.
#[derive(Clone, Copy, Debug)]
pub struct IdentityUpgrade<'a> {
    pub conversation: &'a str,
    pub write: IdentityWrite<'a>,
    pub procedure: Uuid,
    pub strength: IdentityStrength,
    /// The step's op key (the audit detail).
    pub step: &'a str,
    /// The role the runtime ran the call as (`runtime` for a resolve step).
    pub actor_role: &'a str,
    pub job_key: &'a str,
}

/// A procedure wrote a different person than the conversation already is: nothing is written; the call fails and
/// the agent hands over instead of proceeding as the wrong person.
pub const IDENTITY_CONFLICT: &str = "identity_conflict";
/// The identity keys do not name one person (a step wrote `email`/`name` and no subject was produced before).
pub const IDENTITY_INCOMPLETE: &str = "identity_incomplete";
/// The conversation state could not be read or written.
pub const IDENTITY_UNAVAILABLE: &str = "state_unavailable";

/// Write the identity an IAM procedure established, in the caller's transaction (the runner's declared-output
/// transaction): conversation state `subject`/`email`/`name` with `identity_source = iam`, `identity_strength`, the
/// web chat session row (when the conversation has one) and one audit row. Never a value
/// in a log line, an audit detail or a metric: the audit carries the procedure, the strength, the step and the
/// previous source only.
///
/// One conversation is one person (owner 2026-11-01): a produced subject that differs from the conversation's
/// existing one is [`IDENTITY_CONFLICT`] and nothing changes. The same subject refreshes email/name. The strength
/// never goes down: a verified conversation refreshed by an identified procedure stays verified.
pub async fn upgrade_identity(
    conn: &mut PgConnection,
    upgrade: IdentityUpgrade<'_>,
) -> Result<(), &'static str> {
    use crate::conversations::state::{self, StateValue};

    let IdentityUpgrade {
        conversation,
        write,
        procedure,
        strength,
        step,
        actor_role,
        job_key,
    } = upgrade;
    let state = state::load(conn, conversation)
        .await
        .map_err(|_| IDENTITY_UNAVAILABLE)?;
    let text = |key: &str| {
        state
            .get(key)
            .and_then(StateValue::as_value)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let current = text(state::SUBJECT);
    let produced = write
        .subject
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    if let (Some(cur), Some(new)) = (current.as_deref(), produced.as_deref())
        && cur != new
    {
        return Err(IDENTITY_CONFLICT);
    }
    let subject = produced.or(current).ok_or(IDENTITY_INCOMPLETE)?;
    // the email the conversation now knows: the one this call produced, else the one the chain already stored (the
    // procedure may have asked for it in another step)
    let email = write
        .email
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(str::to_owned)
        .or_else(|| text(state::EMAIL));
    let previous_source = text(state::IDENTITY_SOURCE);
    // the strength of the conversation: verified stays verified whatever refreshes it
    let previous_strength = text(state::IDENTITY_STRENGTH).map(|s| strength_of(&s));
    let strength = match previous_strength {
        Some(prev) if prev.level() > strength.level() => prev,
        _ => strength,
    };
    let by: String = format!("iam:{procedure}/{step}")
        .chars()
        .take(200)
        .collect();
    state::put_known_identity(
        conn,
        conversation,
        state::SOURCE_IAM,
        &subject,
        email.as_deref(),
        write.name,
        &by,
    )
    .await
    .map_err(|_| IDENTITY_UNAVAILABLE)?;
    state::put_value(
        conn,
        conversation,
        state::IDENTITY_STRENGTH,
        strength.as_str(),
        &by,
        None,
    )
    .await
    .map_err(|_| IDENTITY_UNAVAILABLE)?;
    // a web chat session keeps the identity over a reload: the session row is the authority (0183). A different
    // ref is never overwritten.
    sqlx::query(
        "UPDATE web_chat_session \
            SET customer_ref = $2, \
                customer_email = COALESCE($3, customer_email), \
                identified_at = COALESCE(identified_at, clock_timestamp()) \
          WHERE conversation_id = $1 AND (customer_ref IS NULL OR customer_ref = $2)",
    )
    .bind(conversation)
    .bind(&subject)
    .bind(email.as_deref())
    .execute(&mut *conn)
    .await
    .map_err(|_| IDENTITY_UNAVAILABLE)?;
    audit::record(
        conn,
        &crate::audit::Actor::agent(actor_role),
        crate::audit::Event::new(
            "conversation.identity_upgraded",
            "conversation",
            conversation,
        )
        .job(job_key)
        .detail("procedure", procedure.to_string())
        .detail("strength", strength.as_str())
        .detail("step", step.to_owned())
        .detail(
            "previous_source",
            previous_source.unwrap_or_else(|| "none".to_owned()),
        ),
    )
    .await
    .map_err(|_| IDENTITY_UNAVAILABLE)?;
    Ok(())
}

/// Refuse a producer cycle or an over-deep chain: plan every produced key with the channel keys taken as present.
/// A missing producer is not an error here (a key may be written by the channel at runtime).
pub fn check_chains(idx: &ProducerIndex, keys: &[String]) -> ApiResult<()> {
    let present = |k: &str| RESERVED_KEYS.contains(&k);
    for k in keys {
        let planned = plan_chain(&[k.as_str()], &present, idx);
        if let Some(e) = planned
            .err()
            .filter(|e| !matches!(e, ChainError::NoProducer { .. }))
        {
            return Err(chain_refusal(&e));
        }
    }
    Ok(())
}
