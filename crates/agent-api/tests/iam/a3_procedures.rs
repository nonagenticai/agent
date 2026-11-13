//! A3 "Done when": a tenant admin defines ONE identity procedure, attaches it to two jobs, and both resolve through
//! it; a job without it has no producer; a procedure cannot be deleted while attached.
use agent_api::runtime::Env;
use serde_json::json;

use super::world::*;
use crate::common::conv::code;

#[tokio::test]
async fn one_procedure_serves_two_jobs_and_not_a_detached_one() {
    let w = World::build(
        "iam-a3",
        tool_then_say("order_list", json!({}), ANSWER),
        false,
    )
    .await;
    let (job_b, job_c) = (w.new_job().await, w.new_job().await);
    let (a, b, c) = (&w.f, &job_b, &job_c);
    w.wire(a, "test").await;

    // the tenant admin creates the procedure
    let (s, p) = w
        .tenant_call(
            "POST",
            "/api/iam-procedures",
            json!({"name": "Acme sign-in", "description": "one-time code, then CRM", "steps": signin_steps()}),
        )
        .await;
    assert_eq!(s, 201, "{p}");
    let id = p["id"].as_str().unwrap().to_owned();
    let (s, dup) = w
        .tenant_call(
            "POST",
            "/api/iam-procedures",
            json!({"name": "Acme sign-in", "steps": []}),
        )
        .await;
    assert_eq!((s.as_u16(), code(&dup)), (409, "name_taken"), "{dup}");

    // every job declares only the consumer; a and b attach the procedure, c does not
    for f in [a, b, c] {
        w.policy_ok(f, READ, read_policy(false)).await;
    }
    for f in [a, b] {
        let (s, r) = w
            .call(f, "PUT", &format!("iam-procedures/{id}"), json!({}))
            .await;
        assert!(s.is_success(), "{s} {r}");
    }
    let (s, list) = w.tenant_call("GET", "/api/iam-procedures", json!({})).await;
    assert_eq!(s, 200, "{list}");
    assert_eq!(list[0]["jobs"], 2, "{list}");

    // delete is blocked while attached
    let (s, r) = w
        .tenant_call("DELETE", &format!("/api/iam-procedures/{id}"), json!({}))
        .await;
    assert_eq!((s.as_u16(), code(&r)), (409, "procedure_attached"), "{r}");

    // both attached jobs resolve through the one procedure
    for f in [a, b] {
        w.daemons(f, Env::Test, &[READ], false).await;
        let before = w.sys.paths().len();
        w.signin_flow(f).await;
        assert_eq!(
            w.sys.paths()[before..],
            ["/porting/read", "/party/list", "/order/list"],
            "job {}",
            f.job.job_key
        );
    }

    // the detached job has no producer for customer_id: the tool is not run and a person takes over
    w.daemons(c, Env::Test, &[READ], false).await;
    let before = w.sys.paths().len();
    let conv = w.start_chat(c).await;
    w.say(c, &conv, ASK).await;
    let (reason, _) = w.handover(c).await;
    assert_eq!(reason, "tool_failed");
    assert_eq!(w.sys.paths().len(), before, "nothing reached the system");
    let calls = w.calls(c).await;
    assert!(
        calls
            .iter()
            .any(|x| x.0 == READ && x.2.as_deref() == Some("input_unresolved")),
        "{calls:?}"
    );

    // detached everywhere, the procedure can go
    for f in [a, b] {
        let (s, r) = w
            .call(f, "DELETE", &format!("iam-procedures/{id}"), json!({}))
            .await;
        assert!(s.is_success(), "{s} {r}");
    }
    let (s, r) = w
        .tenant_call("DELETE", &format!("/api/iam-procedures/{id}"), json!({}))
        .await;
    assert_eq!(s, 204, "{r}");
}
