//! Support routing (migration 0204, crate::support::routing): queued handovers go round-robin to the staff who are
//! ONLINE on the support surface. The least recently assigned eligible member (support_agent or support_manager) is
//! picked, a human Take always wins the race, a stale AUTO assignment moves on and a person's choice never does.
//! SW-7: the per-tenant platform flag `tenants.support_auto_assign` is OFF by default and gates all of it; every test
//! that expects an assignment switches it on first ([`flag`]). The sweeper is called through `routing::tick` (no waiting on the 30 s interval). Real Postgres 18.
use crate::common;

use std::time::{Duration, Instant};

use agent_api::support::routing::{self, Outcome};
use axum::http::StatusCode;
use common::conv::{self, Ctx, code};
use serde_json::json;

/// SW-7: the platform flag `tenants.support_auto_assign` is OFF by default; set it as the super_admin's PATCH does.
async fn flag(c: &Ctx, on: bool) {
    sqlx::query("UPDATE tenants SET support_auto_assign = $2 WHERE id = $1")
        .bind(c.t)
        .bind(on)
        .execute(&c.e.owner)
        .await
        .unwrap();
}

/// How long a test waits for routing to land.
const WAIT: Duration = Duration::from_secs(5);

/// A web chat conversation `wc` whose customer asked for a person: (conversation id, item id).
async fn queued(c: &Ctx, wc: &str) -> (String, String) {
    let (s, b) = c
        .inbound(wc, &format!("{wc}-1"), "Can I speak to a real person?")
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["status"], "queued", "{b}");
    (
        b["conversation_id"].as_str().unwrap().to_string(),
        b["item_id"].as_str().unwrap().to_string(),
    )
}

/// `sub` was seen on `surface` `age_min` minutes ago (one session per sub and surface).
async fn seen(c: &Ctx, sub: &str, surface: &str, age_min: i32) {
    // `support_seen_at` moves only for the support surface, as seen::write does
    sqlx::query(
        "INSERT INTO member_session (tenant_id, sub, sid, surface, last_seen_at, support_seen_at) \
         VALUES ($1, $2, $3, $4, now() - make_interval(mins => $5), \
                 CASE WHEN $4 = 'support' THEN now() - make_interval(mins => $5) END) \
         ON CONFLICT (tenant_id, sub, sid) DO UPDATE SET surface = EXCLUDED.surface, \
            last_seen_at = EXCLUDED.last_seen_at, \
            support_seen_at = CASE WHEN EXCLUDED.surface = 'support' THEN EXCLUDED.support_seen_at \
                                   ELSE member_session.support_seen_at END",
    )
    .bind(c.t)
    .bind(sub)
    .bind(format!("sid-{sub}"))
    .bind(surface)
    .bind(age_min)
    .execute(&c.e.owner)
    .await
    .unwrap();
}

/// `sub` is online now.
async fn online(c: &Ctx, sub: &str) {
    seen(c, sub, "support", 0).await;
}

/// Who owns `conv` (`queue`, `agent` or the staff member's sub).
async fn owner(c: &Ctx, conv: &str) -> String {
    c.texts(&format!(
        "SELECT CASE WHEN mode = 'staff' THEN by_sub ELSE mode END FROM conversation_control \
          WHERE conversation_id = '{conv}'"
    ))
    .await
    .remove(0)
}

/// Is item `item` stamped auto-assigned?
async fn auto(c: &Ctx, item: &str) -> bool {
    c.texts(&format!(
        "SELECT (auto_assigned_at IS NOT NULL)::text FROM agent_handover_items WHERE id = '{item}'"
    ))
    .await
    .remove(0)
        == "true"
}

