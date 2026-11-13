//! A2 "Done when" (real stack): a tool needing `customer_id` -> a CRM op needing `email` + `auth_token` -> an IAM op
//! needing a one-time code. The agent asks, the user replies, the chain completes, the original tool runs, and the
//! model transcript holds no code, no token and no id.
use agent_api::runtime::Env;
use serde_json::json;

use super::world::*;

#[tokio::test]
async fn a_tool_needing_a_customer_id_runs_the_whole_chain() {
    let w = World::build(
        "iam-a2-chain",
        tool_then_say("order_list", json!({}), ANSWER),
        false,
    )
    .await;
    let f = &w.f;
    w.wire(f, "test").await;
    w.chain_policies(f).await;
    w.daemons(f, Env::Test, &[READ], false).await;

    let conv = w.signin_flow(f).await;

    // the system saw exactly the chain, in order, with the platform's own credential only
    assert_eq!(
        w.sys.paths(),
        ["/porting/read", "/party/list", "/order/list"]
    );
    for h in w.sys.hits() {
        assert_eq!(h.auth, format!("Bearer {SERVICE_TOKEN}"), "{h:?}");
    }

    // the transcript stores the code redacted, never the code
    let msgs = w.messages(f, &conv).await;
    assert!(
        msgs.iter()
            .any(|m| m.0 == "customer" && m.1 == "[redacted]"),
        "{msgs:?}"
    );
    assert!(msgs.iter().all(|m| !m.1.contains(CODE)), "{msgs:?}");

    // state: the token is a sealed secret (no plain value), the customer id a value, the code a sealed secret
    let state = w.state(f, &conv).await;
    let get = |k: &str| {
        state
            .iter()
            .find(|s| s.0 == k)
            .unwrap_or_else(|| panic!("{k} in {state:?}"))
    };
    let token = get("auth_token");
    assert_eq!(token.1, "secret");
    assert!(token.2.is_none(), "no plain value for a secret");
    assert!(
        token
            .3
            .as_deref()
            .is_some_and(|s| !s.is_empty() && !s.contains(TOKEN)),
        "sealed"
    );
    let id = get("customer_id");
    assert_eq!((id.1.as_str(), id.2.as_deref()), ("value", Some(CUSTOMER)));
    let code = get("one_time_code");
    assert_eq!(code.1, "secret");
    assert!(code.2.is_none());

    // audit trail: producers ran as `resolve` sub-calls, the tool as the assistant's own call
    let calls = w.calls(f).await;
    for op in [VERIFY, LOOKUP] {
        assert!(
            calls
                .iter()
                .any(|c| c.0 == op && c.1 == "ok" && c.3 == "resolve"),
            "{op} resolved: {calls:?}"
        );
    }
    assert!(
        calls
            .iter()
            .any(|c| c.0 == READ && c.1 == "ok" && c.3 != "resolve"),
        "{calls:?}"
    );

    // the model never saw the code, the token or the id
    let seen = w.llm.everything();
    for secret in [CODE, TOKEN, CUSTOMER] {
        assert!(!seen.contains(secret), "{secret} reached a model request");
    }
    assert!(!w.llm.assistant_bodies().is_empty());
}
