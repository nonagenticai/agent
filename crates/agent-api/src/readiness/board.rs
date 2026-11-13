//! The J3 board (fixture `buildBoard`, `releaseReadinessOf`, `blockingItem` in `areas/readiness.ts`) and the 10 §5.2
//! checklist (fixture `readinessOf` in `areas/release.ts`), built from the contributors' findings. Pure functions:
//! no database.
use std::collections::BTreeMap;

use super::{
    AREA_ORDER, AreaKey, AreaStatus, Assessed, Blocks, Checked, Enforced, Evaluation, Fixer,
    MissingItem, ReadinessArea, ReadinessBoard, ReadinessCode, ReadinessGap, ReadinessItem,
    ReadinessStatus, ReadinessStep, ReleaseReadiness, Verdict,
};

/// The summary of an area or the detail of a step no contributor reports yet.
pub const NOT_CHECKED: &str = "Not checked yet";

/// The checklist steps in fixture order: `(key, label, path)`; a path `job:<sub>` is job-relative
/// (`jobPath(job, sub)`), see [`step_href`].
pub const STEPS: [(&str, &str, &str); 10] = [
    ("template", "Choose job template", "job:/journey"),
    ("operations", "Review operation list", "job:/operations"),
    ("systems", "Pick systems", "/admin/connections/systems"),
    (
        "instances",
        "Configure connector instances",
        "/admin/connections/systems",
    ),
    ("routes", "Route matrix", "job:/operations"),
    ("mapping", "Mapping", "/admin/connections/systems"),
    ("rules", "Business-rule slots", "job:/rules"),
    ("tests", "Test", "job:/operations"),
    ("go_live", "Go live", "job:/release"),
    ("operate", "Operate", "job:/analytics"),
];

/// The definition-of-ready lines in fixture order: `(key, label, path)`.
pub const DOR: [(&str, &str, &str); 6] = [
    ("dor_slots", "W5 slots deployed", "/admin/providers"),
    (
        "dor_routes",
        "Required operations routed in Production",
        "job:/operations",
    ),
    ("dor_tests", "Contract tests", "job:/operations"),
    ("dor_rules", "Rule values", "job:/rules"),
    ("dor_gates", "Gate questions reviewed", "job:/gates"),
    (
        "dor_knowledge",
        "Knowledge kinds bound",
        "job:/knowledge-kinds",
    ),
];

/// What each W5 slot does for the customer (fixture `SLOT_USE`).
const SLOT_USE: [(&str, &str); 5] = [
    ("strata_rag.embed", "index knowledge so it can be searched"),
    (
        "strata_rag.rerank",
        "rank search hits so the best passage is quoted",
    ),
    ("strata_rag.chat", "write answers from your knowledge"),
    ("strata_ingest.ocr", "read text from scanned PDFs"),
    (
        "strata_ingest.vision",
        "read the photos and images customers send",
    ),
];

/// JavaScript `encodeURIComponent`.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The web path of a job page (`$lib/scope` `jobPath`): `/admin/jobs/<job>` + `sub`.
pub fn job_path(job: &str, sub: &str) -> String {
    format!("/admin/jobs/{}{sub}", encode(job))
}

/// A [`STEPS`] / [`DOR`] path: `job:<sub>` is [`job_path`], anything else is absolute.
pub fn step_href(job: &str, path: &str) -> String {
    match path.strip_prefix("job:") {
        Some(sub) => job_path(job, sub),
        None => path.to_string(),
    }
}

/// A step of [`STEPS`] / [`DOR`] with its label and link, and `status` / `detail`.
pub fn step(
    job: &str,
    key: &str,
    status: ReadinessStatus,
    detail: impl Into<String>,
) -> ReadinessStep {
    let found = STEPS.iter().chain(DOR.iter()).find(|(k, _, _)| *k == key);
    let (label, path): (&str, &str) = match found {
        Some(&(_, l, p)) => (l, p),
        None => (key, ""),
    };
    ReadinessStep {
        key: key.to_string(),
        label: label.to_string(),
        status,
        detail: detail.into(),
        href: step_href(job, path),
    }
}

/// `"case.create{return}@prod"` → `("case.create{return}", "prod")` (fixture `splitRoute`).
pub fn split_route(key: &str) -> (&str, &str) {
    match key.rfind('@') {
        Some(i) => (&key[..i], &key[i + 1..]),
        None => (key, ""),
    }
}