/// The `auto_assigned` of `conv`'s row in the open tab of the inbox (as `u-mgr`), which the conversation header's owner
/// shares (`OwnerView`).
async fn inbox_auto(c: &Ctx, conv: &str) -> bool {
    let (s, b) = c.get("/api/support/inbox?tab=open", "u-mgr").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = b["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["conversation_id"] == conv)
        .unwrap_or_else(|| panic!("{conv} is not in the open tab: {b}"));
    row["owner"]["auto_assigned"].as_bool().unwrap()
}

/// Pretend `item` was assigned `mins` minutes ago.
async fn age_assignment(c: &Ctx, item: &str, mins: i32) {
    sqlx::query(
        "UPDATE agent_handover_items SET auto_assigned_at = now() - make_interval(mins => $2) \
          WHERE id = $1 AND auto_assigned_at IS NOT NULL",
    )
    .bind(item)
    .bind(mins)
    .execute(&c.e.owner)
    .await
    .unwrap();
}

async fn tick(c: &Ctx) -> usize {
    routing::tick(&c.e.state).await.unwrap()
}

// `queued` returns when the inbound request does; the trigger (`handover::notify_workers`, which routes) runs in a
// detached task after that. So an expected assignment is POLLED for, and "nothing is assigned" is established by
// running one routing decision ourselves (it serialises on the tenant's routing row behind any decision in flight).

/// Wait (up to [`WAIT`]) until `conv` is owned by `want`.
async fn await_owner(c: &Ctx, conv: &str, want: &str) {
    let end = Instant::now() + WAIT;
    loop {
        let got = owner(c, conv).await;
        if got == want {
            return;
        }
        assert!(
            Instant::now() < end,
            "{conv} is owned by {got:?}, expected {want:?} within {WAIT:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Wait (up to [`WAIT`]) until `conv` is owned by a member (not the queue); that member.
async fn await_assigned(c: &Ctx, conv: &str) -> String {
    let end = Instant::now() + WAIT;
    loop {
        let got = owner(c, conv).await;
        if got != "queue" {
            return got;
        }
        assert!(
            Instant::now() < end,
            "{conv} is still queued after {WAIT:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Run the sweeper (and let any trigger task land) until every conversation of `convs` has a member owner.
async fn sweep_until_assigned(c: &Ctx, convs: &[&str]) {
    let end = Instant::now() + WAIT;
    loop {
        tick(c).await;
        let mut queued = Vec::new();
        for conv in convs {
            if owner(c, conv).await == "queue" {
                queued.push(*conv);
            }
        }
        if queued.is_empty() {
            return;
        }
        assert!(
            Instant::now() < end,
            "{queued:?} still queued after {WAIT:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// `conv` stays with the queue: one routing decision (the trigger's, blocking on the tenant's routing row) skips it.
async fn nothing_assigned(c: &Ctx, conv: &str) {
    let out = routing::route_item(&c.e.state, c.t, c.job, conv, None, true)
        .await
        .unwrap();
    assert_eq!(out, Outcome::Skipped, "{conv}");
    assert_eq!(owner(c, conv).await, "queue", "{conv}");
}

#[tokio::test]
async fn round_robin_alternates_between_online_members() {
    let c = conv::setup().await;
    flag(&c, true).await;
    online(&c, "u-agent").await;
    online(&c, "u-agent2").await;
    // the trigger: each item is handed out as it is queued (after the commit, in notify_workers)
    let mut got = Vec::new();
    for i in 1..=4 {
        let (conv_id, item) = queued(&c, &format!("wc-{i}")).await;
        got.push(await_assigned(&c, &conv_id).await);
        assert!(auto(&c, &item).await, "{item} is stamped auto-assigned");
    }
    assert_eq!(got, ["u-agent", "u-agent2", "u-agent", "u-agent2"]);
    // the assignment reads like the others: an assigned item, a system line, an audit row
    let (conv_id, item) = queued(&c, "wc-5").await;
    await_owner(&c, &conv_id, "u-agent").await;
    assert_eq!(
        c.texts(&format!(
            "SELECT state FROM agent_handover_items WHERE id = '{item}'"
        ))
        .await,
        ["assigned"]
    );
    let lines = c
        .texts(&format!(
            "SELECT text FROM conversation_message WHERE conversation_id = '{conv_id}' AND role = 'system'"
        ))
        .await;
    assert!(
        lines.iter().any(|l| l.starts_with("Auto-assigned to ")),
        "{lines:?}"
    );
    assert!(
        !c.texts(
            "SELECT 1::text FROM audit_event WHERE action = 'conversation.auto_assign' LIMIT 1"
        )
        .await
        .is_empty()
    );
    // the sweeper alternates the same way for items that waited: the least recently assigned goes next (u-agent2
    // took the 4th, u-agent the 5th), here both items are queued while nobody is online
    sqlx::query("DELETE FROM member_session WHERE tenant_id = $1")
        .bind(c.t)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let (a, _) = queued(&c, "wc-6").await;
    let (b, _) = queued(&c, "wc-7").await;
    nothing_assigned(&c, &a).await;
    nothing_assigned(&c, &b).await;
    assert_eq!(tick(&c).await, 0, "nobody is online");
    online(&c, "u-agent").await;
    online(&c, "u-agent2").await;
    sweep_until_assigned(&c, &[&a, &b]).await;
    let mut two = [owner(&c, &a).await, owner(&c, &b).await];
    two.sort();
    assert_eq!(two, ["u-agent", "u-agent2"], "one each");
}

#[tokio::test]
async fn offline_members_are_skipped_and_item_stays_queued() {
    let c = conv::setup().await;
    flag(&c, true).await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    nothing_assigned(&c, &conv_id).await;
    assert_eq!(tick(&c).await, 0, "nobody online: the item stays queued");
    assert_eq!(owner(&c, &conv_id).await, "queue");
    // seen on another surface, or on the support surface too long ago, is not online
    seen(&c, "u-agent", "admin", 0).await;
    seen(&c, "u-agent2", "support", 11).await;
    assert_eq!(tick(&c).await, 0);
    assert_eq!(owner(&c, &conv_id).await, "queue");
    // a member who is online but not eligible (no support role) does not count either
    seen(&c, "u-obs", "support", 0).await;
    assert_eq!(tick(&c).await, 0);
    // u-agent2 comes online: it gets the item; u-agent (offline) never does
    online(&c, "u-agent2").await;
    sweep_until_assigned(&c, &[&conv_id]).await;
    assert_eq!(owner(&c, &conv_id).await, "u-agent2");
    assert!(auto(&c, &item).await);
}

#[tokio::test]
async fn single_member_tenant_gets_every_item() {
    let c = conv::setup().await;
    flag(&c, true).await;
    sqlx::query("UPDATE users SET disabled_at = now() WHERE tenant_id = $1 AND sub <> 'u-agent'")
        .bind(c.t)
        .execute(&c.e.owner)
        .await
        .unwrap();
    online(&c, "u-agent").await;
    for i in 1..=3 {
        let (conv_id, _) = queued(&c, &format!("wc-{i}")).await;
        await_owner(&c, &conv_id, "u-agent").await;
    }
    // and the sweeper's path: items queued while the member was away go to them when they are back
    sqlx::query("DELETE FROM member_session WHERE tenant_id = $1")
        .bind(c.t)
        .execute(&c.e.owner)
        .await
        .unwrap();
    let (a, _) = queued(&c, "wc-4").await;
    let (b, _) = queued(&c, "wc-5").await;
    nothing_assigned(&c, &a).await;
    nothing_assigned(&c, &b).await;
    assert_eq!(tick(&c).await, 0);
    online(&c, "u-agent").await;
    sweep_until_assigned(&c, &[&a, &b]).await;
    assert_eq!(owner(&c, &a).await, "u-agent");
    assert_eq!(owner(&c, &b).await, "u-agent");
}

#[tokio::test]
async fn human_take_wins_race_with_auto_assign() {
    let c = conv::setup().await;
    flag(&c, true).await;
    // a person takes the queued item before routing looks at it: routing skips, the Take stays
    let (conv_id, item) = queued(&c, "wc-1").await;
    let (s, b) = c.take(&conv_id, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    online(&c, "u-agent").await;
    assert_eq!(tick(&c).await, 0);
    assert_eq!(owner(&c, &conv_id).await, "u-agent2");
    assert!(!auto(&c, &item).await, "a Take is a person's choice");
    // the race itself: routing decided on `queue`, the Take got there first (a normal skip, not an error)
    let out = routing::route_item(&c.e.state, c.t, c.job, &conv_id, None, true)
        .await
        .unwrap();
    assert_eq!(out, Outcome::Skipped);
    assert_eq!(owner(&c, &conv_id).await, "u-agent2");
    // and the other way round: routing got there first, so the Take that expected `queue` is a 409
    let (conv2, _) = queued(&c, "wc-2").await;
    await_owner(&c, &conv2, "u-agent").await;
    let (s, b) = c.take(&conv2, "u-agent2", "queue").await;
    assert_eq!(s, StatusCode::CONFLICT, "{b}");
    assert_eq!(code(&b), "owner_changed", "{b}");
    assert_eq!(b["error"]["owner"]["owner"], "user:u-agent", "{b}");
}

#[tokio::test]
async fn stale_auto_assignment_moves_to_next_online_member() {
    let c = conv::setup().await;
    flag(&c, true).await;
    online(&c, "u-agent").await;
    let (idle, idle_item) = queued(&c, "wc-idle").await;
    let (worked, worked_item) = queued(&c, "wc-work").await;
    await_owner(&c, &idle, "u-agent").await;
    await_owner(&c, &worked, "u-agent").await;
    // the assignee answers one of them (a note counts as well as a reply)
    let (s, b) = c
        .send_message(&worked, "u-agent", "note", "Looking into it", None)
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");

    // not stale yet: a fresh assignment is left alone even though the assignee is away and a colleague is online
    seen(&c, "u-agent", "support", 30).await;
    online(&c, "u-agent2").await;
    assert_eq!(tick(&c).await, 0);
    // stale (10 min > the default 5) and the assignee is away: the idle one moves, the worked one stays
    age_assignment(&c, &idle_item, 10).await;
    age_assignment(&c, &worked_item, 10).await;
    assert_eq!(tick(&c).await, 1);
    assert_eq!(owner(&c, &idle).await, "u-agent2");
    assert_eq!(owner(&c, &worked).await, "u-agent");
    assert!(
        auto(&c, &idle_item).await,
        "still auto-assigned, with a fresh time"
    );
    assert!(
        inbox_auto(&c, &idle).await,
        "the new assignee's row is Auto"
    );
    // the new assignee has the full time: nothing moves on the next pass
    assert_eq!(tick(&c).await, 0);
    // an assignee who is still online keeps an untouched item
    age_assignment(&c, &idle_item, 10).await;
    assert_eq!(tick(&c).await, 0);
    assert_eq!(owner(&c, &idle).await, "u-agent2");
}

#[tokio::test]
async fn manual_assignment_is_never_reassigned() {
    let c = conv::setup().await;
    flag(&c, true).await;
    // a manager's Assign
    let (assigned, assigned_item) = queued(&c, "wc-assign").await;
    let (s, b) = c.assign(&assigned, "u-mgr", "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // a person's Take
    let (taken, taken_item) = queued(&c, "wc-take").await;
    let (s, b) = c.take(&taken, "u-agent", "queue").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    // an auto assignment its assignee then took for themselves
    online(&c, "u-agent").await;
    let (mine, mine_item) = queued(&c, "wc-mine").await;
    await_owner(&c, &mine, "u-agent").await;
    assert!(auto(&c, &mine_item).await);
    assert!(inbox_auto(&c, &mine).await, "routing assigned it: Auto");
    let (s, b) = c.take(&mine, "u-agent", "user:u-agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(!auto(&c, &mine_item).await, "the assignee made it theirs");
    assert!(!inbox_auto(&c, &mine).await, "a Take clears the marker");
    assert!(
        !inbox_auto(&c, &assigned).await,
        "a manager's Assign is manual"
    );
    assert!(!inbox_auto(&c, &taken).await, "a Take is manual");
    // a manager's Assign over an auto assignment is a person's choice too
    let (over, over_item) = queued(&c, "wc-over").await;
    await_owner(&c, &over, "u-agent").await;
    let (s, b) = c.assign(&over, "u-mgr", "u-agent2", "user:u-agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert!(!auto(&c, &over_item).await);
    assert!(
        !inbox_auto(&c, &over).await,
        "an Assign over Auto clears it"
    );

    // everyone who holds one is away, a colleague is online, and a long time passed: nothing moves
    seen(&c, "u-agent", "support", 60).await;
    online(&c, "u-mgr").await;
    for item in [&assigned_item, &taken_item, &mine_item, &over_item] {
        assert!(!auto(&c, item).await, "{item} is a person's choice");
    }
    assert_eq!(tick(&c).await, 0);
    assert_eq!(owner(&c, &assigned).await, "u-agent");
    assert_eq!(owner(&c, &taken).await, "u-agent");
    assert_eq!(owner(&c, &mine).await, "u-agent");
    assert_eq!(owner(&c, &over).await, "u-agent2");
}

#[tokio::test]
async fn mode_off_leaves_items_queued() {
    let c = conv::setup().await;
    flag(&c, true).await;
    online(&c, "u-agent").await;
    let (s, b) = c
        .call(
            "PUT",
            "/api/support/routing",
            "u-mgr",
            json!({"mode": "off"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["mode"], "off", "{b}");
    assert_eq!(b["updated_by"], "u-mgr", "{b}");
    let (conv_id, _) = queued(&c, "wc-1").await;
    nothing_assigned(&c, &conv_id).await;
    assert_eq!(tick(&c).await, 0, "the sweeper is off");
    assert_eq!(owner(&c, &conv_id).await, "queue");
    assert_eq!(
        c.texts("SELECT 1::text FROM audit_event WHERE action = 'support_routing.update' LIMIT 1")
            .await
            .len(),
        1
    );
    // back on: the waiting item is handed out
    let (s, b) = c
        .call(
            "PUT",
            "/api/support/routing",
            "u-mgr",
            json!({"mode": "round_robin"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    sweep_until_assigned(&c, &[&conv_id]).await;
    assert_eq!(owner(&c, &conv_id).await, "u-agent");
}

#[tokio::test]
async fn routing_put_is_refused_for_a_plain_support_agent() {
    let c = conv::setup().await;
    let (s, b) = c
        .call(
            "PUT",
            "/api/support/routing",
            "u-agent",
            json!({"mode": "off", "reassign_after_secs": 600}),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "support_manager", "{b}");
    // nothing was written
    let (s, b) = c.get("/api/support/routing", "u-agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["mode"], "round_robin", "{b}");
    assert_eq!(b["updated_by"], json!(null), "{b}");
}

#[tokio::test]
async fn routing_put_requires_manager_or_admin() {
    let c = conv::setup().await;
    // no row: the defaults, readable by any staff role
    let (s, b) = c.get("/api/support/routing", "u-agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b,
        json!({"mode": "round_robin", "reassign_after_secs": 300, "updated_at": null, "updated_by": null, "enabled": false})
    );
    // a member without a staff role may not read it
    let (s, b) = c.get("/api/support/routing", "u-obs").await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    // an agent may not change it
    let (s, b) = c
        .call(
            "PUT",
            "/api/support/routing",
            "u-agent",
            json!({"mode": "off"}),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{b}");
    assert_eq!(b["error"]["missing_role"], "support_manager", "{b}");
    // the manager and the tenant admin may
    let (s, b) = c
        .call(
            "PUT",
            "/api/support/routing",
            "u-mgr",
            json!({"reassign_after_secs": 900}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["mode"], "round_robin",
        "a field left out keeps its value: {b}"
    );
    assert_eq!(b["reassign_after_secs"], 900, "{b}");
    let (s, b) = c
        .call(
            "PUT",
            "/api/support/routing",
            "u-admin",
            json!({"mode": "off"}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["reassign_after_secs"], 900, "{b}");
    let (s, b) = c.get("/api/support/routing", "u-agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["mode"], "off", "{b}");
    assert_eq!(b["updated_by"], "u-admin", "{b}");
    // bad bodies
    for body in [
        json!({}),
        json!({"mode": "random"}),
        json!({"reassign_after_secs": 30}),
        json!({"reassign_after_secs": 86_401}),
    ] {
        let (s, b) = c
            .call("PUT", "/api/support/routing", "u-mgr", body.clone())
            .await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{body}: {b}");
        assert_eq!(code(&b), "invalid_routing", "{body}: {b}");
    }
}

#[tokio::test]
async fn worker_bound_job_is_not_auto_assigned() {
    let c = conv::setup().await;
    flag(&c, true).await;
    online(&c, "u-agent").await;
    // another surface writing later does not make the member offline (support_seen_at is kept)
    seen(&c, "u-agent", "admin", 0).await;
    let (before, _) = queued(&c, "wc-before").await;
    await_owner(&c, &before, "u-agent").await;
    // an enabled worker-side staff binding: the external tool routes, items stay queued
    sqlx::query(
        "INSERT INTO connector_bindings (tenant_id, job_id, kind, side, external_id, created_by, verified_at) \
         VALUES ($1, $2, 'helpdesk', 'worker', 'T-ws', 'test', now())",
    )
    .bind(c.t)
    .bind(c.job)
    .execute(&c.e.owner)
    .await
    .unwrap();
    let (conv_id, _) = queued(&c, "wc-bound").await;
    nothing_assigned(&c, &conv_id).await; // the trigger skips a worker-bound job
    assert_eq!(tick(&c).await, 0, "the sweeper skips it too");
    assert_eq!(owner(&c, &conv_id).await, "queue");
    // disabled binding: routing applies again
    sqlx::query("UPDATE connector_bindings SET enabled = false WHERE tenant_id = $1")
        .bind(c.t)
        .execute(&c.e.owner)
        .await
        .unwrap();
    sweep_until_assigned(&c, &[&conv_id]).await;
    assert_eq!(owner(&c, &conv_id).await, "u-agent");
}

#[tokio::test]
async fn runtime_pool_defers_routing_to_the_sweeper() {
    let c = conv::setup().await;
    flag(&c, true).await;
    // the runtime processes' state: the same app on the restricted `agent_runtime` pool (0065), which holds no
    // grant on the routing tables. Routing there is refused by the database (42501), which is a deferral.
    let mut rt = c.e.state.clone();
    rt.db = common::runtime_pool(&c.e.owner).await;
    let (conv_id, item) = queued(&c, "wc-1").await;
    nothing_assigned(&c, &conv_id).await;
    let err = routing::route_item(&rt, c.t, c.job, &conv_id, None, true)
        .await
        .unwrap_err();
    assert!(routing::is_denied(&err), "{err:?}");
    // the notify's best-effort trigger on that pool swallows it (logged at debug) and leaves the item queued
    routing::auto_assign_item(&rt, c.t, c.job, &item).await;
    assert_eq!(owner(&c, &conv_id).await, "queue");
    // a refusal that is not a missing grant is not a deferral
    assert!(!routing::is_denied(&agent_api::error::ApiError::Missing(
        "not_found",
        "x".into()
    )));
    // agent-api's sweeper then hands it out
    online(&c, "u-agent").await;
    sweep_until_assigned(&c, &[&conv_id]).await;
    assert_eq!(owner(&c, &conv_id).await, "u-agent");
}

/// The sweeper's real loop (`spawn_every`, interval one hour so the tick cannot be what assigns) is running with its
/// listener; a runtime-pool queue + NOTIFY then assigns the item within ~3 s.
#[tokio::test]
async fn runtime_queued_item_is_assigned_without_waiting_for_the_tick() {
    let c = conv::setup().await;
    flag(&c, true).await;
    let mut rt = c.e.state.clone();
    rt.db = common::runtime_pool(&c.e.owner).await;
    let sweeper = routing::spawn_every(c.e.state.clone(), Duration::from_secs(3600));
    // let the loop connect, LISTEN and run its one catch-up pass before anything is queued
    tokio::time::sleep(Duration::from_millis(750)).await;
    // nobody is online: the item queues and stays queued
    let (conv_id, item) = queued(&c, "wc-1").await;
    nothing_assigned(&c, &conv_id).await;
    online(&c, "u-agent").await;
    // the runtime side: routing is refused (42501) and it NOTIFYs agent-api's sweeper
    routing::auto_assign_item(&rt, c.t, c.job, &item).await;
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        let got = owner(&c, &conv_id).await;
        if got == "u-agent" {
            break;
        }
        assert!(
            Instant::now() < end,
            "{conv_id} is owned by {got:?}, expected u-agent within 3s of the notify"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(auto(&c, &item).await);
    sweeper.abort();
}

/// SW-7: flag OFF (the default) with `mode = round_robin` and someone online: nothing is assigned by the trigger, the
/// sweeper or the runtime NOTIFY path; GET reports `enabled: false`; the mode stays editable; switching the flag on
/// hands the waiting item out.
#[tokio::test]
async fn flag_off_means_no_auto_assignment_whatever_the_mode_says() {
    let c = conv::setup().await;
    online(&c, "u-agent").await;
    let (s, b) = c.get("/api/support/routing", "u-agent").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["mode"], "round_robin", "{b}");
    assert_eq!(b["enabled"], false, "{b}");
    // the trigger
    let (conv_id, item) = queued(&c, "wc-1").await;
    nothing_assigned(&c, &conv_id).await;
    // the sweeper
    assert_eq!(
        tick(&c).await,
        0,
        "the sweeper skips a tenant with the flag off"
    );
    assert_eq!(owner(&c, &conv_id).await, "queue");
    // the runtime NOTIFY path ends in route_item
    let out = routing::route_item(&c.e.state, c.t, c.job, &conv_id, None, true)
        .await
        .unwrap();
    assert_eq!(out, Outcome::Skipped);
    assert!(!auto(&c, &item).await);
    // the mode setting is still editable while the flag is off, and still reports the flag off
    let (s, b) = c
        .call(
            "PUT",
            "/api/support/routing",
            "u-mgr",
            json!({"reassign_after_secs": 600}),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["reassign_after_secs"], 600, "{b}");
    assert_eq!(b["enabled"], false, "{b}");
    assert_eq!(tick(&c).await, 0);
    // on: the waiting item is handed out and GET reports it
    flag(&c, true).await;
    sweep_until_assigned(&c, &[&conv_id]).await;
    assert_eq!(owner(&c, &conv_id).await, "u-agent");
    let (_, b) = c.get("/api/support/routing", "u-agent").await;
    assert_eq!(b["enabled"], true, "{b}");
    // off again: an AUTO assignment that went stale is not moved either
    flag(&c, false).await;
    online(&c, "u-agent2").await;
    age_assignment(&c, &item, 60).await;
    sqlx::query("DELETE FROM member_session WHERE tenant_id = $1 AND sub = 'u-agent'")
        .bind(c.t)
        .execute(&c.e.owner)
        .await
        .unwrap();
    assert_eq!(
        tick(&c).await,
        0,
        "flag off: the stale item stays with its assignee"
    );
    assert_eq!(owner(&c, &conv_id).await, "u-agent");
}
