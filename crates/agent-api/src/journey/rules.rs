//! B8 T2a: the 11§7 machine rules as pure functions over an [`IndustryJourney`] and the head catalogue it is checked
//! against. Rules 1, 3, 4, 5 are the fixture's `industryFindings` (same order, same messages); rules 2 and 6 (from
//! P8 T5) follow; rule 7 is import evidence, never red ([`evidence_findings`]).
use std::collections::{BTreeMap, BTreeSet};

use super::model::{
    ANY_STAGE, ChannelKey, IndustryJourney, Severity, ValidationFinding, object_key,
};

/// What the rules know about one canonical operation (B6 `operation_def` / the fixture's `OPS`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpInfo {
    /// The identification level (`0`, `1`, `2`, `1→2`, `1 / 2`).
    pub level: String,
    /// `do` | `request` | `observe`.
    pub modality: String,
    /// A `request` op lands somewhere: a case type or a pending state on its object (rule 6).
    pub lands: bool,
}

/// The canonical model the rules check against: objects with their fields, and operations.
#[derive(Clone, Debug, Default)]
pub struct Catalogue {
    pub objects: BTreeMap<String, BTreeSet<String>>,
    pub operations: BTreeMap<String, OpInfo>,
}

fn finding(
    rule: u8,
    severity: Severity,
    message: String,
    stage: Option<&str>,
    activity: Option<&str>,
) -> ValidationFinding {
    ValidationFinding {
        rule,
        severity,
        message,
        stage: stage.map(str::to_owned),
        activity: activity.map(str::to_owned),
    }
}

/// Every finding of rules 1–4 and 6 on `j`. `red` blocks tagging.
pub fn industry_findings(j: &IndustryJourney, cat: &Catalogue) -> Vec<ValidationFinding> {
    let mut out = rule4(j);
    out.extend(rule1(j, cat));
    out.extend(rule3(j));
    out.extend(rule2(j, cat));
    out.extend(rule6(j, cat));
    out
}

/// How many findings are red.
pub fn red_count(f: &[ValidationFinding]) -> usize {
    f.iter().filter(|x| x.severity == Severity::Red).count()
}

/// Rule 4, per in-scope stage: no activity / no chat messages / no business objects is red; no knowledge kind
/// (neither for the stage nor `*`, D107) is amber.
pub fn rule4(j: &IndustryJourney) -> Vec<ValidationFinding> {
    let mut out = Vec::new();
    for s in j.stages.iter().filter(|s| !s.out_of_scope) {
        let key = s.key.as_str();
        let acts: Vec<_> = j
            .activities
            .iter()
            .filter(|a| a.stage.as_str() == key)
            .collect();
        if acts.is_empty() {
            out.push(finding(
                4,
                Severity::Red,
                format!("stage {} has no activity", s.label),
                Some(key),
                None,
            ));
        }
        let chat = j.messages.iter().any(|m| {
            m.stage.as_str() == key && m.channel == ChannelKey::Chat && !m.utterances.is_empty()
        });
        if !chat {
            out.push(finding(
                4,
                Severity::Red,
                format!("stage {} has no chat messages", s.label),
                Some(key),
                None,
            ));
        }
        if !acts.is_empty() && !acts.iter().any(|a| !a.objects.is_empty()) {
            out.push(finding(
                4,
                Severity::Red,
                format!("stage {} has no business objects", s.label),
                Some(key),
                None,
            ));
        }
        if !j
            .knowledge_kinds
            .iter()
            .any(|k| k.stage == key || k.stage == ANY_STAGE)
        {
            out.push(finding(
                4,
                Severity::Amber,
                format!("stage {} names no knowledge kind", s.label),
                Some(key),
                None,
            ));
        }
    }
    out
}

/// Rule 1: an activity names an object that is not canonical.
pub fn rule1(j: &IndustryJourney, cat: &Catalogue) -> Vec<ValidationFinding> {
    let mut out = Vec::new();
    for a in &j.activities {
        for o in &a.objects {
            if !cat.objects.contains_key(object_key(&o.object)) {
                out.push(finding(
                    1,
                    Severity::Red,
                    format!("{} names non-canonical object {}", a.key, o.object),
                    Some(a.stage.as_str()),
                    Some(&a.key),
                ));
            }
        }
    }
    out
}

/// Rule 2: every field / required input an activity names exists on its (canonical) object, and every operation
/// it expects exists. A non-canonical object is rule 1's, not checked here.
pub fn rule2(j: &IndustryJourney, cat: &Catalogue) -> Vec<ValidationFinding> {
    let mut out = Vec::new();
    for a in &j.activities {
        let at = |m: String| finding(2, Severity::Red, m, Some(a.stage.as_str()), Some(&a.key));
        for o in &a.objects {
            let key = object_key(&o.object);
            let Some(fields) = cat.objects.get(key) else {
                continue;
            };
            let named: BTreeSet<&String> = o.fields.iter().chain(&o.required_inputs).collect();
            for f in named {
                if !fields.contains(f) {
                    out.push(at(format!("{} names unknown field {key}.{f}", a.key)));
                }
            }
        }
        for op in &a.operations {
            if !cat.operations.contains_key(&op.op) {
                out.push(at(format!("{} expects unknown operation {}", a.key, op.op)));
            }
        }
    }
    out
}

