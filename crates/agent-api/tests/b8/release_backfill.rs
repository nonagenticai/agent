//! B8 R2, OWNER DECISION 2026-10-26 (decision 1): a release carries its own activity operations. A release cut BEFORE
//! seed v8 (legacy activity keys, no `ops`, pre-v8 pins whose fold names none) would lose every activity -> operation
//! grant and every space -> activity link now that the KH fold is the only source; migration 0194 backfills them
//! from the old `activity_ops.json` into the stored bundle (the one approved exception to D62) and recomputes
//! `bundle_hash`. New cuts are self-contained (`release::compile::activity_items` writes `ops`, `required`).
//! Real Postgres 18; the migration's SQL is run by hand over a pre-v8-shaped release (the test database was migrated
//! before the row existed), as `tests/b8/kinds.rs` does for 0191's CHECK.
use agent_api::design::DesignOf;
use agent_api::job_design::base::{self, Bases};
use agent_api::job_design::gates::hooks::{NOT_ENTITLED, ToolPoint};
use agent_api::release::hash::bundle_hash;
use agent_api::runtime::design::RuntimeDesign;
use agent_api::runtime::domain::OpCall;
use agent_api::runtime::runner::authorise::ToolGate;
use serde_json::{Value, json};

use crate::common::design_contract::{self, ContractJob};
use crate::common::{self, TestEnv};

const MIGRATION: &str = include_str!("../../migrations/0194_backfill_release_activity_ops.sql");

/// A legacy activity the old file granted (job/customer_care_agent): `party.list`, `credential.verify`,
/// `order.read` (required) and `order.cancel` (optional).
const CANCEL: &str = "change:cancel_order";
/// `delivery:track_parcel`: `delivery.read{by_tracking_ref}`, `order.read`, `knowledge.search` (all required);
/// the old `spaces` mapped `retail:logistics:track_parcel` to it.
const TRACK: &str = "delivery:track_parcel";
const TRACK_SPACE: &str = "retail:logistics:track_parcel";

async fn bases_of(e: &TestEnv, f: &ContractJob, id: &str) -> std::sync::Arc<Bases> {
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let b = base::of_release(&mut tx, &f.job_ref(), id)
        .await
        .unwrap_or_else(|err| panic!("of_release {id}: {err:?}"))
        .expect("the release exists");
    tx.commit().await.unwrap();
    b
}

