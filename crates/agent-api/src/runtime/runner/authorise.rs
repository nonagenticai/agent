//! The runner's authorisation steps (B4 T6, spec 12 §3.3): 1b tenant active, 3 the state the op requires (A2:
//! [`super::declared::required_keys`], `state_required`; [`identity`] stays for the staff-only check and the probes),
//! the step's `performed_by` and preconditions (T6a), 4 confirmation, 5b the B5 tool gate. Each answers `Ok(())` or the `denied`
//! outcome code; nothing here calls upstream. Step 7b (the per-case cap) is [`super::cap`].
//!
//! Step 2 (the licence gate) is [`super::licence`]; 6b (toolset) and 7 (activity) are B5's and are implemented:
//! `job_design::gates::hooks::ToolPoint` (step 5b) asks `DesignToolsets::allows` (role enabled, the activity known to the
//! toolset, the op in its grant) before its P2 questions. Step 8 (route health) is [`super::route_health`].
use std::pin::Pin;

use agent_model::kinds::catalog::IdentityLevel;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::runtime::conditions;
use crate::runtime::design::RuntimeDesign;
use crate::runtime::domain::OpCall;
use crate::state::AppState;

/// Step 1b: the tenant of the open (tenant-scoped) transaction is active; else `tenant_suspended` /
/// `tenant_archived`. A tenant row the runtime cannot see is `tenant_not_found`.
pub async fn tenant_active(c: &mut PgConnection) -> ApiResult<Result<(), &'static str>> {
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM tenants WHERE id = app_current_tenant()")
            .fetch_optional(&mut *c)
            .await?;
    Ok(match status.as_deref() {
        Some("active") => Ok(()),
        Some("suspended") => Err("tenant_suspended"),
        Some("archived") => Err("tenant_archived"),
        _ => Err("tenant_not_found"),
    })
}

/// The case identity level (0..=3) an operation needs; `None` for a staff-only operation (no customer case may run
/// it).
pub fn required_level(l: IdentityLevel) -> Option<u8> {
    match l {
        IdentityLevel::Public => Some(0),
        IdentityLevel::Identified => Some(1),
        IdentityLevel::Verified => Some(2),
        IdentityLevel::StepUp => Some(3),
        IdentityLevel::Staff => None,
    }
}

/// Step 3: the case's identity level reaches the catalogue's; else `identity_required` (`staff_only` for an
/// operation only staff may run).
pub fn identity(have: u8, need: IdentityLevel) -> Result<(), &'static str> {
    match required_level(need) {
        None => Err("staff_only"),
        Some(n) if have >= n => Ok(()),
        Some(_) => Err("identity_required"),
    }
}

/// T6a: a step that names the role that runs it (`performed_by`) is refused to any other role: `not_entitled`
/// (the reason B5's toolset check gives).
pub fn performed_by(call: &OpCall) -> Result<(), &'static str> {
    match call
        .performed_by
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(role) if role != call.agent_role => Err("not_entitled"),
        _ => Ok(()),
    }
}

/// T6a (F-20), after step 3: the step's preconditions on the call's case facts (the identity level is the call's,
/// as step 3 read it). `Err` = the ids of the conditions that do not hold (comma separated, the call's `detail`).
pub fn preconditions(call: &OpCall) -> Result<(), String> {
    if call.preconditions.is_empty() {
        return Ok(());
    }
    let mut facts = call.facts.clone();
    facts.identity_level = call.identity_level;
    let failed = conditions::failed(&call.preconditions, &facts);
    if failed.is_empty() {
        Ok(())
    } else {
        Err(failed.join(", "))
    }
}

/// Step 4: a W/X/P call carries a `confirm_ref` that is a message authored by the conversation's user (12 §3.2: never
/// LLM or backend text): role `customer` (an end user) or `system` (a machine caller, A4); else
/// `confirmation_required`. A declaration with `confirmation: auto` (A2, worker jobs) and a runtime `resolve`
/// producer call skip this step at the caller.
pub async fn confirmed(c: &mut PgConnection, call: &OpCall) -> ApiResult<Result<(), &'static str>> {
    let Some(r) = call
        .confirm_ref
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    else {
        return Ok(Err("confirmation_required"));
    };
    let found: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM conversation_message \
                         WHERE conversation_id = $1 AND role IN ('customer', 'caller') AND message_id::text = $2)",
    )
    .bind(&call.conversation_id)
    .bind(r.to_owned())
    .fetch_one(&mut *c)
    .await?;
    Ok(if found {
        Ok(())
    } else {
        Err("confirmation_required")
    })
}

/// Step 5b: B5's tool gate (toolsets, D94 verdicts), asked PER CALL with the actual call (`op_key` and its
/// arguments) of tenant `tenant` in the case's design. `Err(code)` denies the call. Async: B5's P2 asks Laya.
pub trait ToolGate: Send + Sync {
    fn check<'a>(
        &'a self,
        st: &'a AppState,
        tenant: Uuid,
        design: &'a RuntimeDesign,
        call: &'a OpCall,
        op_key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
}

/// B4's gate: allows every call (tests; `bin/runner` installs B5's `ToolPoint`).
pub struct AllowAll;

impl ToolGate for AllowAll {
    fn check<'a>(
        &'a self,
        _st: &'a AppState,
        _tenant: Uuid,
        _design: &'a RuntimeDesign,
        _call: &'a OpCall,
        _op_key: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_levels_compare_by_rank() {
        assert_eq!(identity(0, IdentityLevel::Public), Ok(()));
        assert_eq!(
            identity(1, IdentityLevel::Verified),
            Err("identity_required")
        );
        assert_eq!(identity(2, IdentityLevel::Verified), Ok(()));
        assert_eq!(identity(3, IdentityLevel::StepUp), Ok(()));
        assert_eq!(identity(3, IdentityLevel::Staff), Err("staff_only"));
    }

    #[test]
    fn performed_by_names_the_one_role_that_may_run_the_step() {
        let mut call: OpCall = serde_json::from_value(serde_json::json!({
            "call_id": "opc_00000000-0000-0000-0000-000000000001", "case_id": "CS-1", "conversation_id": "c",
            "phase": 0, "step": "s", "attempt": 1, "agent_role": "assistant", "operation": "order.read",
            "env": "test", "initiator": "assistant@t", "deadline_ms": 0
        }))
        .unwrap();
        assert_eq!(performed_by(&call), Ok(()), "no role named: any role");
        call.performed_by = Some("procedure".into());
        assert_eq!(performed_by(&call), Err("not_entitled"));
        call.agent_role = "procedure".into();
        assert_eq!(performed_by(&call), Ok(()));
    }
}