/// Rule 3: an object of the business model that no activity uses.
pub fn rule3(j: &IndustryJourney) -> Vec<ValidationFinding> {
    let used: BTreeSet<&str> = j
        .activities
        .iter()
        .flat_map(|a| a.objects.iter().map(|o| object_key(&o.object)))
        .collect();
    j.model_objects
        .iter()
        .filter(|o| !used.contains(o.as_str()))
        .map(|o| {
            finding(
                3,
                Severity::Red,
                format!("object {o} is in the business model but no activity uses it"),
                None,
                None,
            )
        })
        .collect()
}

/// Rule 6: every `request` operation the journey expects lands in a case type or a pending state. One finding per
/// operation (at its first activity).
pub fn rule6(j: &IndustryJourney, cat: &Catalogue) -> Vec<ValidationFinding> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for a in &j.activities {
        for o in &a.operations {
            let Some(info) = cat.operations.get(&o.op) else {
                continue;
            };
            if info.modality == "request" && !info.lands && seen.insert(o.op.as_str()) {
                out.push(finding(
                    6,
                    Severity::Red,
                    format!(
                        "request {} has no case type or pending state to land in",
                        o.op
                    ),
                    Some(a.stage.as_str()),
                    Some(&a.key),
                ));
            }
        }
    }
    out
}

/// One uncovered cluster of imported evidence (generator evidence / discovery import).
#[derive(Clone, Debug)]
pub struct Uncovered<'a> {
    pub label: &'a str,
    pub tickets: u32,
    pub stage: Option<&'a str>,
}

