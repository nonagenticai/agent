//! `instruction` (D108, plan KH T4): the 26th layer-row kind, one prompt slot of a template layer. The key is the
//! slot ([`SLOTS`]); the fields are `text`, an optional `language` and `mode` (`replace`, the default, or
//! `append`). Instruction rows live in every template layer (base, industry, industry_dept, job; the region
//! sub-layers of D111 once T9 builds them) and fold in the compile's layer order (D112):
//!
//! - an `add` or `patch` with `mode: replace` replaces the slot (one section, this layer's);
//! - `mode: append` adds this layer's text as one more section after the inherited ones;
//! - a tombstone removes the slot.
//!
//! **The job preamble is the job's `identity` instruction (D108).** In each layer the `identity` comes from, first
//! match wins: the layer's own `identity` instruction row; its `job_setting` row keyed `preamble` (`value`, else
//! `summary`; a tombstone removes the identity); its `job` row's `preamble` field (the job layer, a channel layer of it,
//! or the base row that adds the job). Each folds as an `identity` replace at that layer's position, so stored rows and
//! immutable tags need no rewrite.
//!
//! **Channel layers fold per channel (KH T5).** A row of a `job/<j>/channel=<c>` dimension layer changes only channel
//! `c`'s view of its slot: its sections carry `channel: c`; a `replace` drops the channel-agnostic sections from that
//! channel's view only (and its own earlier sections), a tombstone hides the slot for that channel only. Every other
//! channel — and the channel-agnostic view — keeps the slot as the non-channel layers left it
//! ([`FoldedInstruction::view`]). Non-channel layers fold as before.
//!
//! [`InstructionFold::finish`] returns the folded slots in [`SLOTS`] order, each with its sections and the layer
//! (and pinned tag) every section came from, so the prompt can mark each section `[layer <key>@<version>]`. Tenant
//! refinements (D8) may touch [`TENANT_SLOTS`] only — never `identity`, `policy` or `safety`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::{FieldError, done, require_text};
use crate::layer::LayerKey;

/// The six prompt slots, in the order a prompt assembles them.
pub const SLOTS: [&str; 6] = [
    "identity",
    "policy",
    "tone",
    "safety",
    "escalation",
    "format",
];

/// The slots a tenant may refine (D108, D8).
pub const TENANT_SLOTS: [&str; 3] = ["tone", "escalation", "format"];

/// The slot a job's `preamble` becomes.
pub const IDENTITY: &str = "identity";

pub fn is_slot(key: &str) -> bool {
    SLOTS.contains(&key)
}

/// Whether a tenant refinement may touch `slot`.
pub fn tenant_refinable(slot: &str) -> bool {
    TENANT_SLOTS.contains(&slot)
}

/// How an instruction row combines with the slot it inherits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Replace,
    Append,
}

impl Mode {
    fn is_replace(&self) -> bool {
        *self == Mode::Replace
    }
}

/// Fields of an `instruction` row. `summary` is what platform writes store (the text in one line).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionFields {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Mode::is_replace")]
    pub mode: Mode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

pub(crate) fn check(f: &InstructionFields) -> Result<(), Vec<FieldError>> {
    let mut errors = Vec::new();
    require_text(&f.text, "text", &mut errors);
    if let Some(l) = &f.language
        && !is_language(l)
    {
        errors.push(FieldError::new(
            "invalid_value",
            Some("language"),
            "must be a language tag such as en or nl-BE",
        ));
    }
    done(errors)
}

