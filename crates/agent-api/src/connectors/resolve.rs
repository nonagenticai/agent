//! B7 T13: route resolution, exactly as TS `routeReadiness` / `resolveRoute` / `suggestRoute` of
//! `web/src/lib/connectors/ops.ts`, plus the serde mirrors of the
//! J6 shapes they read (`OperationRoute`, `RouteTarget`, `ExpectedOperation`, `JobOperations` minus cells). Pure: the
//! database side is crate::connectors::routes_store.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::model::OpRef;
use super::opkey::{op_key, same_op};

fn star() -> String {
    "*".to_owned()
}

/// `RouteTarget` of `types/connectors.ts` (`kind`-tagged; `detail` absent when unset).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouteTarget {
    Instance {
        instance_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Builtin,
    AgentCases,
    DeepLink {
        url: String,
    },
    NotOffered,
}

impl RouteTarget {
    /// The `kind` literal (`operation_route.target_kind`).
    pub fn kind(&self) -> &'static str {
        match self {
            RouteTarget::Instance { .. } => "instance",
            RouteTarget::Builtin => "builtin",
            RouteTarget::AgentCases => "agent_cases",
            RouteTarget::DeepLink { .. } => "deep_link",
            RouteTarget::NotOffered => "not_offered",
        }
    }
}

/// `OperationRoute` (`operation_route(tenant, job, object, verb, variant, env)`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationRoute {
    pub object: String,
    pub verb: String,
    #[serde(default = "star")]
    pub variant: String,
    pub env: String,
    pub target: RouteTarget,
    #[serde(default)]
    pub updated_by: String,
    /// RFC 3339 (`2026-10-08T15:00:00Z`).
    #[serde(default)]
    pub updated_at: String,
}

impl OperationRoute {
    pub fn op_ref(&self) -> OpRef {
        OpRef {
            object: self.object.clone(),
            verb: self.verb.clone(),
            variant: self.variant.clone(),
        }
    }
}

/// `ExpectedOperation`: one expected operation of the job with its catalogue attributes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExpectedOperation {
    pub object: String,
    pub verb: String,
    #[serde(default = "star")]
    pub variant: String,
    #[serde(default)]
    pub modality: String,
    #[serde(default)]
    pub is_derive: bool,
    #[serde(default)]
    pub side_effect: String,
    #[serde(default)]
    pub identity_level: String,
    #[serde(default)]
    pub stages: Vec<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub note: String,
}

impl ExpectedOperation {
    pub fn op_ref(&self) -> OpRef {
        OpRef {
            object: self.object.clone(),
            verb: self.verb.clone(),
            variant: self.variant.clone(),
        }
    }
}

/// `JobOperations.instances[]`: the instance fields route resolution reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteInstance {
    pub id: String,
    #[serde(default)]
    pub label: String,
    pub env: String,
    #[serde(default)]
    pub stub: bool,
    #[serde(default)]
    pub platform: bool,
    #[serde(default)]
    pub template_key: String,
    #[serde(default)]
    pub template_version: String,
    #[serde(default)]
    pub status: String,
}

/// `JobOperations.contract[id][]`: one T3 result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContractLite {
    pub op: String,
    pub outcome: String,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub at: Option<String>,
}

/// TS `RouteData` (= `Omit<JobOperations, 'cells'>`): what route resolution and the cells read. Unknown fields of
/// the TS shape (`job`, `draft_rev`, `stats`, `names`) are ignored on read.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RouteData {
    #[serde(default)]
    pub ops: Vec<ExpectedOperation>,
    #[serde(default)]
    pub routes: Vec<OperationRoute>,
    #[serde(default)]
    pub instances: Vec<RouteInstance>,
    /// `object.verb{variant}` each instance's template implements, keyed by instance id.
    #[serde(default)]
    pub implements: BTreeMap<String, Vec<String>>,
    /// Instances whose template is a draft: allowed on `test` only.
    #[serde(default)]
    pub draft_instances: Vec<String>,
    /// T3-green ops per instance id.
    #[serde(default)]
    pub tested: BTreeMap<String, Vec<String>>,
    /// Every T3 result per instance id (optional in TS).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract: Option<BTreeMap<String, Vec<ContractLite>>>,
}

/// TS `ResolvedRoute`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedRoute {
    pub route: Option<OperationRoute>,
    /// The route came from the `*` row of a variant.
    pub inherited: bool,
    /// `ok` | `untested` | `stub` | `missing` | `not_offered`.
    pub readiness: &'static str,
    pub suggested: Option<RouteTarget>,
}

fn find_route<'a>(
    routes: &'a [OperationRoute],
    op: &OpRef,
    env: &str,
) -> Option<&'a OperationRoute> {
    routes
        .iter()
        .find(|r| r.env == env && same_op(&r.op_ref(), op))
}

fn with_star(op: &OpRef) -> OpRef {
    OpRef {
        object: op.object.clone(),
        verb: op.verb.clone(),
        variant: "*".to_owned(),
    }
}