/// One item a publish refuses on (fixture `blockingItem`).
pub fn blocking_item(m: &MissingItem) -> ReadinessItem {
    if m.dependency == super::HIERARCHY_DEPENDENCY {
        return ReadinessItem {
            code: ReadinessCode::HierarchyIncomplete,
            subject: m.slot.clone(),
            message: m.detail.clone(),
            href: m.href.clone(),
        };
    }
    if m.dependency == super::KNOWLEDGE_DEPENDENCY {
        return ReadinessItem {
            code: ReadinessCode::KnowledgeUnbound,
            subject: m.slot.clone(),
            message: m.detail.clone(),
            href: m.href.clone(),
        };
    }
    if m.dependency == "routing" {
        let (op, env) = split_route(&m.slot);
        return ReadinessItem {
            code: ReadinessCode::RouteNotReady,
            subject: m.slot.clone(),
            message: format!("{op} has no tested route on {env}"),
            href: m.href.clone(),
        };
    }
    let slot = format!("{}.{}", m.dependency, m.slot);
    let tab = if m.dependency == "strata_ingest" {
        "ingest"
    } else {
        "knowledge"
    };
    ReadinessItem {
        code: ReadinessCode::SlotNotDeployed,
        message: format!("No provider deployed for {slot}"),
        subject: slot,
        href: format!("/admin/providers?tab={tab}"),
    }
}

/// The apply gap of one blocking item (fixture `buildBoard`, "apply").
fn apply_gap(m: &MissingItem, released: bool, causes: &BTreeMap<String, String>) -> ReadinessGap {
    let item = blocking_item(m);
    if m.dependency == super::HIERARCHY_DEPENDENCY {
        return ReadinessGap {
            key: format!("selection:{}", m.slot),
            area: AreaKey::Rules,
            subject: item.subject,
            title: item.message,
            why: "Every job sits in one industry and one or more of its departments (D106/D115); without them \
                  nothing compiles and apply refuses the release."
                .to_owned(),
            cause: None,
            blocks: Blocks::Apply,
            fixer: Fixer::TenantAdmin,
            fix_label: "Choose the industry and departments".to_owned(),
            href: item.href,
            enforced_from: Enforced::M1,
        };
    }
    if m.dependency == super::KNOWLEDGE_DEPENDENCY {
        return knowledge_gap(m, item);
    }
    if m.dependency == "routing" {
        let (op, env) = split_route(&m.slot);
        return ReadinessGap {
            key: format!("route:{}", m.slot),
            area: AreaKey::Routes,
            subject: item.subject,
            title: item.message,
            why: format!(
                "Every required operation needs a route to a real (non-stub) system that passed its contract test on {env}, or an explicit fallback. Without one the agent cannot do {op} there."
            ),
            cause: causes.get(&m.slot).cloned(),
            blocks: Blocks::Apply,
            fixer: Fixer::TenantAdmin,
            fix_label: format!("Route {op} on {env}"),
            href: item.href,
            enforced_from: Enforced::M4,
        };
    }
    let slot = format!("{}.{}", m.dependency, m.slot);
    let use_ = SLOT_USE
        .iter()
        .find(|(k, _)| *k == slot)
        .map(|(_, u)| u.to_string())
        .unwrap_or_else(|| format!("use {slot}"));
    let who = if released {
        "This release"
    } else {
        "Your selection"
    };
    ReadinessGap {
        key: format!("slot:{slot}"),
        area: AreaKey::Providers,
        subject: item.subject,
        title: item.message,
        why: format!(
            "{who} uses {slot} to {use_}. With no deployment behind the slot those calls fail, so apply refuses the release."
        ),
        cause: causes.get(&slot).cloned(),
        blocks: Blocks::Apply,
        fixer: Fixer::TenantAdmin,
        fix_label: format!("Pick a provider for {}", m.slot),
        href: item.href,
        enforced_from: Enforced::M1,
    }
}

/// `<department|job>:<stage>|<kind>[:<lang>]` (a [`super::KNOWLEDGE_DEPENDENCY`] slot) → `(scope, kind, lang)`.
pub fn split_knowledge(slot: &str) -> (&str, &str, Option<&str>) {
    let mut parts = slot.splitn(3, ':');
    let scope = parts.next().unwrap_or("");
    let key = parts.next().unwrap_or("");
    let kind = key.split_once('|').map_or(key, |(_, k)| k);
    (scope, kind, parts.next())
}