/// A BCP-47-ish tag: 2–3 letters, then `-` parts of 2–8 letters or digits (`en`, `nl-BE`, `zh-Hant`).
fn is_language(s: &str) -> bool {
    let mut parts = s.split('-');
    let primary = parts.next().unwrap_or_default();
    (2..=3).contains(&primary.len())
        && primary.chars().all(|c| c.is_ascii_alphabetic())
        && parts.all(|p| (2..=8).contains(&p.len()) && p.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// The row's one-line summary: its text, flattened (`append · ` first for an append).
pub(crate) fn summary(f: &InstructionFields) -> String {
    let text = f.text.split_whitespace().collect::<Vec<_>>().join(" ");
    match f.mode {
        Mode::Replace => text,
        Mode::Append => format!("append · {text}"),
    }
}

/// One layer's contribution to a folded slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstructionSection {
    /// The layer key (`job/customer_care_agent`).
    pub layer: String,
    /// The pinned tag of that layer's family (`job/customer_care_agent@v5`); absent in a head compile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The channel `c` of a `job/<j>/channel=<c>` layer's section: it belongs to that channel's view only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

impl InstructionSection {
    /// The version part of [`Self::tag`] (`v5`), or `head` for an untagged compile.
    pub fn version(&self) -> &str {
        self.tag
            .as_deref()
            .and_then(|t| t.rsplit_once('@'))
            .map_or("head", |(_, v)| v)
    }

    /// The section marker the prompt opens it with (D108): `[layer <key>@<version>]`.
    pub fn marker(&self) -> String {
        format!("[layer {}@{}]", self.layer, self.version())
    }
}

/// One folded slot: its sections in fold order (channel sections included) and their texts joined by a blank line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FoldedInstruction {
    /// The slot.
    pub key: String,
    pub text: String,
    pub sections: Vec<InstructionSection>,
    /// The channels whose own layer replaced or hid the slot: their view drops the channel-agnostic sections.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub channel_replaces: Vec<String>,
}

impl FoldedInstruction {
    /// The sections a turn on `channel` sees: the channel-agnostic ones (unless that channel's layer replaced or hid
    /// the slot), then that channel's own. `None`: the channel-agnostic sections only.
    pub fn view(&self, channel: Option<&str>) -> Vec<&InstructionSection> {
        let replaced = channel.is_some_and(|c| self.channel_replaces.iter().any(|r| r == c));
        self.sections
            .iter()
            .filter(|s| match s.channel.as_deref() {
                None => !replaced,
                Some(c) => Some(c) == channel,
            })
            .collect()
    }
}

/// The channel `c` of a `job/<j>/channel=<c>` layer label.
fn channel_of(layer: &str) -> Option<String> {
    LayerKey::parse_label(layer)
        .ok()
        .and_then(|k| k.dimension)
        .filter(|(dim, _)| dim == "channel")
        .map(|(_, value)| value)
}

#[derive(Debug, Default)]
struct SlotState {
    sections: Vec<InstructionSection>,
    replaced: Vec<String>,
}

/// The per-slot fold (module docs).
#[derive(Debug, Default)]
pub struct InstructionFold {
    slots: BTreeMap<String, SlotState>,
}

impl InstructionFold {
    /// Apply an `add` / `patch` of `slot` from `layer` (pinned at `tag`).
    pub fn set(&mut self, layer: &str, tag: Option<&str>, slot: &str, f: &InstructionFields) {
        let channel = channel_of(layer);
        let state = self.slots.entry(slot.to_owned()).or_default();
        if f.mode == Mode::Replace {
            match &channel {
                None => *state = SlotState::default(),
                Some(c) => hide_for(state, c),
            }
        }
        state.sections.push(InstructionSection {
            layer: layer.to_owned(),
            tag: tag.map(str::to_owned),
            text: f.text.trim().to_owned(),
            language: f.language.clone(),
            channel,
        });
    }

    /// A tombstone: the slot is gone, with everything it inherited.
    pub fn remove(&mut self, slot: &str) {
        self.slots.remove(slot);
    }

    /// A tombstone from `layer`: a channel layer hides the slot for its channel only; any other layer removes it
    /// ([`Self::remove`]).
    pub fn remove_at(&mut self, layer: &str, slot: &str) {
        match channel_of(layer) {
            None => self.remove(slot),
            Some(c) => {
                if let Some(state) = self.slots.get_mut(slot) {
                    hide_for(state, &c);
                }
            }
        }
    }

    /// The folded slots in [`SLOTS`] order.
    pub fn finish(mut self) -> Vec<FoldedInstruction> {
        SLOTS
            .iter()
            .filter_map(|slot| {
                let SlotState { sections, replaced } = self.slots.remove(*slot)?;
                if sections.is_empty() {
                    return None;
                }
                let text = sections
                    .iter()
                    .map(|s| s.text.as_str())
                    .collect::<Vec<_>>()
                    .join("\n\n");
                Some(FoldedInstruction {
                    key: (*slot).to_owned(),
                    text,
                    sections,
                    channel_replaces: replaced,
                })
            })
            .collect()
    }
}

/// Channel `c`'s view of `state` drops the channel-agnostic sections (they stay for every other view) and `c`'s own
/// so far.
fn hide_for(state: &mut SlotState, c: &str) {
    state.sections.retain(|s| s.channel.as_deref() != Some(c));
    if !state.replaced.iter().any(|r| r == c) {
        state.replaced.push(c.to_owned());
    }
}

/// The folded text of `slot` in `set` (`None` when the slot is absent).
pub fn slot_text<'a>(set: &'a [FoldedInstruction], slot: &str) -> Option<&'a str> {
    set.iter().find(|i| i.key == slot).map(|i| i.text.as_str())
}

#[cfg(test)]
mod tests {
    use super::super::{Kind, Op, validate_key, validate_row};
    use super::*;
    use serde_json::json;

    fn fields(text: &str, mode: Mode) -> InstructionFields {
        InstructionFields {
            text: text.to_owned(),
            language: None,
            mode,
            summary: None,
        }
    }

    #[test]
    fn keys_are_the_six_slots() {
        for slot in SLOTS {
            assert_eq!(validate_key(Kind::Instruction, slot), Ok(()), "{slot}");
        }
        for bad in ["persona", "Identity", "", "tone|x"] {
            assert_eq!(
                validate_key(Kind::Instruction, bad).err().map(|e| e.code),
                Some("invalid_key"),
                "{bad:?}"
            );
        }
        assert!(tenant_refinable("tone") && tenant_refinable("format"));
        assert!(!tenant_refinable("policy") && !tenant_refinable("identity"));
    }