async fn bundle_of(e: &TestEnv, f: &ContractJob, id: &str) -> (Value, String) {
    sqlx::query_as(
        "SELECT bundle, bundle_hash FROM job_release WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(id)
    .fetch_one(&e.owner)
    .await
    .unwrap()
}

async fn run_migration(e: &TestEnv) {
    sqlx::raw_sql(MIGRATION)
        .execute(&e.owner)
        .await
        .unwrap_or_else(|err| panic!("0194: {err}"));
}

fn call(op: &str, activity: &str) -> OpCall {
    serde_json::from_value(json!({
        "call_id": "opc_00000000-0000-0000-0000-000000000001", "case_id": "CS-BF", "conversation_id": "c-bf",
        "phase": 0, "step": "s", "attempt": 1, "agent_role": "procedure", "activity": activity,
        "operation": op, "env": "test", "initiator": "procedure@bf", "deadline_ms": 0, "input": {},
    }))
    .unwrap()
}

/// The release row as a pre-v8 cut left it: legacy activity items without `ops`, context spaces without `activity`,
/// `bundle_hash` consistent with the bytes (so the migration may touch it). Owner-level, trigger off for the write.
async fn make_pre_v8(e: &TestEnv, f: &ContractJob, id: &str) {
    let (mut bundle, _) = bundle_of(e, f, id).await;
    let activity = |key: &str, stage: &str, label: &str| json!({"key": key, "stage": stage, "label": label, "actor": "customer", "required": false});
    bundle["objects"]["activity"] = json!([
        activity(CANCEL, "change", "Cancel an order"),
        activity(TRACK, "delivery", "Track a parcel"),
        activity("exit:close_account", "exit", "Close my account"),
        activity("zzz:not_in_the_old_file", "support", "Not in the old file"),
    ]);
    let spaces = bundle["objects"]["context_space"]
        .as_array_mut()
        .expect("the bundle has context spaces");
    // the contract job selects the `order-support` area only; an older cut that also selected `logistics` held this
    if !spaces.iter().any(|s| s["key"] == TRACK_SPACE) {
        spaces.push(json!({
            "key": TRACK_SPACE, "label": "Where is my parcel", "required": ["tracking_ref"], "entities": [],
        }));
    }
    for s in spaces.iter_mut() {
        s.as_object_mut().unwrap().remove("activity");
    }
    let hash = bundle_hash(&bundle).unwrap();
    let mut tx = e.owner.begin().await.unwrap();
    sqlx::query("ALTER TABLE job_release DISABLE TRIGGER job_release_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE job_release SET bundle = $3, bundle_hash = $4 WHERE job_id = $1 AND release_id = $2",
    )
    .bind(f.job.job_id)
    .bind(id)
    .bind(&bundle)
    .bind(&hash)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("ALTER TABLE job_release ENABLE TRIGGER job_release_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

fn ops_of(b: &Bases, key: &str) -> Vec<(String, Option<String>, bool)> {
    b.activity(key)
        .unwrap_or_else(|| panic!("the release has {key}"))
        .ops
        .iter()
        .map(|o| (o.op.clone(), o.variant.clone(), o.required))
        .collect()
}

fn op(o: &str, v: Option<&str>, required: bool) -> (String, Option<String>, bool) {
    (o.to_owned(), v.map(str::to_owned), required)
}

#[tokio::test]
async fn migration_0194_gives_a_pre_v8_release_its_old_grants_and_keeps_its_hash_valid() {
    let e = common::env().await;
    let f = crate::b8::support::new_job(&e).await;
    let id = design_contract::publish(&e, &f).await;
    make_pre_v8(&e, &f, &id).await;

    // before: the legacy items carry no grant and the pre-v8 pins' fold gives none (the loss the owner decided on)
    let before = bases_of(&e, &f, &id).await;
    // (the test platform's pins fold this activity's own operations; what only the old file granted is missing)
    assert!(
        !ops_of(&before, CANCEL)
            .iter()
            .any(|o| o.0 == "party.list" || o.0 == "credential.verify"),
        "no legacy grant before 0194"
    );
    assert!(ops_of(&before, TRACK).is_empty());
    assert_eq!(before.space_activity(TRACK_SPACE), None);

    run_migration(&e).await;

    let (bundle, stored_hash) = bundle_of(&e, &f, &id).await;
    // bundle_hash is the Rust canonical hash of the new bytes (release::apply refuses a bundle that does not hash)
    assert_eq!(stored_hash, bundle_hash(&bundle).unwrap());
    let items = bundle["objects"]["activity"].as_array().unwrap();
    let item = |k: &str| items.iter().find(|i| i["key"] == k).unwrap();
    assert_eq!(item(CANCEL)["ops"][3]["op"], "order.cancel");
    assert!(
        item("zzz:not_in_the_old_file").get("ops").is_none(),
        "an activity the old file never knew gets nothing"
    );
    assert_eq!(
        item(CANCEL)["required"],
        false,
        "the other fields are untouched"
    );

    let after = bases_of(&e, &f, &id).await;
    assert_eq!(
        ops_of(&after, CANCEL),
        [
            op("party.list", None, true),
            op("credential.verify", None, true),
            op("order.read", None, true),
            op("order.cancel", None, false),
        ]
    );
    assert_eq!(
        ops_of(&after, TRACK),
        [
            op("delivery.read", Some("by_tracking_ref"), true),
            op("order.read", None, true),
            op("knowledge.search", None, true),
        ]
    );
    assert!(ops_of(&after, "zzz:not_in_the_old_file").is_empty());
    // the old `spaces` mapping, read from the bundle's context_space item (the compile at the pre-v8 pins has none)
    let space = bundle["objects"]["context_space"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == TRACK_SPACE)
        .expect("the bundle holds the track_parcel space");
    assert_eq!(space["activity"], TRACK);
    assert_eq!(after.space_activity(TRACK_SPACE).as_deref(), Some(TRACK));

    // the runtime's tool gate allows a legacy-granted operation and still refuses one outside the grant
    // the runtime reads the job's Live design, so the pre-v8 release is the job's current one
    sqlx::query("UPDATE jobs SET current_release_id = $2 WHERE id = $1")
        .bind(f.job.job_id)
        .bind(&id)
        .execute(&e.owner)
        .await
        .unwrap();
    let d = {
        let mut tx = f.job.begin(&e.state.db).await.unwrap();
        let d = RuntimeDesign::load(&mut tx, &e.state.design, &f.job_ref(), &DesignOf::Live)
            .await
            .unwrap_or_else(|err| panic!("load: {err:?}"));
        tx.commit().await.unwrap();
        d
    };
    assert!(
        d.job_design
            .toolsets
            .activity_grant(TRACK)
            .iter()
            .any(|k| k.as_str() == "order.read"),
        "the release's toolsets grant the legacy activity's operations"
    );
    assert_eq!(
        ToolPoint
            .check(
                &e.state,
                f.tenant,
                &d,
                &call("order.read", TRACK),
                "order.read"
            )
            .await,
        Ok(())
    );
    assert_eq!(
        ToolPoint
            .check(
                &e.state,
                f.tenant,
                &d,
                &call("order.cancel", TRACK),
                "order.cancel"
            )
            .await,
        Err(NOT_ENTITLED.to_owned()),
        "the control: an operation outside the activity's grant"
    );

    // idempotent: a second run changes nothing
    run_migration(&e).await;
    assert_eq!(bundle_of(&e, &f, &id).await, (bundle, stored_hash));
}

#[tokio::test]
async fn a_new_cut_carries_its_ops_required_and_space_activity() {
    let e = common::env().await;
    let f = crate::b8::support::new_job(&e).await;
    let id = design_contract::publish(&e, &f).await;
    let (bundle, stored_hash) = bundle_of(&e, &f, &id).await;
    assert_eq!(stored_hash, bundle_hash(&bundle).unwrap());

    let items = bundle["objects"]["activity"].as_array().unwrap();
    let item = |k: &str| {
        items
            .iter()
            .find(|i| i["key"] == k)
            .unwrap_or_else(|| panic!("{k}"))
    };
    let track = item("delivery:where_is_my_order");
    let ops = track["ops"].as_array().expect("a cut carries ops");
    assert!(
        ops.iter()
            .any(|o| o["op"] == "delivery.read" && o["variant"] == "by_tracking_ref"),
        "{ops:?}"
    );
    assert!(
        ops.iter()
            .all(|o| o.get("required").is_some() && o.get("variant").is_some())
    );
    // owner 2026-10-26 (decision 2): the template's required flag lands in the bundle item
    assert_eq!(track["required"], true);
    assert_eq!(item("support:return")["required"], true);
    assert_eq!(item("change:cancel_order")["required"], false);
    // the contract selection holds the `order-support` area (not `logistics`): its order-status space answers the
    // v8 where-is-my-order activity
    let space = bundle["objects"]["context_space"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == "retail:order-support:order_status")
        .expect("the cut holds the order_status space");
    assert_eq!(space["activity"], "delivery:where_is_my_order");

    // the release's bases read them from the bundle: the same grants as the item
    let b = bases_of(&e, &f, &id).await;
    let got = ops_of(&b, "delivery:where_is_my_order");
    let want: Vec<_> = ops
        .iter()
        .map(|o| {
            op(
                o["op"].as_str().unwrap(),
                o["variant"].as_str(),
                o["required"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(got, want);

    // 0194 leaves a v8 release alone (items already have ops / activity)
    run_migration(&e).await;
    assert_eq!(bundle_of(&e, &f, &id).await, (bundle, stored_hash));
}