/// Rule 7: import evidence the journey does not cover. Information only: `info`, never red, never blocks a tag.
pub fn evidence_findings(clusters: &[Uncovered<'_>]) -> Vec<ValidationFinding> {
    clusters
        .iter()
        .map(|c| {
            finding(
                7,
                Severity::Info,
                format!(
                    "{} tickets about “{}” match no activity",
                    c.tickets, c.label
                ),
                c.stage,
                None,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journey::model::*;
    use chrono::Utc;

    fn slug(s: &str) -> Slug {
        Slug::new(s).unwrap()
    }

    fn stage(key: &str, out_of_scope: bool) -> JourneyStage {
        JourneyStage {
            key: slug(key),
            label: key.to_uppercase(),
            order: 1,
            description: String::new(),
            synonyms: vec![],
            out_of_scope,
            themes: vec![],
            layer: None,
        }
    }

    fn obj(o: &str, fields: &[&str]) -> ActivityObject {
        ActivityObject {
            object: o.into(),
            role: ObjectRole::Subject,
            fields: fields.iter().map(|f| (*f).into()).collect(),
            required_inputs: vec![],
        }
    }

    fn op(o: &str) -> OperationExpectation {
        OperationExpectation {
            op: o.into(),
            variant: None,
            required: true,
        }
    }

    fn act(key: &str, st: &str, objects: Vec<ActivityObject>, ops: &[&str]) -> JourneyActivity {
        JourneyActivity {
            key: key.into(),
            stage: slug(st),
            actor: Actor::Customer,
            label: key.into(),
            trigger: String::new(),
            also_in_stages: vec![],
            objects,
            operations: ops.iter().copied().map(op).collect(),
            summary: None,
            description: None,
            area: None,
            layer: None,
            required: false,
        }
    }

    fn info(level: &str, modality: &str, lands: bool) -> OpInfo {
        OpInfo {
            level: level.into(),
            modality: modality.into(),
            lands,
        }
    }

    fn cat() -> Catalogue {
        let mut c = Catalogue::default();
        c.objects
            .insert("order".into(), ["id", "status"].map(String::from).into());
        c.objects
            .insert("case".into(), ["id", "reason"].map(String::from).into());
        c.operations
            .insert("order.read".into(), info("2", "observe", false));
        c.operations
            .insert("offering.list".into(), info("0", "observe", false));
        c.operations.insert(
            "delivery.report_problem".into(),
            info("2", "request", false),
        );
        c.operations
            .insert("case.create".into(), info("1", "request", true));
        c
    }

    /// One complete in-scope stage: green on every rule.
    fn grid() -> IndustryJourney {
        IndustryJourney {
            industry: "retail".into(),
            label: "Retail".into(),
            department: "customer_service".into(),
            status: JourneyStatus::Draft,
            tag: None,
            dept_tag: None,
            draft_rev: 0,
            updated_at: Utc::now(),
            updated_by: "owner".into(),
            themes: vec![],
            stages: vec![stage("delivery", false)],
            priors: vec![],
            channels: vec![],
            activities: vec![act(
                "delivery:track",
                "delivery",
                vec![obj("order", &["status"])],
                &["order.read"],
            )],
            messages: vec![StageMessage {
                stage: slug("delivery"),
                channel: ChannelKey::Chat,
                utterances: vec!["where is my parcel".into()],
                layer: None,
            }],
            knowledge_kinds: vec![StageKnowledgeKind {
                stage: "delivery".into(),
                kind: "delivery_policy".into(),
                required: true,
                description: String::new(),
                layer: None,
            }],
            model_objects: vec!["order".into()],
            findings: vec![],
            source_run: None,
        }
    }

    fn rules_of(f: &[ValidationFinding]) -> Vec<(u8, Severity)> {
        f.iter().map(|x| (x.rule, x.severity)).collect()
    }

    #[test]
    fn complete_grid_is_green() {
        assert!(industry_findings(&grid(), &cat()).is_empty());
    }

    #[test]
    fn rule1_free_text_object_is_red() {
        let mut j = grid();
        j.activities[0].objects.push(obj("parcel thing", &[]));
        let f = industry_findings(&j, &cat());
        assert_eq!(rules_of(&f), [(1, Severity::Red)]);
        assert_eq!(f[0].activity.as_deref(), Some("delivery:track"));
        // a variant of a canonical object is canonical
        j.activities[0].objects[1] = obj("case{return}", &["reason"]);
        assert!(industry_findings(&j, &cat()).is_empty());
    }

    #[test]
    fn rule2_unknown_field_is_red() {
        let mut j = grid();
        j.activities[0].objects[0]
            .required_inputs
            .push("colour".into());
        j.activities[0].operations.push(op("order.teleport"));
        let f = rule2(&j, &cat());
        assert_eq!(rules_of(&f), [(2, Severity::Red), (2, Severity::Red)]);
        assert!(f[0].message.contains("order.colour"));
        assert!(f[1].message.contains("order.teleport"));
    }

    #[test]
    fn rule3_unused_object_is_red() {
        let mut j = grid();
        j.model_objects.push("case".into());
        let f = industry_findings(&j, &cat());
        assert_eq!(rules_of(&f), [(3, Severity::Red)]);
        assert!(f[0].message.starts_with("object case "));
    }

    #[test]
    fn rule4_stage_without_activity_red_unless_out_of_scope() {
        let mut j = grid();
        j.stages.push(stage("exit", false));
        let f = rule4(&j);
        // no activity, no chat messages: red; no knowledge kind: amber
        assert_eq!(
            rules_of(&f),
            [(4, Severity::Red), (4, Severity::Red), (4, Severity::Amber)]
        );
        assert!(f.iter().all(|x| x.stage.as_deref() == Some("exit")));
        j.stages[1].out_of_scope = true;
        assert!(rule4(&j).is_empty());
    }

    #[test]
    fn rule4_no_business_objects_is_red() {
        let mut j = grid();
        j.activities[0].objects.clear();
        j.model_objects.clear();
        assert_eq!(rules_of(&rule4(&j)), [(4, Severity::Red)]);
    }

    #[test]
    fn rule4_no_knowledge_kind_is_amber() {
        let mut j = grid();
        j.knowledge_kinds.clear();
        let f = industry_findings(&j, &cat());
        assert_eq!(rules_of(&f), [(4, Severity::Amber)]);
        assert_eq!(red_count(&f), 0);
        // a kind for every stage (`*`, D107) covers it
        j.knowledge_kinds.push(StageKnowledgeKind {
            stage: ANY_STAGE.into(),
            kind: "faq".into(),
            required: false,
            description: String::new(),
            layer: None,
        });
        assert!(industry_findings(&j, &cat()).is_empty());
    }

    #[test]
    fn rule6_request_without_landing_is_red() {
        let mut j = grid();
        j.activities[0]
            .operations
            .push(op("delivery.report_problem"));
        j.activities.push(act(
            "delivery:report",
            "delivery",
            vec![obj("order", &[])],
            &["delivery.report_problem", "case.create"],
        ));
        let f = industry_findings(&j, &cat());
        // once per operation; case.create lands in a case type
        assert_eq!(rules_of(&f), [(6, Severity::Red)]);
        assert_eq!(f[0].activity.as_deref(), Some("delivery:track"));
    }

    #[test]
    fn rule7_evidence_is_never_red() {
        let f = evidence_findings(&[Uncovered {
            label: "gift cards",
            tickets: 40,
            stage: Some("purchase"),
        }]);
        assert_eq!(rules_of(&f), [(7, Severity::Info)]);
        assert_eq!(red_count(&f), 0);
    }
}