    #[test]
    fn fields_are_checked() {
        let ok =
            json!({"text": "Be brief.", "language": "nl-BE", "mode": "append", "summary": "x"});
        assert_eq!(validate_row(Kind::Instruction, Op::Add, &ok), Ok(()));
        assert_eq!(
            validate_row(Kind::Instruction, Op::Patch, &json!({"text": "Be brief."})),
            Ok(())
        );
        for bad in [
            json!({"text": " "}),
            json!({"text": "x", "language": "english!"}),
            json!({"text": "x", "mode": "prepend"}),
            json!({"text": "x", "colour": 1}),
            json!({"language": "en"}),
        ] {
            assert!(
                validate_row(Kind::Instruction, Op::Add, &bad).is_err(),
                "{bad}"
            );
        }
        assert_eq!(
            validate_row(Kind::Instruction, Op::Tombstone, &json!({})),
            Ok(())
        );
    }

    #[test]
    fn replace_append_and_tombstone_fold_per_slot() {
        let mut f = InstructionFold::default();
        f.set(
            "base",
            Some("base@v3"),
            "tone",
            &fields("Be kind.", Mode::Replace),
        );
        f.set(
            "base",
            Some("base@v3"),
            "policy",
            &fields("No advice.", Mode::Replace),
        );
        f.set(
            "industry/retail",
            Some("industry/retail@v2"),
            "tone",
            &fields(" Use first names. ", Mode::Append),
        );
        f.set(
            "job/x",
            None,
            "identity",
            &fields("You help X.", Mode::Append),
        );
        f.set(
            "job/x",
            None,
            "policy",
            &fields("Advice is fine.", Mode::Replace),
        );
        f.set("job/x", None, "format", &fields("Short.", Mode::Replace));
        f.remove("format");
        let set = f.finish();
        let keys: Vec<&str> = set.iter().map(|i| i.key.as_str()).collect();
        assert_eq!(
            keys,
            ["identity", "policy", "tone"],
            "slot order, format removed"
        );
        assert_eq!(
            slot_text(&set, "tone"),
            Some("Be kind.\n\nUse first names.")
        );
        assert_eq!(slot_text(&set, "policy"), Some("Advice is fine."));
        assert_eq!(slot_text(&set, "identity"), Some("You help X."));
        let tone = &set[2];
        let markers: Vec<String> = tone
            .sections
            .iter()
            .map(InstructionSection::marker)
            .collect();
        assert_eq!(markers, ["[layer base@v3]", "[layer industry/retail@v2]"]);
        assert_eq!(set[1].sections[0].marker(), "[layer job/x@head]");
    }

    #[test]
    fn a_channel_layer_replaces_or_hides_a_slot_for_its_channel_only() {
        let texts = |set: &[FoldedInstruction], slot: &str, ch: Option<&str>| -> Vec<String> {
            set.iter()
                .find(|i| i.key == slot)
                .map(|i| i.view(ch).iter().map(|s| s.text.clone()).collect())
                .unwrap_or_default()
        };
        let mut f = InstructionFold::default();
        f.set("base", None, "format", &fields("Markdown.", Mode::Replace));
        f.set("base", None, "tone", &fields("Kind.", Mode::Replace));
        f.set(
            "job/x/channel=email",
            None,
            "format",
            &fields("Plain text.", Mode::Replace),
        );
        f.set(
            "job/x/channel=whatsapp",
            None,
            "format",
            &fields("Emoji ok.", Mode::Append),
        );
        f.remove_at("job/x/channel=email", "tone");
        let set = f.finish();
        assert_eq!(texts(&set, "format", Some("web_chat")), ["Markdown."]);
        assert_eq!(texts(&set, "format", None), ["Markdown."]);
        assert_eq!(texts(&set, "format", Some("email")), ["Plain text."]);
        assert_eq!(
            texts(&set, "format", Some("whatsapp")),
            ["Markdown.", "Emoji ok."]
        );
        assert_eq!(texts(&set, "tone", Some("web_chat")), ["Kind."]);
        assert!(texts(&set, "tone", Some("email")).is_empty());
        let format = set.iter().find(|i| i.key == "format").unwrap();
        assert_eq!(format.channel_replaces, ["email"]);
        assert_eq!(format.sections[1].channel.as_deref(), Some("email"));
        // a non-channel tombstone still removes the slot for everyone
        let mut g = InstructionFold::default();
        g.set("base", None, "tone", &fields("Kind.", Mode::Replace));
        g.remove_at("job/x", "tone");
        assert!(g.finish().is_empty());
    }

    #[test]
    fn summary_flattens_the_text() {
        assert_eq!(summary(&fields("Be\n  kind.", Mode::Replace)), "Be kind.");
        assert_eq!(summary(&fields("More.", Mode::Append)), "append · More.");
    }
}
