//! B8 T8: the drafting prompts, one per LLM call kind, compiled in (`prompts/*.md`). Every prompt ends with the
//! shared answer contract (`prompts/_contract.md`): `{"items": [..]}`, parsed by crate::generator::llm.
use crate::journey::model::GeneratorStepKey;

const CONTRACT: &str = include_str!("prompts/_contract.md");

/// One kind of drafting call: a generator step, the journey step's merge of its samples, or discovery's
/// cluster drafting (T14b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Call {
    Step(GeneratorStepKey),
    JourneyMerge,
    Clusters,
}

impl Call {
    /// The `usage_event.use` label of the call (`^[a-z][a-z0-9_]{0,39}$`, 0064).
    pub fn use_label(self) -> &'static str {
        match self {
            Self::Step(s) => s.as_str(),
            Self::JourneyMerge => "journey_merge",
            Self::Clusters => "cluster",
        }
    }
}

fn body(c: Call) -> Option<&'static str> {
    Some(match c {
        Call::Step(GeneratorStepKey::BusinessModel) => include_str!("prompts/business_model.md"),
        Call::Step(GeneratorStepKey::Journey) => include_str!("prompts/journey.md"),
        Call::Step(GeneratorStepKey::Objects) => include_str!("prompts/objects.md"),
        Call::Step(GeneratorStepKey::Operations) => include_str!("prompts/operations.md"),
        Call::Step(GeneratorStepKey::Harmonise) => include_str!("prompts/harmonise.md"),
        Call::Step(GeneratorStepKey::UseCases) => include_str!("prompts/use_cases.md"),
        Call::JourneyMerge => include_str!("prompts/journey_merge.md"),
        Call::Clusters => include_str!("prompts/clusters.md"),
        Call::Step(_) => return None,
    })
}

/// The system prompt of `c` (its body + the answer contract); `None` for a step that does not call the LLM.
pub fn system(c: Call) -> Option<String> {
    body(c).map(|b| format!("{}\n\n{CONTRACT}", b.trim_end()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all() -> Vec<Call> {
        let mut v: Vec<Call> = GeneratorStepKey::ALL.into_iter().map(Call::Step).collect();
        v.extend([Call::JourneyMerge, Call::Clusters]);
        v
    }

    #[test]
    fn every_llm_step_has_a_prompt() {
        for s in GeneratorStepKey::ALL {
            assert_eq!(system(Call::Step(s)).is_some(), s.drafts(), "{s:?}");
        }
        assert!(system(Call::JourneyMerge).is_some());
        assert!(system(Call::Clusters).is_some());
    }

    #[test]
    fn prompts_say_operations_not_actions() {
        for c in all() {
            let Some(p) = system(c) else { continue };
            let lower = p.to_lowercase();
            let words = lower.split(|ch: char| !ch.is_ascii_alphabetic());
            for w in words {
                assert!(w != "action" && w != "actions", "{c:?} says “{w}”");
            }
            assert!(lower.contains("operation"), "{c:?}");
            assert!(lower.contains("\"items\""), "{c:?}");
        }
    }

    #[test]
    fn use_labels_are_metering_labels() {
        for c in all() {
            let l = c.use_label();
            assert!(l.len() <= 40 && l.starts_with(|ch: char| ch.is_ascii_lowercase()));
            assert!(
                l.chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
            );
        }
    }
}
