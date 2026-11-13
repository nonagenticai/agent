//! B4 T4 — the operation catalogue (crate::runtime::catalog, R9) compiled from a job's pins, and the default routes
//! (crate::runtime::routes, R10).
use agent_api::design::DesignOf;
use agent_api::runtime::Env;
use agent_api::runtime::catalog::{OpSource, OperationCatalog};
use agent_api::runtime::routes::{
    DefaultRoutes, ResolvedTarget, RouteQuery, RouteResolver, route_kind,
};
use agent_model::kinds::catalog::{IdentityLevel, SideEffect};

use crate::b4::design::{load, put_target, setup, setup_on};

/// The operations the test world's template authors for the demo stub's faults (a subset of the web
/// `FAULT_OPERATIONS`, `fixtures/live.ts`).
const FAULT_OPERATIONS: [&str; 8] = [
    "party.read",
    "order.read",
    "order.cancel",
    "delivery.read",
    "payment.list",
    "payment.refund",
    "case.create{return}",
    "rule.evaluate{RET-01}",
];

#[tokio::test]
async fn catalogue_has_the_fault_operations() {
    let fx = setup_on(None, true).await;
    put_target(&fx).await;
    let d = load(&fx, DesignOf::Draft).await;
    assert!(d.template_errors.is_empty(), "{:?}", d.template_errors);
    let c = &d.catalog;
    for op in FAULT_OPERATIONS {
        let def = c
            .get(op)
            .unwrap_or_else(|| panic!("{op} is in the job's catalogue: {:?}", c.keys()));
        assert_eq!(
            def.source,
            OpSource::Template,
            "{op} comes from the pinned seed"
        );
        assert!(
            !def.outputs.is_empty() || def.verb == "evaluate",
            "{op} declares outputs"
        );
    }
    // a variant inherits the `*` row's columns (B6a resolution): outputs, effect, built-in flag
    let ret = c.get("rule.evaluate{RET-01}").unwrap();
    assert!(ret.builtin);
    assert_eq!(ret.side_effect, SideEffect::R);
    assert_eq!(ret.inputs, ["order_id", "delivered_at", "return_lines"]);
    assert_eq!(
        c.outputs("rule.evaluate{RET-01}"),
        ["outcome", "reason", "options"]
    );
    let refund = c.get("payment.refund").unwrap();
    assert!(refund.writes(), "{refund:?}");
    // B7's OpRequest.outputs are the declared ones only
    assert_eq!(
        c.outputs("order.read"),
        [
            "status",
            "lines",
            "total",
            "delivery_type",
            "payment_method_ref"
        ]
    );
    // the built-ins are present (from the seed here, so not from the static table)
    for op in [
        "knowledge.search",
        "knowledge.answer",
        "case.create{handover}",
    ] {
        assert!(c.get(op).is_some(), "{op}");
    }
    assert!(c.get("spaceship.launch").is_none());
    assert_eq!(
        c.get("party.list").unwrap().identity_level,
        IdentityLevel::Identified,
        "party.list is level 1"
    );
}

#[tokio::test]
async fn unknown_op_routes_not_offered() {
    let fx = setup(None).await;
    let routes = DefaultRoutes::new();
    let q = |op: &str, env: Env| RouteQuery {
        tenant: fx.f.tenant,
        job_id: fx.f.job.job_id,
        job_key: fx.f.job.job_key.clone(),
        env,
        op: op.to_string(),
    };
    let mut tx = fx.f.job.begin(&fx.e.state.db).await.unwrap();
    for (op, env, want) in [
        ("spaceship.launch", Env::Test, ResolvedTarget::NotOffered),
        ("order.read", Env::Prod, ResolvedTarget::NotOffered),
        ("order.read", Env::Test, ResolvedTarget::NotOffered),
        (
            "case.create{handover}",
            Env::Prod,
            ResolvedTarget::AgentCases,
        ),
        (
            "knowledge.answer",
            Env::Prod,
            ResolvedTarget::Builtin {
                detail: Some("knowledge".into()),
            },
        ),
    ] {
        let got = routes.resolve(&mut tx, &q(op, env)).await.unwrap();
        assert_eq!(got, want, "{op} {env}");
    }
    tx.commit().await.unwrap();
    assert_eq!(route_kind(&ResolvedTarget::NotOffered), "not_offered");
    // 0062's CHECK names the agent_cases target `fallback`
    assert_eq!(route_kind(&ResolvedTarget::AgentCases), "fallback");
}