/// The apply gap of a required-knowledge item (D110/D115, KH T8).
fn knowledge_gap(m: &MissingItem, item: ReadinessItem) -> ReadinessGap {
    let (scope, kind, lang) = split_knowledge(&m.slot);
    let of = if scope == "job" {
        "the job".to_owned()
    } else {
        scope.to_owned()
    };
    let (why, fix_label) = match lang {
        Some(lang) => (
            format!(
                "{lang} is the job's primary language: every required kind needs documents in it before the job goes \
                 live (D110), or the agent has nothing to quote to most customers. Other languages only warn."
            ),
            format!("Add {kind} documents in {lang}"),
        ),
        None => (
            format!(
                "Every department's required knowledge must resolve to at least one source — a store of yours or a \
                 frozen platform pack; your own data (order status, delivery slots) needs your own source — before \
                 the job goes live (D110/D115). Without it the agent has nothing to cite for {of}, so apply refuses \
                 the release."
            ),
            format!("Bind {kind}"),
        ),
    };
    ReadinessGap {
        key: format!("knowledge:{}", m.slot),
        area: AreaKey::Knowledge,
        subject: item.subject,
        title: item.message,
        why,
        cause: None,
        blocks: Blocks::Apply,
        fixer: Fixer::TenantAdmin,
        fix_label,
        href: item.href,
        enforced_from: Enforced::M4,
    }
}

/// The testing gap of one Test-env item (fixture `buildBoard`, "testing").
fn testing_gap(m: &MissingItem, causes: &BTreeMap<String, String>) -> ReadinessGap {
    let (op, _) = split_route(&m.slot);
    ReadinessGap {
        key: format!("test-route:{}", m.slot),
        area: AreaKey::Routes,
        subject: m.slot.clone(),
        title: format!("{op} has no tested route in the Test env"),
        why: if m
            .detail
            .starts_with(crate::connectors::readiness::PROCEDURE_DETAIL)
        {
            // a procedure needs it: the item names the procedures (and says what the test chat does)
            m.detail.clone()
        } else {
            format!(
                "The test chat and regression runs use the Test env. Without a route there they cannot try {op}, so a draft change that needs it goes untested. Production and publishing are not affected (only Production routes block apply)."
            )
        },
        cause: causes.get(&m.slot).cloned(),
        blocks: Blocks::Testing,
        fixer: Fixer::TenantAdmin,
        fix_label: format!("Route {op} on test"),
        href: m.href.clone(),
        enforced_from: Enforced::M4,
    }
}

/// The static part of an area's board row (fixture `buildBoard` `areas`), `todo` / [`NOT_CHECKED`] with no counts.
/// A contributor starts from it and fills `ready`, `total`, `status`, `summary` and `template_part`.
pub fn area_shell(key: AreaKey, job: &str) -> ReadinessArea {
    let p = |sub: &str| job_path(job, sub);
    let (label, question, template_part, tenant_part, enforced, page, href) = match key {
        AreaKey::Providers => (
            "Provider slots",
            "Is every AI slot the release uses deployed?",
            "W5 slots, from the features you selected",
            "A provider per slot",
            Enforced::M1,
            "T6 Providers",
            "/admin/providers".to_string(),
        ),
        AreaKey::Routes => (
            "Operations routed",
            "Does every required operation reach a tested system?",
            "Required operations in Production",
            "A route to one of your systems, or a fallback",
            Enforced::M4,
            "J6 Operations & routes",
            p("/operations"),
        ),
        AreaKey::Tests => (
            "Contract tests",
            "Has each route been proven against the real system?",
            "T3 contract test per operation",
            "Run them; OK any prod write",
            Enforced::M4,
            "J6 Operations & routes",
            p("/operations"),
        ),
        AreaKey::Knowledge => (
            "Knowledge kinds",
            "Does every stage have the policies it must quote?",
            "Required kinds across the journey stages",
            "A knowledge store bound to each",
            Enforced::M4,
            "J10 Knowledge kinds",
            p("/knowledge-kinds"),
        ),
        AreaKey::Rules => (
            "Business rules",
            "Has every rule your policy decides got a value?",
            "Rule slots",
            "Your thresholds and wording",
            Enforced::M4,
            "J9 Business rules",
            p("/rules"),
        ),
        AreaKey::Gates => (
            "Gates",
            "Have you reviewed when the agent stops and asks?",
            "Gate questions",
            "One review; relabel, hide or tune thresholds",
            Enforced::M4,
            "J7 Gates",
            p("/gates"),
        ),
        AreaKey::Procedures => (
            "Procedures",
            "Is there a published procedure for what the agent does?",
            "Procedures",
            "Review and publish your versions",
            Enforced::M4,
            "J12 Procedures",
            p("/procedures"),
        ),
        AreaKey::Toolsets => (
            "Toolsets",
            "Can each agent role reach only the operations it needs?",
            "Agent roles with grants",
            "Narrow a grant (never widen)",
            Enforced::M4,
            "J8 Toolsets",
            p("/toolsets"),
        ),
    };
    ReadinessArea {
        key,
        label: label.to_string(),
        question: question.to_string(),
        template_part: template_part.to_string(),
        tenant_part: tenant_part.to_string(),
        ready: 0,
        total: 0,
        status: AreaStatus::Todo,
        summary: NOT_CHECKED.to_string(),
        enforced_from: enforced,
        page: page.to_string(),
        href,
    }
}

