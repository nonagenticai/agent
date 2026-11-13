//! What a bundle requires and what keeps it from being approved, derived from the compiled bundle itself: no
//! per-industry data and no compiled-in seed (D128). Pure functions over the [`BundleObjects`] the layer compile
//! produced and the selection, so any industry compiles the same way.
//!
//! Rules (each one is a unit test below):
//! - slots: `strata_rag.embed` and `strata_rag.chat` always; `strata_rag.rerank` when the job uses knowledge search
//!   (an activity expects `knowledge.search`, or the job has a knowledge store); `strata_ingest.ocr` and
//!   `strata_ingest.vision` when a selected channel accepts attachments ([`ATTACHMENT_CHANNELS`]);
//! - routes (Production): the distinct operations of the folded activities, `<op>[{variant}]@prod`, sorted;
//! - test routes: `case.read@test` for each selection with an attachment channel (its regression run looks a case up);
//! - causes: the attachment requirements name the channel that introduced them;
//! - violations: the draft job pin and its stage-less journey, "Needs at least one area" (only when the compiled
//!   context spaces carry areas) and "Pick at least one channel".
use std::collections::{BTreeMap, BTreeSet};

use agent_model::compile::Selection;
use serde_json::Value;

use super::bundle::{BundleObjects, BundleViolation, OverlayItem, Requires};
use crate::platform::templates::TemplatePin;

/// The channels whose customers send photos or files (SW-9 attachments): they need the ingest slots.
pub const ATTACHMENT_CHANNELS: [&str; 1] = ["web_chat"];

/// The stage an activity must exist in for a journey to be complete.
pub const EXIT_STAGE: &str = "exit";

/// The operation a job calls to search knowledge.
pub const KNOWLEDGE_SEARCH_OP: &str = "knowledge.search";

const EMBED: &str = "strata_rag.embed";
const RERANK: &str = "strata_rag.rerank";
const CHAT: &str = "strata_rag.chat";
const OCR: &str = "strata_ingest.ocr";
const VISION: &str = "strata_ingest.vision";
const TEST_CASE_READ: &str = "case.read@test";

/// What [`derive`] returns.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Derived {
    pub requires: Requires,
    pub causes: BTreeMap<String, String>,
    pub violations: Vec<BundleViolation>,
}

/// The first selected channel that accepts attachments.
fn attachment_channel(sel: &Selection) -> Option<&str> {
    sel.dimensions
        .channel
        .iter()
        .map(String::as_str)
        .find(|c| ATTACHMENT_CHANNELS.contains(c))
}

/// `(op, variant)` of every operation the activity items name (`ops` as [`super::compile::activity_items`] writes it).
fn operations(activities: &[OverlayItem]) -> Vec<(String, Option<String>)> {
    activities
        .iter()
        .filter_map(|a| a.get("ops").and_then(Value::as_array))
        .flatten()
        .filter_map(|o| {
            let op = o.get("op")?.as_str()?.to_owned();
            let variant = o.get("variant").and_then(Value::as_str).map(str::to_owned);
            Some((op, variant))
        })
        .collect()
}

fn uses_knowledge_search(objects: &BundleObjects) -> bool {
    !objects.knowledge_store.is_empty()
        || operations(&objects.activity)
            .iter()
            .any(|(op, _)| op == KNOWLEDGE_SEARCH_OP)
}

fn violation(path: &str, message: &str) -> BundleViolation {
    BundleViolation {
        path: path.to_owned(),
        message: message.to_owned(),
    }
}

/// The violations of a job pinned at a draft (v0): the pin itself, and a journey whose activities leave the exit
/// stage empty (only when some other stage has activities, so an empty job is not told about one stage).
fn draft_violations(pin: &TemplatePin, activities: &[OverlayItem]) -> Vec<BundleViolation> {
    let mut out = vec![violation(
        &format!("pins.{}", pin.layer),
        "v0 is a draft; a release needs a tagged job template.",
    )];
    let stage_of = |a: &OverlayItem| a.get("stage").and_then(Value::as_str).map(str::to_owned);
    let stages: BTreeSet<String> = activities.iter().filter_map(stage_of).collect();
    if !stages.is_empty() && !stages.contains(EXIT_STAGE) {
        out.push(violation(
            "journey.stages.exit.activities",
            "Stage Exit has no activity.",
        ));
    }
    out
}

/// Whether the compiled context spaces carry areas (a job without areas is not asked to select one).
fn has_areas(objects: &BundleObjects) -> bool {
    objects.context_space.iter().any(|s| {
        s.get("area")
            .and_then(Value::as_str)
            .is_some_and(|a| !a.is_empty())
    })
}

