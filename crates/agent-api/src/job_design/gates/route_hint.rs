//! The qualifier route hint (B5 T10a, F-19): B4's [`RouteHinter`] seam, answered from the P1 verdict's GS (journey
//! stage) and GA (activity) answers. Advisory only: the qualifier still decides the context space.
//!
//! 1. Both answers must be on the verdict (Laya answered them; a failed or unbound point carries none).
//! 2. GA is restricted to the top-2 GS stages: a GA option key is `<stage>:<activity>`, and only options whose
//!    stage is one of the two most probable GS options count (spec 17 F-19, the GA library row's "the enabled
//!    activities of the top-2 stages"). Ties keep the option order of the key (deterministic).
//! 3. The hinted activity is the most probable remaining GA option; its GA probability is the hint's confidence.
//!    Below [`MIN_CONFIDENCE`] there is no hint.
//! 4. The request type is the context space that serves that activity (`SpaceCatalog::for_activity`, the space's
//!    `activity`, R15); an activity no space serves gives no hint.
//!
//! Ids, codes and a confidence only (K50).
use std::cmp::Ordering;

use crate::runtime::design::RuntimeDesign;
use crate::runtime::gates::{GateAnswerLite, GateVerdict, RouteHint, RouteHinter};

/// The journey-stage question (library `GS`).
pub const STAGE_GATE: &str = "GS";
/// The activity question (library `GA`); its option keys are `<stage>:<activity>`.
pub const ACTIVITY_GATE: &str = "GA";
/// How many GS stages GA is restricted to.
pub const TOP_STAGES: usize = 2;
/// Below this GA probability there is no hint.
pub const MIN_CONFIDENCE: f64 = 0.5;

/// B5's hinter (installed on `AppState.gates` by T12).
pub struct QualifierHint;

impl RouteHinter for QualifierHint {
    fn hint(&self, p1: &GateVerdict, d: &RuntimeDesign) -> Option<RouteHint> {
        let (stage, activity, confidence) = best_activity(&p1.answers)?;
        let space = d.spaces.for_activity(&activity)?;
        Some(RouteHint {
            request_type: space.key.clone(),
            stage: Some(stage),
            activity: Some(activity),
            confidence,
        })
    }
}

fn answer<'a>(answers: &'a [GateAnswerLite], gate: &str) -> Option<&'a GateAnswerLite> {
    answers.iter().find(|a| a.gate == gate)
}

/// Highest probability first; equal probabilities keep key order.
fn by_probability(a: &(&String, &f64), b: &(&String, &f64)) -> Ordering {
    b.1.total_cmp(a.1).then_with(|| a.0.cmp(b.0))
}

/// The top-2 GS stages (the chosen value alone when GS carries no probabilities).
pub fn top_stages(gs: &GateAnswerLite) -> Vec<String> {
    if gs.probabilities.is_empty() {
        return vec![gs.value.clone()];
    }
    let mut ps: Vec<(&String, &f64)> = gs.probabilities.iter().collect();
    ps.sort_by(by_probability);
    ps.into_iter()
        .take(TOP_STAGES)
        .map(|(k, _)| k.clone())
        .collect()
}

/// `(stage, activity, confidence)` of the hint, before the space lookup: the most probable GA option within the
/// top-2 GS stages, at or above [`MIN_CONFIDENCE`].
pub fn best_activity(answers: &[GateAnswerLite]) -> Option<(String, String, f64)> {
    let gs = answer(answers, STAGE_GATE)?;
    let ga = answer(answers, ACTIVITY_GATE)?;
    let stages = top_stages(gs);
    let mut ps: Vec<(&String, &f64)> = ga
        .probabilities
        .iter()
        .filter(|(k, _)| {
            k.split_once(':')
                .is_some_and(|(stage, _)| stages.iter().any(|s| s == stage))
        })
        .collect();
    ps.sort_by(by_probability);
    let (activity, p) = ps.first()?;
    if **p < MIN_CONFIDENCE {
        return None;
    }
    let stage = activity.split_once(':').map(|(s, _)| s.to_owned())?;
    Some((stage, String::clone(activity), **p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lite(gate: &str, value: &str, ps: &[(&str, f64)]) -> GateAnswerLite {
        GateAnswerLite {
            gate: gate.into(),
            value: value.into(),
            probabilities: ps.iter().map(|(k, p)| ((*k).to_owned(), *p)).collect(),
        }
    }

    #[test]
    fn ga_is_restricted_to_the_top_two_stages() {
        let gs = lite(
            STAGE_GATE,
            "support",
            &[("support", 0.6), ("delivery", 0.3), ("change", 0.1)],
        );
        // change:cancel_order is GA's top, but change is not a top-2 stage
        let ga = lite(
            ACTIVITY_GATE,
            "change:cancel_order",
            &[
                ("change:cancel_order", 0.4),
                ("delivery:track_parcel", 0.55),
                ("support:start_return", 0.05),
            ],
        );
        assert_eq!(top_stages(&gs), ["support", "delivery"]);
        assert_eq!(
            best_activity(&[gs, ga]),
            Some((
                "delivery".to_owned(),
                "delivery:track_parcel".to_owned(),
                0.55
            ))
        );
    }

    #[test]
    fn no_hint_without_both_answers_or_below_the_line() {
        let gs = lite(STAGE_GATE, "support", &[("support", 0.9)]);
        assert_eq!(best_activity(std::slice::from_ref(&gs)), None);
        let ga = lite(
            ACTIVITY_GATE,
            "support:start_return",
            &[("support:start_return", 0.49)],
        );
        assert_eq!(best_activity(&[gs, ga]), None);
    }
}