/// Fixture `buildBoard` verdict: nothing cut → `no_release`; an apply gap → `blocked`; a go-live gap →
/// `go_live_gaps`; else `ready`. Testing gaps never change it.
pub fn verdict(has_release: bool, gaps: &[ReadinessGap]) -> Verdict {
    if !has_release {
        Verdict::NoRelease
    } else if gaps.iter().any(|g| g.blocks == Blocks::Apply) {
        Verdict::Blocked
    } else if gaps.iter().any(|g| g.blocks == Blocks::GoLive) {
        Verdict::GoLiveGaps
    } else {
        Verdict::Ready
    }
}

/// The B2 step `go_live` (fixture `readinessOf`).
fn go_live_step(checked: &Checked, missing: &[MissingItem]) -> ReadinessStep {
    let job = &checked.job.key;
    let (status, detail) = match &checked.release {
        None => (
            ReadinessStatus::Todo,
            "No release yet — approve one on the Release page".to_string(),
        ),
        Some(r) if !missing.is_empty() => (
            ReadinessStatus::Blocked,
            format!(
                "{} would be refused: 409 release_not_ready ({} missing)",
                r.id,
                missing.len()
            ),
        ),
        Some(r) if checked.waiting() => (
            ReadinessStatus::Current,
            format!("{} is approved and ready to apply", r.id),
        ),
        Some(r) => (ReadinessStatus::Done, format!("{} is current", r.id)),
    };
    step(job, "go_live", status, detail)
}

/// The steps of `table` in order: B2's own (`template`, `go_live`, `operate`), else the first contributor step of
/// that key, else `todo` / [`NOT_CHECKED`].
fn merge_steps(
    checked: &Checked,
    table: &[(&str, &str, &str)],
    from: &[ReadinessStep],
    missing: &[MissingItem],
) -> Vec<ReadinessStep> {
    let job = &checked.job.key;
    table
        .iter()
        .map(|(key, _, _)| match *key {
            "template" if checked.pinned => {
                step(job, key, ReadinessStatus::Done, "Template pinned")
            }
            "template" => step(job, key, ReadinessStatus::Todo, "No template pinned"),
            "go_live" => go_live_step(checked, missing),
            "operate" if checked.current_id.is_some() => {
                step(job, key, ReadinessStatus::Done, "Live")
            }
            "operate" => step(job, key, ReadinessStatus::Todo, "Not live yet"),
            _ => from
                .iter()
                .find(|s| s.key == *key)
                .cloned()
                .unwrap_or_else(|| step(job, key, ReadinessStatus::Todo, NOT_CHECKED)),
        })
        .collect()
}

/// The board and checklist of `checked` from the contributors' findings (fixture `buildBoard` over `readinessOf`).
pub fn build(checked: &Checked, a: Assessed) -> Evaluation {
    let job = checked.job.key.clone();
    let released = checked.release.is_some();
    let mut gaps: Vec<ReadinessGap> = Vec::new();
    // apply: the 409 release_not_ready list, one gap per item, in the same order
    gaps.extend(
        a.missing
            .iter()
            .map(|m| apply_gap(m, released, &checked.causes)),
    );
    // testing: Test-env routes missing; publishing is not blocked
    gaps.extend(a.test_env.iter().map(|m| testing_gap(m, &checked.causes)));
    // go-live: apply passes, the agent would run with these holes (contributors in AREA_ORDER)
    gaps.extend(a.go_live);
    let areas: Vec<ReadinessArea> = AREA_ORDER
        .iter()
        .map(|k| {
            a.areas
                .iter()
                .find(|x| x.key == *k)
                .cloned()
                .unwrap_or_else(|| area_shell(*k, &job))
        })
        .collect();
    let steps = merge_steps(checked, &STEPS, &a.steps, &a.missing);
    let definition_of_ready = merge_steps(checked, &DOR, &a.steps, &a.missing);
    let board = ReadinessBoard {
        job,
        release: checked.release.clone(),
        current_id: checked.current_id.clone(),
        verdict: verdict(released, &gaps),
        gaps,
        areas,
        steps,
    };
    Evaluation {
        board,
        missing: a.missing,
        test_env: a.test_env,
        definition_of_ready,
    }
}