/// Derive `requires`, `causes` and the cut violations of a compiled bundle's `objects` for `sel` over `pins`.
pub fn derive(pins: &[TemplatePin], sel: &Selection, objects: &BundleObjects) -> Derived {
    let mut d = Derived::default();
    let attach = attachment_channel(sel);

    d.requires.slots.push(EMBED.to_owned());
    if uses_knowledge_search(objects) {
        d.requires.slots.push(RERANK.to_owned());
    }
    d.requires.slots.push(CHAT.to_owned());
    if let Some(channel) = attach {
        for slot in [OCR, VISION] {
            d.requires.slots.push(slot.to_owned());
            d.causes.insert(
                slot.to_owned(),
                format!("channel {channel} (customers send attachments)"),
            );
        }
        d.requires.test_routes.push(TEST_CASE_READ.to_owned());
        d.causes.insert(
            TEST_CASE_READ.to_owned(),
            format!("channel {channel} (case look-up in its regression run)"),
        );
    }

    let routes: BTreeSet<String> = operations(&objects.activity)
        .into_iter()
        .map(|(op, variant)| match variant {
            Some(v) => format!("{op}{{{v}}}@prod"),
            None => format!("{op}@prod"),
        })
        .collect();
    d.requires.routes = routes.into_iter().collect();

    if let Some(p) = pins
        .iter()
        .find(|p| p.layer.starts_with("job/") && p.version == 0)
    {
        d.violations.extend(draft_violations(p, &objects.activity));
    }
    if sel.areas.is_empty() && has_areas(objects) {
        d.violations
            .push(violation("context_spaces", "Needs at least one area."));
    }
    if sel.dimensions.channel.is_empty() {
        d.violations
            .push(violation("channels", "Pick at least one channel."));
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(v: Value) -> OverlayItem {
        v.as_object().cloned().expect("an object")
    }

    fn sel(areas: &[&str], channels: &[&str]) -> Selection {
        let mut s = Selection {
            areas: areas.iter().map(|a| (*a).to_owned()).collect(),
            ..Default::default()
        };
        s.dimensions.channel = channels.iter().map(|c| (*c).to_owned()).collect();
        s
    }

    fn pin(layer: &str, version: i32) -> TemplatePin {
        TemplatePin {
            layer: layer.to_owned(),
            version,
        }
    }

    fn objects(ops: Value) -> BundleObjects {
        BundleObjects {
            activity: vec![item(
                json!({"key": "support:a", "stage": "support", "ops": ops}),
            )],
            ..BundleObjects::default()
        }
    }

    #[test]
    fn an_empty_job_needs_embed_and_chat_only() {
        let d = derive(&[], &sel(&[], &["chat"]), &BundleObjects::default());
        assert_eq!(d.requires.slots, [EMBED, CHAT]);
        assert!(d.requires.routes.is_empty() && d.requires.test_routes.is_empty());
        assert!(d.causes.is_empty() && d.violations.is_empty());
    }

    #[test]
    fn rerank_follows_knowledge_search_or_a_knowledge_store() {
        let o = objects(json!([{"op": "knowledge.search", "variant": null, "required": true}]));
        assert_eq!(
            derive(&[], &sel(&[], &["chat"]), &o).requires.slots,
            [EMBED, RERANK, CHAT]
        );
        let mut o = BundleObjects::default();
        o.knowledge_store.push(item(json!({"key": "kb"})));
        assert!(
            derive(&[], &sel(&[], &["chat"]), &o)
                .requires
                .slots
                .contains(&RERANK.to_owned())
        );
    }

    #[test]
    fn routes_are_the_distinct_sorted_operations_with_their_variant() {
        let o = objects(json!([
            {"op": "case.read", "variant": null, "required": true},
            {"op": "case.create", "variant": "return", "required": true},
            {"op": "case.read", "variant": null, "required": false}
        ]));
        let d = derive(&[], &sel(&[], &["chat"]), &o);
        assert_eq!(
            d.requires.routes,
            ["case.create{return}@prod", "case.read@prod"]
        );
    }

    #[test]
    fn an_attachment_channel_adds_ingest_slots_a_test_route_and_its_cause() {
        let d = derive(
            &[],
            &sel(&[], &["chat", "web_chat"]),
            &BundleObjects::default(),
        );
        assert_eq!(d.requires.slots, [EMBED, CHAT, OCR, VISION]);
        assert_eq!(d.requires.test_routes, [TEST_CASE_READ]);
        assert_eq!(d.causes.len(), 3);
        assert!(d.causes[VISION].starts_with("channel web_chat"));
        let plain = derive(&[], &sel(&[], &["chat"]), &BundleObjects::default());
        assert!(plain.causes.is_empty());
    }

    #[test]
    fn no_channel_is_a_violation() {
        let d = derive(&[], &sel(&[], &[]), &BundleObjects::default());
        assert_eq!(
            d.violations,
            [violation("channels", "Pick at least one channel.")]
        );
    }

    #[test]
    fn no_area_is_a_violation_only_when_the_job_has_areas() {
        let mut o = BundleObjects::default();
        assert!(derive(&[], &sel(&[], &["chat"]), &o).violations.is_empty());
        o.context_space
            .push(item(json!({"key": "s", "area": "order-support"})));
        let d = derive(&[], &sel(&[], &["chat"]), &o);
        assert_eq!(
            d.violations,
            [violation("context_spaces", "Needs at least one area.")]
        );
        assert!(
            derive(&[], &sel(&["order-support"], &["chat"]), &o)
                .violations
                .is_empty()
        );
    }

    #[test]
    fn a_draft_job_pin_is_a_violation_and_exit_is_derived_from_the_activities() {
        let pins = [pin("base", 3), pin("job/x", 0)];
        let d = derive(&pins, &sel(&[], &["chat"]), &objects(json!([])));
        assert_eq!(
            d.violations,
            [
                violation(
                    "pins.job/x",
                    "v0 is a draft; a release needs a tagged job template."
                ),
                violation(
                    "journey.stages.exit.activities",
                    "Stage Exit has no activity."
                ),
            ]
        );
        let mut o = objects(json!([]));
        o.activity
            .push(item(json!({"key": "exit:close", "stage": "exit"})));
        let d = derive(&pins, &sel(&[], &["chat"]), &o);
        assert_eq!(d.violations.len(), 1);
        assert!(
            derive(&[pin("job/x", 2)], &sel(&[], &["chat"]), &o)
                .violations
                .is_empty()
        );
    }
}
