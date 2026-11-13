//! The "release approved" hook (spec B2 §2.7, owner 2026-10-20, F-06): code another plan runs when a person approves
//! a release (B8 `validation::grid::OnApprove`: the grid run is triggered only by a user's approve, never by a
//! schedule).
//!
//! - Hooks are registered on `AppState.release_approved` ([`ApprovedHooks::register_approved`]).
//! - Approve runs every registered hook AFTER its transaction committed, on a spawned task, never inside the
//!   transaction: the hook sees the committed release.
//! - A hook's result never reaches the approve: an error (or a panic) is logged as `release_hook_failed` with the
//!   hook's name, and never undoes or blocks the approve, the other hooks, or a later apply.
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::future::BoxFuture;
use sqlx::PgPool;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::audit::Actor;
use crate::design::JobRef;
use crate::error::ApiResult;

/// What a hook is told about the approve.
#[derive(Clone, Debug)]
pub struct Approved {
    /// The job's tenant: a hook reads under RLS, so it scopes its own transaction to it
    /// (`tenancy::begin_scoped(db, tenant)`).
    pub tenant: Uuid,
    pub job: JobRef,
    pub release_id: String,
    /// Who approved.
    pub by: Actor,
}

pub trait ReleaseApproved: Send + Sync + 'static {
    /// The hook's name in the `release_hook_failed` log line.
    fn name(&self) -> &'static str;
    /// Runs after the approve committed, on the runtime pool `db` (role `agent_app`, RLS on). An `Err` is logged,
    /// nothing else.
    fn on_release_approved<'a>(
        &'a self,
        db: &'a PgPool,
        approved: &'a Approved,
    ) -> BoxFuture<'a, ApiResult<()>>;
}

/// The registered hooks (`AppState.release_approved`). Registration may happen after the state is built (the field
/// is shared): the list is copied out before anything runs, so the lock is never held across an `.await`.
#[derive(Default)]
pub struct ApprovedHooks {
    hooks: Mutex<Vec<Arc<dyn ReleaseApproved>>>,
}

impl ApprovedHooks {
    /// Add a hook; hooks run in registration order.
    pub fn register_approved(&self, h: Arc<dyn ReleaseApproved>) {
        self.hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(h);
    }

    /// The registered hooks, in registration order.
    pub fn hooks(&self) -> Vec<Arc<dyn ReleaseApproved>> {
        self.hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Spawn one task that runs every registered hook in order (call it AFTER the approve committed). Each hook runs
    /// on its own task, so a panic is caught and logged like an error; neither stops the hooks after it. `None` when
    /// no hook is registered. The returned handle is for tests; approve drops it.
    pub fn spawn_after_commit(&self, db: PgPool, approved: Approved) -> Option<JoinHandle<()>> {
        let hooks = self.hooks();
        if hooks.is_empty() {
            return None;
        }
        Some(tokio::spawn(async move {
            for h in hooks {
                let name = h.name();
                let (db, a) = (db.clone(), approved.clone());
                let run = tokio::spawn(async move { h.on_release_approved(&db, &a).await });
                let failure = match run.await {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(format!("{e:?}")),
                    Err(e) => Some(format!("the hook task failed: {e}")),
                };
                if let Some(error) = failure {
                    tracing::warn!(
                        hook = name,
                        job = %approved.job.key,
                        release = %approved.release_id,
                        error = %error,
                        "release_hook_failed"
                    );
                }
            }
        }))
    }
}