/// The warning code of a non-apply gap (fixture `GAP_CODE`, `test_route_not_ready` for a testing gap).
fn warning_code(g: &ReadinessGap) -> ReadinessCode {
    if g.blocks == Blocks::Testing {
        return ReadinessCode::TestRouteNotReady;
    }
    match g.area {
        AreaKey::Knowledge => ReadinessCode::KnowledgeUnbound,
        AreaKey::Rules => ReadinessCode::RulePlaceholder,
        AreaKey::Gates => ReadinessCode::GatesUnreviewed,
        AreaKey::Procedures => ReadinessCode::ProcedureUnpublished,
        _ => ReadinessCode::ContractTestsPending,
    }
}

/// The `ReleaseReadiness` contract from the board (fixture `releaseReadinessOf`).
pub fn release_readiness(b: &ReadinessBoard, missing: &[MissingItem]) -> ReleaseReadiness {
    let blocking: Vec<ReadinessItem> = missing.iter().map(blocking_item).collect();
    let warnings: Vec<ReadinessItem> = b
        .gaps
        .iter()
        .filter(|g| g.blocks != Blocks::Apply)
        .map(|g| ReadinessItem {
            code: warning_code(g),
            subject: g.subject.clone(),
            message: g.title.clone(),
            href: g.href.clone(),
        })
        .collect();
    ReleaseReadiness {
        job: b.job.clone(),
        release_id: b.release.as_ref().map(|r| r.id.clone()),
        routing_ready: !blocking
            .iter()
            .any(|i| i.code == ReadinessCode::RouteNotReady),
        blocking,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_route_takes_the_last_at() {
        assert_eq!(split_route("case.read@prod"), ("case.read", "prod"));
        assert_eq!(split_route("a@b@test"), ("a@b", "test"));
        assert_eq!(split_route("case.read"), ("case.read", ""));
    }

    #[test]
    fn blocking_item_codes() {
        let slot = MissingItem {
            dependency: "strata_ingest".into(),
            slot: "vision".into(),
            detail: "W5 slot strata_ingest.vision has no deployment".into(),
            href: "/admin/providers".into(),
        };
        let i = blocking_item(&slot);
        assert_eq!(i.code, ReadinessCode::SlotNotDeployed);
        assert_eq!(i.subject, "strata_ingest.vision");
        assert_eq!(i.message, "No provider deployed for strata_ingest.vision");
        assert_eq!(i.href, "/admin/providers?tab=ingest");
        let route = MissingItem {
            dependency: "routing".into(),
            slot: "case.read@prod".into(),
            detail: String::new(),
            href: "/admin/jobs/j1/operations".into(),
        };
        let i = blocking_item(&route);
        assert_eq!(i.code, ReadinessCode::RouteNotReady);
        assert_eq!(i.subject, "case.read@prod");
        assert_eq!(i.message, "case.read has no tested route on prod");
        assert_eq!(i.href, "/admin/jobs/j1/operations");
    }

    #[test]
    fn knowledge_items_name_the_department_and_kind() {
        let m = MissingItem {
            dependency: "knowledge".into(),
            slot: "industry_dept/retail/care:support|policy.returns".into(),
            detail: "Required knowledge kind policy.returns (Support stage) of department \
                     industry_dept/retail/care has no knowledge: bind it to a store or pin a platform pack."
                .into(),
            href: "/admin/jobs/j1/knowledge-kinds".into(),
        };
        let i = blocking_item(&m);
        assert_eq!(i.code, ReadinessCode::KnowledgeUnbound);
        assert_eq!(
            i.subject,
            "industry_dept/retail/care:support|policy.returns"
        );
        assert_eq!(i.href, "/admin/jobs/j1/knowledge-kinds");
        let g = apply_gap(&m, true, &BTreeMap::new());
        assert_eq!(g.area, AreaKey::Knowledge);
        assert_eq!(g.blocks, Blocks::Apply);
        assert_eq!(g.fix_label, "Bind policy.returns");
        assert!(g.why.contains("industry_dept/retail/care"), "{}", g.why);
        assert_eq!(
            split_knowledge("job:*|faq.general:nl"),
            ("job", "faq.general", Some("nl"))
        );
    }

    #[test]
    fn job_path_encodes_like_encode_uri_component() {
        assert_eq!(
            job_path("customer_care_agent", "/rules"),
            "/admin/jobs/customer_care_agent/rules"
        );
        assert_eq!(job_path("a b/c", ""), "/admin/jobs/a%20b%2Fc");
    }
}
