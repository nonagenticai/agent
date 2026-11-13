//! Default operation routes (B4 R10) behind [`RouteResolver`]: where the runner sends an operation call of a scope.
//!
//! | Operation | Target |
//! |---|---|
//! | `knowledge.*`, `rule.evaluate` (any variant) | `builtin` (runs inside agent, K49) |
//! | `case.create{handover}`, `case.create{complaint}` | `agent_cases` (the fallback case store) |
//! //! | anything else | `not_offered` |
//!
//! B7's `operation_route` rows (0094) are consulted BEFORE these
//! defaults: the runner's [`EffectiveRoutes`] reads the job's effective routes (`connectors::routes_store::effective`,
//! R10) first and falls back to [`DefaultRoutes::default_target`].
//!
//! [`ResolvedTarget`] is B7's `connectors::runtime::ResolvedTarget` (re-exported): one type from the resolver to the
//! executor.
use futures_util::future::BoxFuture;
use sqlx::PgConnection;
use uuid::Uuid;

use super::scope::Env;
use crate::error::ApiResult;

/// An instance id the integration tests use for a stub connector instance (not read by any runtime logic).
pub const DEMO_STUB_INSTANCE: &str = "demo-retail-stub";

pub use crate::connectors::runtime::ResolvedTarget;

/// The `operation_call.route` text of a target (0062 CHECK: `agent_cases` is stored as `fallback`).
pub fn route_kind(t: &ResolvedTarget) -> &'static str {
    match t {
        ResolvedTarget::Instance { .. } => "instance",
        ResolvedTarget::Builtin { .. } => "builtin",
        ResolvedTarget::AgentCases => "fallback",
        ResolvedTarget::DeepLink { .. } => "deep_link",
        ResolvedTarget::NotOffered => "not_offered",
    }
}

/// One route question: operation `op` (`object.verb` / `object.verb{variant}`) of a scope.
#[derive(Clone, Debug)]
pub struct RouteQuery {
    pub tenant: Uuid,
    pub job_id: Uuid,
    pub job_key: String,
    pub env: Env,
    pub op: String,
}

/// Resolves an operation of a scope to its target. B7 implements it over `operation_route`; B4 ships
/// [`DefaultRoutes`]. Runs inside the caller's (tenant-scoped) transaction, never across an upstream call.
pub trait RouteResolver: Send + Sync {
    fn resolve<'a>(
        &'a self,
        c: &'a mut PgConnection,
        q: &'a RouteQuery,
    ) -> BoxFuture<'a, ApiResult<ResolvedTarget>>;
}

/// The R10 defaults (pure: reads no table).
#[derive(Clone, Debug)]
pub struct DefaultRoutes;

impl DefaultRoutes {
    pub fn new() -> DefaultRoutes {
        DefaultRoutes
    }

    /// The default target of `q` (R10, table in the module docs).
    pub fn default_target(&self, q: &RouteQuery) -> ResolvedTarget {
        let op = q.op.as_str();
        if let Some(detail) = builtin_family(op) {
            return ResolvedTarget::Builtin {
                detail: Some(detail.to_owned()),
            };
        }
        if op == "case.create{handover}" || op == "case.create{complaint}" {
            return ResolvedTarget::AgentCases;
        }
        ResolvedTarget::NotOffered
    }
}

/// The built-in family (`knowledge`, `rule`) of operation `op`, if it is a built-in: `knowledge.search`,
/// `knowledge.answer` and `rule.evaluate` (any variant, the rule id).
pub fn builtin_family(op: &str) -> Option<&'static str> {
    let family = op.split('.').next().unwrap_or_default();
    let verb_part = op.split_once('.').map_or("", |(_, v)| v);
    let verb = verb_part.split('{').next().unwrap_or_default();
    let builtin = match (family, verb) {
        ("knowledge", "search" | "answer") => Some("knowledge"),
        ("rule", "evaluate") => Some("rule"),
        _ => None,
    };
    builtin.filter(|_| verb_part == verb || family == "rule")
}

impl Default for DefaultRoutes {
    fn default() -> Self {
        Self::new()
    }
}

impl RouteResolver for DefaultRoutes {
    fn resolve<'a>(
        &'a self,
        _c: &'a mut PgConnection,
        q: &'a RouteQuery,
    ) -> BoxFuture<'a, ApiResult<ResolvedTarget>> {
        let target = self.default_target(q);
        Box::pin(async move { Ok(target) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(op: &str, env: Env) -> RouteQuery {
        RouteQuery {
            tenant: Uuid::from_u128(1),
            job_id: Uuid::from_u128(2),
            job_key: "customer_care_agent".into(),
            env,
            op: op.into(),
        }
    }

    #[test]
    fn r10_defaults() {
        let r = DefaultRoutes::new();
        let builtin = |d: &str| ResolvedTarget::Builtin {
            detail: Some(d.into()),
        };
        assert_eq!(
            r.default_target(&q("knowledge.search", Env::Prod)),
            builtin("knowledge")
        );
        assert_eq!(
            r.default_target(&q("rule.evaluate{RET-01}", Env::Test)),
            builtin("rule")
        );
        assert_eq!(
            r.default_target(&q("case.create{handover}", Env::Test)),
            ResolvedTarget::AgentCases
        );
        // anything else is not offered by default
        assert_eq!(
            r.default_target(&q("order.read", Env::Test)),
            ResolvedTarget::NotOffered
        );
        assert_eq!(
            r.default_target(&q("knowledge.search{x}", Env::Test)),
            ResolvedTarget::NotOffered
        );
    }
}
