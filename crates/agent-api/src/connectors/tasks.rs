//! The connector backend of the job's durable MCP tasks (crate::mcp_client::store): a task whose row says
//! `origin = 'connector'` lives on a tenant's own external MCP server (the template the instance pins declares the op
//! in `task_tools`), so every get / answer / cancel must reach THAT server with THAT instance's credential. A
//! `platform` task goes to the platform MCP server instead ([`crate::mcp_client::client::HttpMcp`]).
//!
//! The call context is read per task in one short tenant-scoped transaction (the instance, its pinned template and
//! the stored credential), closed before the upstream call. A licence-bearer template has no stored credential: the
//! licence is minted per call, exactly as the executor does for a job's own call (crate::connectors::runtime).
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value};
use sqlx::PgPool;

use super::custody;
use super::instances;
use super::transport::{AuthSpec, CallTarget, Transport};
use super::wire::{AuthKind, CallSecrets, OpError};
use crate::crypto::keyring::Keyring;
use crate::error::ApiError;
use crate::mcp_client::ToolError;
use crate::mcp_client::client::HttpMcp;
use crate::mcp_client::tasks::{StaffActor, TaskError, TaskRef, TaskSnapshot, actor_meta};
use crate::tenancy;
use crate::tenants::app_licence::{App, BundleMinter};

/// Each task operation's own bound (there is no op row to take a timeout from).
const TASK_TIMEOUT: Duration = Duration::from_secs(30);

/// The connector side of a task: the task's instance and pinned template, read per call.
pub struct ConnectorTasks {
    db: PgPool,
    connectors: Transport,
    keyring: Option<Arc<Keyring>>,
    minter: Arc<BundleMinter>,
}

impl ConnectorTasks {
    pub fn new(
        db: PgPool,
        connectors: Transport,
        keyring: Option<Arc<Keyring>>,
        minter: Arc<BundleMinter>,
    ) -> Self {
        Self {
            db,
            connectors,
            keyring,
            minter,
        }
    }

    /// The call context of the task's connector system, in one short tenant-scoped transaction; a licence bearer is
    /// minted after it closes (the licence service is remote).
    async fn context(
        &self,
        t: &TaskRef<'_>,
    ) -> Result<(CallTarget, AuthSpec, CallSecrets), TaskError> {
        // A deleted system can never serve its task again: `lost` ends the row instead of a fatal `Tool` the poller
        // would retry forever.
        let missing =
            || TaskError::Lost("the task's connector system no longer exists".to_string());
        let instance = t.instance.ok_or_else(missing)?;
        let mut tx = tenancy::begin_scoped(&self.db, t.tenant)
            .await
            .map_err(db_error)?;
        let inst = instances::one(&mut tx, instance)
            .await
            .map_err(db_error)?
            .ok_or_else(missing)?;
        let tpl = instances::pinned_template(&mut tx, &inst)
            .await
            .map_err(db_error)?;
        let mut secret = if tpl.auth_kind.needs_stored_credential() {
            custody::open_credential(&mut tx, self.keyring.as_deref(), t.tenant, instance)
                .await
                .map_err(db_error)?
        } else {
            None
        };
        tx.commit().await.map_err(|e| db_error(ApiError::from(e)))?;
        if tpl.auth_kind == AuthKind::LicenceBearer {
            let minted = self
                .minter
                .mint(t.tenant, App::Conveyor)
                .await
                .map_err(|e| {
                    tracing::warn!(error = ?e, "the conveyor licence cannot be minted");
                    TaskError::Tool(ToolError::retryable(
                        "the conveyor licence cannot be minted right now",
                    ))
                })?;
            secret.get_or_insert_with(Map::new).insert(
                "licence_token".to_owned(),
                Value::String(minted.token.expose().to_owned()),
            );
        }
        let target = CallTarget {
            tenant: t.tenant,
            instance_id: inst.id.clone(),
            env: if inst.env == "test" { "test" } else { "prod" },
            credential_version: inst.cred_version.unwrap_or(0),
            base_url: inst.base_url.clone(),
            stub: inst.stub,
        };
        let auth = AuthSpec {
            kind: tpl.auth_kind,
            params: tpl.auth_params,
        };
        let secrets = CallSecrets {
            config: inst.env_map(),
            secret: secret.unwrap_or_default(),
        };
        Ok((target, auth, secrets))
    }

    pub async fn get(&self, t: &TaskRef<'_>) -> Result<TaskSnapshot, TaskError> {
        let (target, auth, secrets) = self.context(t).await?;
        let kind = auth.kind;
        self.connectors
            .external_mcp()
            .get_task(
                &self.connectors,
                &target,
                &auth,
                &secrets,
                TASK_TIMEOUT,
                t.server_task_id,
            )
            .await
            .map_err(|e| self.call_error(t, kind, e))
    }