/// The route the runner uses for `op` on `env` (TS `routeReadiness`): the variant's own route, else the `*` route
/// of the same (object, verb) (`inherited`); plus readiness. No suggestion.
pub fn route_readiness<'a>(
    d: &'a RouteData,
    op: &OpRef,
    env: &str,
) -> (Option<&'a OperationRoute>, bool, &'static str) {
    let mut route = find_route(&d.routes, op, env);
    let mut inherited = false;
    if route.is_none() && op.variant != "*" {
        route = find_route(&d.routes, &with_star(op), env);
        inherited = route.is_some();
    }
    let key = op_key(op);
    let Some(r) = route else {
        return (None, false, "missing");
    };
    let readiness = match &r.target {
        RouteTarget::NotOffered => "not_offered",
        RouteTarget::Instance { instance_id, .. } => {
            match d.instances.iter().find(|i| &i.id == instance_id) {
                None => "stub",
                Some(inst) if inst.stub => "stub",
                Some(inst) if inst.platform => "ok",
                Some(inst) => {
                    let served = if inherited {
                        op_key(&with_star(op))
                    } else {
                        key
                    };
                    let tested = d.tested.get(&inst.id).is_some_and(|t| t.contains(&served));
                    if tested { "ok" } else { "untested" }
                }
            }
        }
        _ => "ok",
    };
    (Some(r), inherited, readiness)
}

/// TS `resolveRoute`: [`route_readiness`] plus, when no route exists, [`suggest_route`].
pub fn resolve_route(d: &RouteData, op: &OpRef, env: &str) -> ResolvedRoute {
    let (route, inherited, readiness) = route_readiness(d, op, env);
    let suggested = if route.is_some() {
        None
    } else {
        suggest_route(d, op, env)
    };
    ResolvedRoute {
        route: route.cloned(),
        inherited,
        readiness,
        suggested,
    }
}

/// The operations the agent itself runs (`builtin` targets): exactly the runtime's
/// [`builtin_family`](crate::runtime::routes::builtin_family) — `knowledge.search`, `knowledge.answer` and
/// `rule.evaluate` (any variant).
pub fn is_builtin_op(op: &OpRef) -> bool {
    crate::runtime::routes::builtin_family(&op_key(op)).is_some()
}

/// The built-in operations, for messages.
pub const BUILTIN_OPS_TEXT: &str = "knowledge.search, knowledge.answer and rule.evaluate";

/// TS `suggestRoute` (doc 10 §5.2 step 5): built-in for the [`is_builtin_op`] operations; else the first non-stub
/// instance on `env` (or `all`) that implements the op (tenant systems before platform connectors, never a draft-
/// template instance on prod; the `agent_cases` connector answers `agent_cases`); else `agent_cases` for case and
/// handover operations; else none.
pub fn suggest_route(d: &RouteData, op: &OpRef, env: &str) -> Option<RouteTarget> {
    let key = op_key(op);
    let star = op_key(&with_star(op));
    if is_builtin_op(op) {
        return Some(RouteTarget::Builtin);
    }
    let implements_it = |id: &str| {
        d.implements
            .get(id)
            .is_some_and(|impl_| impl_.iter().any(|k| *k == key || *k == star))
    };
    let candidates = d
        .instances
        .iter()
        .filter(|i| !i.platform)
        .chain(d.instances.iter().filter(|i| i.platform));
    for inst in candidates {
        if inst.stub {
            continue;
        }
        if inst.env != env && inst.env != "all" {
            continue;
        }
        if env == "prod" && d.draft_instances.contains(&inst.id) {
            continue;
        }
        if !implements_it(&inst.id) {
            continue;
        }
        if inst.template_key == "agent_cases" {
            return Some(RouteTarget::AgentCases);
        }
        return Some(RouteTarget::Instance {
            instance_id: inst.id.clone(),
            detail: None,
        });
    }
    if op.object == "case" || op.object == "handover" {
        return Some(RouteTarget::AgentCases);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn targets_serialize_like_ts() {
        let t = RouteTarget::Instance {
            instance_id: "shop".into(),
            detail: None,
        };
        assert_eq!(
            serde_json::to_value(&t).unwrap(),
            json!({"kind": "instance", "instance_id": "shop"})
        );
        let back: RouteTarget =
            serde_json::from_value(json!({"kind": "deep_link", "url": "https://x"})).unwrap();
        assert_eq!(
            back,
            RouteTarget::DeepLink {
                url: "https://x".into()
            }
        );
        assert_eq!(
            serde_json::to_value(RouteTarget::NotOffered).unwrap(),
            json!({"kind": "not_offered"})
        );
    }

    fn op(object: &str, verb: &str, variant: &str) -> OpRef {
        OpRef {
            object: object.into(),
            verb: verb.into(),
            variant: variant.into(),
        }
    }

    #[test]
    fn builtin_ops_match_the_runtime() {
        for (o, v, var) in [
            ("knowledge", "search", "*"),
            ("knowledge", "search", ""),
            ("knowledge", "answer", "*"),
            ("rule", "evaluate", "*"),
            ("rule", "evaluate", "refund_window"),
        ] {
            assert!(is_builtin_op(&op(o, v, var)), "{o}.{v}{{{var}}}");
        }
        for (o, v, var) in [
            ("knowledge", "search", "x"),
            ("knowledge", "answer", "x"),
            ("credential", "verify", "*"),
            ("knowledge", "read", "*"),
            ("order", "read", "*"),
        ] {
            assert!(!is_builtin_op(&op(o, v, var)), "{o}.{v}{{{var}}}");
        }
    }

    #[test]
    fn knowledge_ops_are_suggested_builtin() {
        let d = RouteData::default();
        for verb in ["search", "answer"] {
            assert_eq!(
                suggest_route(&d, &op("knowledge", verb, "*"), "prod"),
                Some(RouteTarget::Builtin)
            );
        }
        assert_ne!(
            suggest_route(&d, &op("knowledge", "search", "x"), "prod"),
            Some(RouteTarget::Builtin)
        );
    }
}
