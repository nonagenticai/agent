//! The release bundle, schema `agent.bundle/1` (spec B2 §5): what a cut compiles from the job's selection and
//! template pins. B2 reads only the parts below; B6 may add sections (so stored bundles are read leniently).
//!
//! - `sections`: display, counts and compare (fixture `bundleFor`).
//! - `objects`: the overlay bases of the refinable objects (T8 refinements, the conflict detector); since PK-2 also
//!   `knowledge_pack` (the template's pack references per seeded knowledge kind), serialised only when non-empty
//!   (PK-3), so every bundle without packs hashes as before; since KH T4 (D108) also `instruction`, the folded
//!   prompt slots of the release snapshot ([`Bundle::instructions`]), likewise absent when empty.
//! - `settings`: the job-setting bases (`preamble`, `actor`, `stance`); `preamble` is the folded `identity`
//!   instruction (D108).
//! - `rules`: the business-rule slots (fixture `BusinessRule` minus every tenant field).
//! - `requires` / `causes`: what the release needs from the tenant's environment and which part of the selection
//!   introduced it (fixture `requiresFor` / `requirementCauses`).
//! - `profile_values`: the profile's values, frozen into the release (fixture `PROFILE_VALUES`).
use std::collections::BTreeMap;

use agent_model::kinds::instruction::FoldedInstruction;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::hash::{HashError, bundle_hash};

/// The bundle schema id, stored in every bundle as `format`.
pub const BUNDLE_FORMAT: &str = "agent.bundle/1";

/// The refinable objects with an overlay base in `objects` (fixture `RefinableObject` minus `job_setting`, plus
/// PK-2's `knowledge_pack`, which takes tombstones only, and D108's `instruction`, whose tenant rows may touch the
/// slots `tone`, `escalation` and `format` only).
pub const OVERLAY_OBJECTS: [&str; 7] = [
    "activity",
    "context_space",
    "ontology_attribute",
    "gate_option",
    "knowledge_store",
    "knowledge_pack",
    "instruction",
];

/// One overlay base item: a JSON object with at least `key` (fixture `OverlayItem`).
pub type OverlayItem = Map<String, Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bundle {
    /// [`BUNDLE_FORMAT`].
    pub format: String,
    pub sections: Vec<BundleSection>,
    pub objects: BundleObjects,
    pub settings: BundleSettings,
    pub rules: Vec<RuleSlot>,
    pub requires: Requires,
    /// Requirement (a slot or route of `requires`) → the part of the selection that introduced it; absent = the
    /// template itself.
    pub causes: BTreeMap<String, String>,
    pub profile_values: BTreeMap<String, String>,
}

impl Bundle {
    /// `bundle_hash` of this bundle (`"sha256:" + hex(SHA-256(canonical_json(bundle)))`). T12 recomputes it on
    /// apply and compares it with the stored `job_release.bundle_hash`.
    pub fn hash(&self) -> Result<String, HashError> {
        bundle_hash(&serde_json::to_value(self)?)
    }

    /// Section `key`'s items (empty when the bundle has no such section).
    pub fn section(&self, key: &str) -> &[String] {
        self.sections
            .iter()
            .find(|s| s.key == key)
            .map(|s| s.items.as_slice())
            .unwrap_or_default()
    }

    /// The release snapshot's folded instruction slots (D108), in slot order: per slot its text and the sections
    /// it folded from, each with its layer and pinned tag (`[layer <key>@<version>]`,
    /// `InstructionSection::marker`). Tenant refinements are not applied here.
    pub fn instructions(&self) -> Vec<FoldedInstruction> {
        instructions_of(&self.objects.instruction)
    }
}