    pub async fn answer(
        &self,
        t: &TaskRef<'_>,
        actor: &StaffActor,
        key: &str,
        response: Value,
    ) -> Result<(), TaskError> {
        let (target, auth, secrets) = self.context(t).await?;
        let kind = auth.kind;
        let done = self
            .connectors
            .external_mcp()
            .answer_task(
                &self.connectors,
                &target,
                &auth,
                &secrets,
                TASK_TIMEOUT,
                t.server_task_id,
                key,
                response,
                Some(actor_meta(actor)),
            )
            .await;
        done.map_err(|e| self.call_error(t, kind, e))
    }

    pub async fn cancel(&self, t: &TaskRef<'_>) -> Result<(), TaskError> {
        let (target, auth, secrets) = self.context(t).await?;
        let kind = auth.kind;
        self.connectors
            .external_mcp()
            .cancel_task(
                &self.connectors,
                &target,
                &auth,
                &secrets,
                TASK_TIMEOUT,
                t.server_task_id,
            )
            .await
            .map_err(|e| self.call_error(t, kind, e))
    }

    /// A connector call's refusal as the task layer's error. A licence-bearer system's token is minted per call and
    /// cached by the minter, so one the server refused is dropped from that cache: the next poll mints a fresh token
    /// instead of replaying the revoked one until its TTL.
    fn call_error(&self, t: &TaskRef<'_>, kind: AuthKind, e: OpError) -> TaskError {
        if e.code == "connector_auth_failed" && kind == AuthKind::LicenceBearer {
            self.minter.invalidate(t.tenant, App::Conveyor);
        }
        op_error(e)
    }
}

/// A stored-read or commit failure: a missing/refused row is permanent, everything else is worth another poll.
fn db_error(e: ApiError) -> TaskError {
    let permanent = matches!(
        &e,
        ApiError::Missing(..) | ApiError::Conflict(..) | ApiError::Unprocessable(..)
    );
    let message = e.to_string();
    TaskError::Tool(if permanent {
        ToolError::fatal(message)
    } else {
        ToolError::retryable(message)
    })
}

/// A connector call's refusal as the task layer's error.
fn op_error(e: OpError) -> TaskError {
    match e.code {
        "task_not_found" => TaskError::Lost(e.detail),
        "connector_misconfigured" | "invalid_target" | "invalid_url" => {
            TaskError::Tool(ToolError::fatal(e.detail))
        }
        _ => TaskError::Tool(ToolError::retryable(e.detail)),
    }
}

/// The task backends by a row's `origin`: the platform MCP server (when configured) and the task's own connector
/// system.
#[derive(Clone)]
pub struct TaskClients {
    pub platform: Option<Arc<HttpMcp>>,
    pub connector: Arc<ConnectorTasks>,
}

impl TaskClients {
    pub async fn get(&self, t: &TaskRef<'_>) -> Result<TaskSnapshot, TaskError> {
        match t.origin {
            "platform" => match &self.platform {
                Some(mcp) => mcp.get_task(t.tenant, t.server_task_id).await,
                None => Err(TaskError::Tool(ToolError::fatal(
                    "the platform MCP server is not configured",
                ))),
            },
            "connector" => self.connector.get(t).await,
            other => Err(TaskError::Tool(ToolError::fatal(format!(
                "the task's origin {other:?} is not known"
            )))),
        }
    }

    pub async fn answer(
        &self,
        t: &TaskRef<'_>,
        actor: &StaffActor,
        key: &str,
        response: Value,
    ) -> Result<(), TaskError> {
        match t.origin {
            "platform" => match &self.platform {
                Some(mcp) => {
                    mcp.answer_task(actor, t.server_task_id, key, response)
                        .await
                }
                None => Err(TaskError::Tool(ToolError::fatal(
                    "the platform MCP server is not configured",
                ))),
            },
            "connector" => self.connector.answer(t, actor, key, response).await,
            other => Err(TaskError::Tool(ToolError::fatal(format!(
                "the task's origin {other:?} is not known"
            )))),
        }
    }

    pub async fn cancel(&self, t: &TaskRef<'_>) -> Result<(), TaskError> {
        match t.origin {
            "platform" => match &self.platform {
                Some(mcp) => mcp.cancel_task(t.tenant, t.server_task_id).await,
                None => Err(TaskError::Tool(ToolError::fatal(
                    "the platform MCP server is not configured",
                ))),
            },
            "connector" => self.connector.cancel(t).await,
            other => Err(TaskError::Tool(ToolError::fatal(format!(
                "the task's origin {other:?} is not known"
            )))),
        }
    }
}
