//! Defense in depth for the identity floor of the operation contract: a catalogue row stored BELOW its contract level
//! (written before the authoring rule existed, or by SQL) is raised to the contract's at the runtime, so the runner
//! can never run `contact_point.update` below step-up (3) whatever the row says. The catalogue here is the contract
//! job's, with `contact_point.update` replaced by a row stored at level 2. Real Postgres 18 + Redis.
use std::sync::atomic::Ordering;

use agent_api::connectors::tenant_templates::op_defs;
use agent_api::runtime::catalog::{OperationCatalog, PinnedCatalog};
use agent_api::runtime::domain::OpState as ResultState;
use agent_model::catalog::OperationRow;
use agent_model::kinds::catalog::IdentityLevel;
use serde_json::json;

use crate::b4::step_up_contact::{OP, case_as_ann, change_call, put_contacts, world_with};

/// The `*` row of `contact_point.update` as an old catalogue stored it: the contract's columns, `identity_level` "2".
fn row_stored_at_level_2() -> OperationRow {
    let d = op_defs()
        .iter()
        .find(|o| o.object == "contact_point" && o.verb == "update" && o.variant == "*")
        .expect("contact_point.update is in the operation contract");
    serde_json::from_value(json!({
        "modality": "request", "is_derive": false, "side_effect": "W", "idempotency": "key+dup",
        "confirm_back": true, "verify_read": null, "builtin": false, "op_kind": "standard",
        "guard_rules": [], "identity_level": "2",
        "input_fields": d.input_fields, "output_fields": d.output_fields,
        "key": OP, "object": "contact_point", "verb": "update", "variant": "*",
        "discriminator": null, "overridden": [], "inherited": [], "description": "stored at level 2",
        "retail": null, "stages": [], "error_states": []
    }))
    .expect("an operation row")
}

fn use_the_old_row(design: &mut agent_api::runtime::design::RuntimeDesign) {
    design.catalog.put_row(&row_stored_at_level_2());
    assert_eq!(
        row_stored_at_level_2()
            .typed_attrs()
            .unwrap()
            .identity_level,
        Some(IdentityLevel::Verified),
        "the stored row says level 2"
    );
}

/// The catalogue raises the stored level-2 row to the contract's 3. (A2: the runner no longer refuses a level-2 customer
/// `identity_required` for it, step 3 requires conversation STATE, so the floor is asserted on the catalogue itself;
/// the former end-to-end step-up handover is deleted by design.) Mutation: `max()` dropped in `effective_level` (the
/// row's 2 wins).
#[tokio::test]
async fn a_level_2_row_for_a_level_3_contract_op_is_raised_to_the_contracts_level() {
    let cat = PinnedCatalog::from_rows(&[row_stored_at_level_2()]);
    assert_eq!(
        cat.get(OP).map(|o| o.identity_level),
        Some(IdentityLevel::StepUp)
    );
}

/// The same stored row still RUNS for a level-3 case: the floor raises the requirement to 3, it does not remove the
/// operation (the executor is reached once).
#[tokio::test]
async fn a_level_2_row_for_a_level_3_contract_op_still_runs_at_level_3() {
    let w = world_with("level-floor-l3", use_the_old_row).await;
    let fx = &w.fx;
    let msg = case_as_ann(fx, "CS-3", "conv-3", 3).await;
    put_contacts(fx, "CS-3").await;
    let res = w
        .runner
        .handle(&change_call(fx, "CS-3", "conv-3", 3, &msg))
        .await
        .unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(w.contact.hits.load(Ordering::SeqCst), 1);
}