/// The part of a base item of `object` a tenant row is made against (its `base_hash` covers this projection, and so
/// does the conflict detector's): the item itself, except an `instruction`, which is `{key, text}` — the folded
/// text, not the sections' layers and tags, so re-tagging a contributing layer with the same text is no change —
/// and an `activity`, which is the item without its `ops`.
pub fn base_projection(object: &str, item: &OverlayItem) -> OverlayItem {
    match object {
        "instruction" => item
            .iter()
            .filter(|(k, _)| matches!(k.as_str(), "key" | "text"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        // an activity's `ops` are the engine's grants (owner 2026-10-26: a release carries them), not a field a
        // tenant refines: they are no part of what a tenant row is made against, so backfilling them into a
        // release changes no `base_hash`
        "activity" if item.contains_key("ops") => item
            .iter()
            .filter(|(k, _)| k.as_str() != "ops")
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        _ => item.clone(),
    }
}

/// The folded instructions an `objects.instruction` list holds (an item that does not read as one is skipped).
pub fn instructions_of(items: &[OverlayItem]) -> Vec<FoldedInstruction> {
    items
        .iter()
        .filter_map(|i| serde_json::from_value(Value::Object(i.clone())).ok())
        .collect()
}

/// The `objects.instruction` items of a folded instruction set (`{key, text, sections}`).
pub fn instruction_items(set: &[FoldedInstruction]) -> Vec<OverlayItem> {
    set.iter()
        .filter_map(|i| match serde_json::to_value(i) {
            Ok(Value::Object(m)) => Some(m),
            _ => None,
        })
        .collect()
}

/// One display section (fixture `BundleSection`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BundleSection {
    pub key: String,
    pub label: String,
    pub items: Vec<String>,
}

/// The overlay bases, one list per refinable object, each in the compiler's order.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct BundleObjects {
    pub activity: Vec<OverlayItem>,
    pub context_space: Vec<OverlayItem>,
    pub ontology_attribute: Vec<OverlayItem>,
    pub gate_option: Vec<OverlayItem>,
    pub knowledge_store: Vec<OverlayItem>,
    /// PK-2: one item `{key: "<stage>|<kind>|<pack_key>", label, pin: "<pack_key>@<version>"}` per pack the template
    /// gives a seeded knowledge kind. PK-3: absent when empty, so older bundles keep their hash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub knowledge_pack: Vec<OverlayItem>,
    /// KH T4 (D108): one item `{key: <slot>, text, sections: [{layer, tag?, text, language?}]}` per folded
    /// `instruction` slot, in slot order. Absent when empty, so a bundle without instructions keeps its hash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub instruction: Vec<OverlayItem>,
}

impl BundleObjects {
    /// The base of `object` (one of [`OVERLAY_OBJECTS`]); `None` for any other name.
    pub fn get(&self, object: &str) -> Option<&[OverlayItem]> {
        let items = match object {
            "activity" => &self.activity,
            "context_space" => &self.context_space,
            "ontology_attribute" => &self.ontology_attribute,
            "gate_option" => &self.gate_option,
            "knowledge_store" => &self.knowledge_store,
            "knowledge_pack" => &self.knowledge_pack,
            "instruction" => &self.instruction,
            _ => return None,
        };
        Some(items.as_slice())
    }
}

/// The job-setting bases (fixture `SETTING_KEYS`); an absent key has no base value.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BundleSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preamble: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stance: Option<String>,
}

impl BundleSettings {
    /// The base of setting `key` (`preamble`, `actor`, `stance`).
    pub fn get(&self, key: &str) -> Option<&str> {
        match key {
            "preamble" => self.preamble.as_deref(),
            "actor" => self.actor.as_deref(),
            "stance" => self.stance.as_deref(),
            _ => None,
        }
    }
}

/// A business-rule slot as the template defines it: the fixture `BusinessRule` without the tenant's values,
/// passage, placeholder flag, history and decisions (T9 merges the tenant's `business_rule` row over it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleSlot {
    pub id: String,
    pub label: String,
    /// `object.verb{variant}` it guards.
    pub applies_to: Vec<String>,
    #[serde(default)]
    pub inputs: Vec<String>,
    pub outcome: String,
    /// The rule in one sentence, `{key}` = a value slot.
    pub statement: Option<String>,
    /// The template layer that defines the slot (`industry/retail@v2`).
    pub slot_layer: Option<String>,
    #[serde(default)]
    pub values: Vec<RuleValueSlot>,
    #[serde(default)]
    pub uses: Vec<RuleUse>,
    #[serde(default)]
    pub outcomes: Vec<RuleOutcomeDef>,
    #[serde(default)]
    pub facts: Vec<RuleFact>,
    #[serde(default)]
    pub examples: Vec<RuleExample>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<agent_model::rules::RuleProgram>,
}

/// One value slot of a rule (fixture `RuleValue` without the tenant's `value`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleValueSlot {
    pub key: String,
    pub label: String,
    pub unit: String,
    #[serde(default)]
    pub hint: String,
    /// What the template suggests; never applied by itself.
    pub suggested: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleUse {
    pub activity: String,
    pub activity_label: String,
    pub procedure: Option<String>,
    pub step: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleOutcomeDef {
    /// `eligible` | `ineligible:RET-01` | `escalate`.
    pub code: String,
    pub label: String,
    pub agent_does: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleFact {
    pub key: String,
    pub label: String,
    /// `date` | `text` | `choice` | `number` | `bool`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleExample {
    pub label: String,
    pub facts: BTreeMap<String, String>,
}

/// What a release needs from the tenant's environment (fixture `requiresFor`): W5 slots, the Production routes
/// apply checks, and the Test-env routes its test runs need (never blocking a publish).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Requires {
    pub slots: Vec<String>,
    pub routes: Vec<String>,
    #[serde(default)]
    pub test_routes: Vec<String>,
}

/// One problem of a bundle or of a compile (`{path, message}`), for 422 `bundle_incomplete` (`violations`) and
/// 422 `compile_failed` (`errors`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleViolation {
    pub path: String,
    pub message: String,
}

// ---- selection options (D65; fixture `SelectionOptions`) ---------------------------------------------------------

/// One option a selection may name (fixture `SelectionChoice`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionChoice {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Industry departments: the base department archetype it specialises (D60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,
    /// The industry this option belongs to (profiles, industry departments, areas).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub industry: Option<String>,
    /// Areas only: the department layer key (`industry_dept/<industry>/<dept>`) that groups the area.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub industry_dept: Option<String>,
}

/// A selection dimension and its values (fixture `SelectionDimension`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionDimension {
    pub key: String,
    pub label: String,
    pub values: Vec<SelectionChoice>,
    /// `dimension` (selects layers) | `placeholder` (recorded only, D59).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionOptionDimensions {
    pub channel: SelectionDimension,
    pub region: SelectionDimension,
}

/// What a selection may name (fixture `SelectionOptions`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionOptions {
    pub profiles: Vec<SelectionChoice>,
    pub industries: Vec<SelectionChoice>,
    /// D115: the departments, keyed by their layer key (`industry_dept/<industry>/<dept>`).
    pub departments: Vec<SelectionChoice>,
    pub areas: Vec<SelectionChoice>,
    pub dimensions: SelectionOptionDimensions,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Bundle {
        Bundle {
            format: BUNDLE_FORMAT.to_owned(),
            sections: vec![BundleSection {
                key: "channels".into(),
                label: "Channels".into(),
                items: vec!["telegram".into()],
            }],
            objects: BundleObjects::default(),
            settings: BundleSettings {
                actor: Some("care".into()),
                ..BundleSettings::default()
            },
            rules: Vec::new(),
            requires: Requires::default(),
            causes: BTreeMap::new(),
            profile_values: BTreeMap::new(),
        }
    }

    #[test]
    fn absent_settings_are_not_serialised_and_the_hash_covers_the_bundle() {
        let b = sample();
        let v = serde_json::to_value(&b).unwrap();
        assert_eq!(v["settings"], json!({"actor": "care"}));
        assert_eq!(b.hash().unwrap(), bundle_hash(&v).unwrap());
        let mut c = b.clone();
        c.sections[0].items.push("telegram".into());
        assert_ne!(b.hash().unwrap(), c.hash().unwrap());
        assert_eq!(b.section("channels"), ["telegram".to_owned()]);
        assert!(b.section("nope").is_empty());
        assert_eq!(b.settings.get("actor"), Some("care"));
        assert_eq!(b.settings.get("preamble"), None);
        assert!(b.objects.get("gate_option").is_some());
        assert!(b.objects.get("job_setting").is_none());
    }

    #[test]
    fn an_empty_knowledge_pack_is_not_serialised() {
        let b = sample();
        let v = serde_json::to_value(&b).unwrap();
        assert!(v["objects"].get("knowledge_pack").is_none(), "{v}");
        // a stored bundle without the field reads back to the same hash
        let back: Bundle = serde_json::from_value(v.clone()).unwrap();
        assert_eq!(back.hash().unwrap(), bundle_hash(&v).unwrap());
        let mut c = b.clone();
        let mut item = OverlayItem::new();
        item.insert("key".into(), json!("support|faq|base"));
        c.objects.knowledge_pack.push(item);
        let cv = serde_json::to_value(&c).unwrap();
        assert_eq!(
            cv["objects"]["knowledge_pack"][0]["key"],
            "support|faq|base"
        );
        assert_ne!(c.hash().unwrap(), b.hash().unwrap());
        assert_eq!(c.objects.get("knowledge_pack").map(<[_]>::len), Some(1));
    }

    #[test]
    fn instructions_are_absent_when_empty_and_read_back() {
        let b = sample();
        let v = serde_json::to_value(&b).unwrap();
        assert!(v["objects"].get("instruction").is_none(), "{v}");
        assert!(b.instructions().is_empty());
        let mut c = b.clone();
        c.objects.instruction = vec![
            serde_json::from_value(json!({
                "key": "identity",
                "text": "You help X.",
                "sections": [{"layer": "job/x", "tag": "job/x@v2", "text": "You help X."}],
            }))
            .unwrap(),
        ];
        assert_ne!(c.hash().unwrap(), b.hash().unwrap());
        let set = c.instructions();
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].sections[0].marker(), "[layer job/x@v2]");
        assert_eq!(instruction_items(&set), c.objects.instruction);
        assert_eq!(c.objects.get("instruction").map(<[_]>::len), Some(1));
    }
}
