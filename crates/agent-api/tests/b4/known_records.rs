//! B4 known customer, four industries (owner ruling 2026-10-26: "I know my customer"): a KYC'd customer is NEVER
//! asked for an id. Each test drives a request qualified for a verified party (level 2) through the real qualifier
//! fold (`qualify::step` over the job's seeded spaces) and the real orchestrator (context phase, `slots` step) over
//! Postgres 18 + Redis, with a fake runner that answers the context reads (`*.list`) with columnar data.
//!
//! | Industry | Job template | Space | Layer |
//! |---|---|---|---|
//! | insurance | `job/insurance_customer_care_agent` | `insurance:cover:coverage_question`, `insurance:claims:claim_status` | orchestrator |
//! | healthcare | `job/healthcare_customer_care_agent` | `healthcare:prescriptions:prescription_status` | orchestrator |
//! | HR | none (no job compiles `base_dept/hr`) | `hr:pay:payslip_question` | `qualify::step` + `resolve_records` |
//! | billing | `job/billing_assistant` | `billing:charges:dispute_a_charge` | orchestrator |
//! | IT desk | none (no seeded IT space declares a ticket slot) | `ticket_id` / `case_id` kinds | `resolve_records` + `record_choice_question` |
//!
//! Every orchestrator test asserts: (1) the qualifier folds the request to `Outcome::Ready` and never `Outcome::Ask`
//! for a registered or lookup-key slot, and an id typed by the customer is not kept as a slot; (2) each list read was
//! called once, with the verified party as its only input; (3) no reply or choice text holds an id-shaped token or
//! asks for a number / reference / id.
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use agent_api::design::DesignOf;
use agent_api::runtime::customer_context::{self as cc, CustomerContext};
use agent_api::runtime::design::RuntimeDesign;
use agent_api::runtime::domain::{CaseOutcome, OpCall, OpState, QualifiedRequest, Resolution};
use agent_api::runtime::orchestrator::{DesignProcedures, Orchestrator};
use agent_api::runtime::reply;
use agent_api::runtime::resolve::{self, Resolved, contains_id_pattern, record_choice_question};
use agent_api::runtime::spaces::{ContextSpace, SpaceCatalog};
use agent_api::runtime::{Env, Scope};
use agent_api::turn::qualify::{self, Draft, Extraction, Outcome, never_asked};
use serde_json::{Value, json};

use crate::b4::design::Fx as DesignFx;
use crate::b4::flow::{self as fl, EveryOpOffered, Fx};
use crate::b4::it_desk_identity::OwnSpace;
use crate::common;
use crate::redis_env as r;

/// The verified party of every case here: the only value any list read is keyed by.
const PARTY: &str = "cust-1";

/// What the fake runner answers a context read with.
enum Read {
    /// An ok answer (columnar rows).
    Rows(Value),
    /// The operation is not offered by the tenant's connector (a refused read: `degraded`, no rows).
    NotOffered,
}

type World<'a> = Vec<(&'a str, Read)>;

/// A job on a job layer of the test's own: `spaces` (each answered from knowledge, so a case in it has a plan without
/// any procedure) and the catalogue operations `ops` (the record lists the context phase reads), on a fresh tenant,
/// an `agent_runtime` pool, a scope in the test environment on a fresh Redis prefix, and the orchestrator with every
/// operation routed.
struct Desk {
    fx: Fx,
    o: Orchestrator<DesignProcedures>,
    design: RuntimeDesign,
}

/// A space of the test's own: neutral keys, the lookup-key slots a real job declares for that record kind.
const fn space(key: &'static str, slots: &'static [&'static str]) -> OwnSpace {
    OwnSpace {
        key,
        slots,
        kinds: &["faq"],
    }
}

async fn desk(test: &str, spaces: &[OwnSpace], ops: &[&str]) -> Desk {
    let e = common::env().await;
    // the operations first: the job layer's tag pins the base tag that holds them
    crate::b4::support::ops(&e, ops).await;
    let f = crate::b4::it_desk_identity::own_job(&e, "known_desk", spaces, false).await;
    let sub = format!("u-b4-known-{}", f.tenant.simple());
    common::insert_user(&e.owner, &sub, f.tenant, "tenant_admin").await;
    let tok = common::person(&sub, &[]);
    let d = DesignFx { e, f, tok };
    let root = r::root(test).await;
    let scope = Scope::new(d.f.tenant, d.f.job.job_key.clone(), Env::Test).unwrap();
    let bus = root.scoped(&scope);
    let rt = common::runtime_pool(&d.e.owner).await;
    let fx = Fx {
        d,
        root,
        scope,
        bus,
        rt,
    };
    let design = {
        let mut tx = fx.d.f.job.begin(&fx.d.e.state.db).await.unwrap();
        let d = RuntimeDesign::load(
            &mut tx,
            &fx.d.e.state.design,
            &fx.d.f.job_ref(),
            &DesignOf::Draft,
        )
        .await
        .unwrap_or_else(|e| panic!("load: {e:?}"));
        tx.commit().await.unwrap();
        d
    };
    let o = fl::orch(&fx, DesignProcedures).with_routes(Arc::new(EveryOpOffered));
    Desk { fx, o, design }
}

/// The qualifier's fold of one customer turn into `space`, as `said` (slot name, value) extracted by the model, over
/// the job's seeded spaces. Asserts the "never asked" contract and returns the ready request.
fn qualify_ready(d: &RuntimeDesign, space: &str, said: &[(&str, &str)]) -> Draft {
    qualify_in(&d.spaces, space, said)
}

/// [`qualify_ready`] over any space catalogue (a job's compiled one, or a hand-built one).
fn qualify_in(spaces: &SpaceCatalog, space: &str, said: &[(&str, &str)]) -> Draft {
    let sp = spaces.by_key(space).unwrap_or_else(|| {
        panic!(
            "{space} is not a space of this job: {:?}",
            spaces.spaces().iter().map(|s| &s.key).collect::<Vec<_>>()
        )
    });
    let keys: Vec<String> = sp
        .slots
        .iter()
        .filter(|s| never_asked(s))
        .cloned()
        .collect();
    assert!(
        !keys.is_empty(),
        "{space} declares no lookup key: {:?}",
        sp.slots
    );

    // the customer types an id for every key the space declares: never kept as a slot, never asked back
    let typed = Extraction {
        context_space: Some(space.to_owned()),
        slots: keys
            .iter()
            .map(|k| (k.clone(), "ZZ-99871".to_owned()))
            .collect(),
    };
    let (out, kept) = qualify::step(None, typed, spaces, "m-typed");
    let slots = match (&out, kept) {
        (Outcome::Ask(s), Some(d)) => {
            assert!(!never_asked(s), "{space}: asked for the key {s}");
            d.slots
        }
        (Outcome::Ready(d), _) => d.slots.clone(),
        other => panic!("{space}: unexpected {other:?}"),
    };
    for k in &keys {
        assert!(
            !slots.contains_key(k),
            "{space}: a typed id was kept as {k}"
        );
    }

    let ex = Extraction {
        context_space: Some(space.to_owned()),
        slots: said
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    };
    match qualify::step(None, ex, spaces, "m-1").0 {
        Outcome::Ready(draft) => draft,
        Outcome::Ask(s) => panic!("{space}: the qualifier asked the known customer for {s}"),
        Outcome::Unclear => panic!("{space}: not recognised"),
    }
}

fn request(d: &RuntimeDesign, draft: &Draft, case: &str) -> QualifiedRequest {
    let sp = d.spaces.by_key(&draft.context_space).unwrap();
    let area = if sp.area.is_empty() {
        draft.context_space.split(':').nth(1).unwrap().to_owned()
    } else {
        sp.area.clone()
    };
    serde_json::from_value(json!({
        "case_id": case, "conversation_id": format!("conv-{case}"), "area": area,
        "context_space": draft.context_space,
        "request_type": draft.context_space.rsplit(':').next().unwrap(),
        "slots": draft.slots, "customer_id": PARTY, "identity_level": 2,
        "message_id": format!("msg-{case}")
    }))
    .unwrap()
}

/// Everything one driven case left behind.
struct Run {
    case: String,
    calls: Vec<OpCall>,
    resolutions: Vec<Resolution>,
    /// The case hash's `slots:resolved` (what the `slots` step filled), `None` while the case parked or ended.
    resolved: Option<BTreeMap<String, String>>,
}

impl Run {
    fn reads(&self, op: &str) -> Vec<&OpCall> {
        self.calls
            .iter()
            .filter(|c| c.operation == op && c.provenance.as_deref() == Some("context"))
            .collect()
    }

    /// The one resolution the case ended or parked with.
    fn only(&self) -> &Resolution {
        assert_eq!(
            self.resolutions.len(),
            1,
            "{}: {:?}",
            self.case,
            self.resolutions
        );
        &self.resolutions[0]
    }

    fn reply(&self) -> String {
        reply::compose(self.only())
    }
}

/// Qualify, run `case` for the verified party and answer every context read from `world` (anything not listed is an
/// ok, empty answer: a real "none").
async fn drive(
    desk: &Desk,
    case: &str,
    space: &str,
    said: &[(&str, &str)],
    world: &World<'_>,
) -> Run {
    let fx = &desk.fx;
    let draft = qualify_ready(&desk.design, space, said);
    let req = request(&desk.design, &draft, case);
    fl::insert_conversation(fx, &req.conversation_id).await;
    fl::insert_case(fx, case, &fl::current_design(fx).await).await;
    // a known customer: the party is on the conversation's state before the first message is planned
    fl::identify_conversation(fx, &req.conversation_id, PARTY, None).await;
    desk.o.on_qualified(fl::qualified(&req)).await.unwrap();

    let mut done: HashSet<String> = HashSet::new();
    for _ in 0..10 {
        let todo: Vec<OpCall> = fl::calls(fx, case)
            .await
            .into_iter()
            .filter(|c| c.provenance.as_deref() == Some("context") && !done.contains(&c.call_id))
            .collect();
        if todo.is_empty() {
            break;
        }
        for c in todo {
            done.insert(c.call_id.clone());
            let (state, code, out) = match world.iter().find(|(op, _)| *op == c.operation) {
                Some((_, Read::Rows(v))) => (OpState::Ok, "ok", v.clone()),
                Some((_, Read::NotOffered)) => (OpState::Denied, "not_offered", json!({})),
                None => (OpState::Ok, "ok", json!({})),
            };
            desk.o
                .on_result(fl::answer(&c, state, code, out))
                .await
                .unwrap();
        }
    }

    let key = desk.o.cases().key(case);
    let raw: Option<String> = redis::cmd("HGET")
        .arg(&key)
        .arg("slots:resolved")
        .query_async(&mut fx.bus.conn())
        .await
        .unwrap();
    Run {
        case: case.to_owned(),
        calls: fl::calls(fx, case).await,
        resolutions: fl::resolutions_in(fx, &req.area, case).await,
        resolved: raw.map(|r| serde_json::from_str(&r).unwrap()),
    }
}

/// Words that would ask the customer for a lookup key.
const KEY_WORDS: [&str; 8] = [
    "number",
    "reference",
    "ref",
    "id",
    "postcode",
    "email",
    "code",
    "identifier",
];

/// The contract every test shares: the list reads were keyed by the verified party alone, and nothing the customer
/// reads holds an id or asks for one.
fn assert_never_asked(run: &Run, list_ops: &[&str]) {
    for op in list_ops {
        let reads = run.reads(op);
        assert_eq!(reads.len(), 1, "{op} is read once: {:?}", run.calls);
        assert_eq!(
            reads[0].input.get("party_id").and_then(Value::as_str),
            Some(PARTY),
            "{op}: keyed by the verified party"
        );
        assert_eq!(reads[0].input.len(), 1, "{op}: nothing but the party");
    }
    for c in run
        .calls
        .iter()
        .filter(|c| c.provenance.as_deref() == Some("context") && c.operation.ends_with(".list"))
    {
        assert_eq!(
            c.input.get("party_id").and_then(Value::as_str),
            Some(PARTY),
            "{}: a list read is keyed by the verified party",
            c.operation
        );
    }
    for res in &run.resolutions {
        let text = reply::compose(res);
        assert!(!contains_id_pattern(&text), "{text}");
        assert!(
            !contains_id_pattern(res.reason.as_deref().unwrap_or_default()),
            "{:?}",
            res.reason
        );
        let low = text.to_lowercase();
        let words: Vec<&str> = low
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        for k in KEY_WORDS {
            assert!(
                !words.contains(&k),
                "{}: asks for a key ({k}): {text}",
                run.case
            );
        }
    }
}

fn expect_filled(run: &Run, want: &[(&str, &str)]) {
    assert!(
        run.resolutions.is_empty(),
        "nothing is asked or answered when one record fits: {:?}",
        run.resolutions
    );
    let want: BTreeMap<String, String> = want
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect();
    assert_eq!(run.resolved.as_ref(), Some(&want), "{}", run.case);
}

fn expect_choice(run: &Run, question: &str) {
    assert!(run.resolved.is_none(), "a parked case fills nothing");
    let res = run.only();
    assert_eq!(res.outcome, CaseOutcome::ConfirmationRequired);
    assert_eq!(res.code.as_deref(), Some("choice_required"));
    assert_eq!(run.reply(), question);
}

fn expect_no_match(run: &Run, noun: &str) {
    assert!(run.resolved.is_none());
    let res = run.only();
    assert_eq!(res.outcome, CaseOutcome::NotEligible);
    assert_eq!(res.code.as_deref(), Some("no_matching_record"));
    assert_eq!(res.reason.as_deref(), Some(noun));
}

// ---- insurance ---------------------------------------------------------------------------------------------------

mod insurance {
    use super::*;

    const COVER: &str = "insurance:cover:coverage_question";
    const CLAIM: &str = "insurance:claims:claim_status";
    const OPS: [&str; 2] = ["policy.list", "claim.list"];
    const SPACES: [OwnSpace; 2] = [
        space(COVER, &["policy_id", "loss_type"]),
        space(CLAIM, &["claim_number"]),
    ];
    /// The qualifier's extraction for the coverage question: the loss is the customer's to give, the policy is not.
    const LOSS: [(&str, &str); 1] = [("loss_type", "water damage")];

    fn one_policy() -> World<'static> {
        vec![(
            "policy.list",
            Read::Rows(json!({
                "policy_id": ["POL-5521"], "line_of_business": ["Car insurance"],
                "renewal_date": ["2026-03-03"],
            })),
        )]
    }

    fn two_policies() -> World<'static> {
        vec![(
            "policy.list",
            Read::Rows(json!({
                "policy_id": ["POL-5521", "POL-7730"],
                "line_of_business": ["Car insurance", "Home insurance"],
                "renewal_date": ["2026-03-03", "2026-05-01"],
            })),
        )]
    }

    /// One policy: the slot is filled from the customer's own record, nobody is asked. Layer: orchestrator.
    #[tokio::test]
    async fn one_policy_is_filled_without_a_question() {
        let d = desk("known-ins-one", &SPACES, &OPS).await;
        let run = drive(&d, "CS-1", COVER, &LOSS, &one_policy()).await;
        assert_never_asked(&run, &["policy.list"]);
        expect_filled(&run, &[("policy_id", "POL-5521")]);
    }

    /// Two policies: the question names both in plain words (line of business, renewal date), oldest first, never an
    /// id; the choice is parked server-side. Layer: orchestrator.
    #[tokio::test]
    async fn two_policies_are_a_question_in_plain_words() {
        let d = desk("known-ins-two", &SPACES, &OPS).await;
        let run = drive(&d, "CS-1", COVER, &LOSS, &two_policies()).await;
        assert_never_asked(&run, &["policy.list"]);
        expect_choice(
            &run,
            "Is this about your Car insurance renewing 3 March or your Home insurance renewing 1 May?",
        );
        let text = run.reply();
        assert!(!text.contains("POL-"), "{text}");
    }

    /// "my home insurance" picks the home policy: filled, no question. Layer: orchestrator.
    #[tokio::test]
    async fn the_hint_my_home_insurance_picks_one_policy() {
        let d = desk("known-ins-hint", &SPACES, &OPS).await;
        let said = [LOSS[0], ("item_hint", "my home insurance")];
        let run = drive(&d, "CS-1", COVER, &said, &two_policies()).await;
        assert_never_asked(&run, &["policy.list"]);
        expect_filled(&run, &[("policy_id", "POL-7730")]);
    }

    /// No policy on the account: a plain sentence, no key asked, no step run. Layer: orchestrator.
    #[tokio::test]
    async fn no_policy_says_so_in_plain_words() {
        let d = desk("known-ins-none", &SPACES, &OPS).await;
        let world: World = vec![("policy.list", Read::Rows(json!({})))];
        let run = drive(&d, "CS-1", COVER, &LOSS, &world).await;
        assert_never_asked(&run, &["policy.list"]);
        expect_no_match(&run, "policy");
        assert_eq!(run.reply(), "I can't find a policy on your account.");
    }

    /// The connector offers no `policy.list`: the records cannot be read, so a colleague takes the case
    /// (`unresolvable_slot`) and the customer is not asked. Layer: orchestrator.
    #[tokio::test]
    async fn an_unrouted_policy_list_hands_over_and_never_asks() {
        let d = desk("known-ins-unrouted", &SPACES, &OPS).await;
        let world: World = vec![("policy.list", Read::NotOffered)];
        let run = drive(&d, "CS-1", COVER, &LOSS, &world).await;
        assert_never_asked(&run, &["policy.list"]);
        assert!(run.resolved.is_none());
        let res = run.only();
        assert_eq!(res.outcome, CaseOutcome::Handover);
        assert_eq!(res.code.as_deref(), Some("unresolvable_slot"));
        assert_eq!(fl::handover_items(&d.fx, "CS-1").await.len(), 1);
    }

    /// `claim_number` is a lookup key too: one claim fills it. Layer: orchestrator.
    #[tokio::test]
    async fn one_claim_is_filled_without_a_question() {
        let d = desk("known-ins-claim-one", &SPACES, &OPS).await;
        let world: World = vec![(
            "claim.list",
            Read::Rows(json!({
                "claim_number": ["CLM-2026-0412"], "loss_type": ["Water damage"],
                "reported_at": ["2026-09-12"],
            })),
        )];
        let run = drive(&d, "CS-1", CLAIM, &[], &world).await;
        assert_never_asked(&run, &["claim.list"]);
        expect_filled(&run, &[("claim_number", "CLM-2026-0412")]);
    }

    /// Two claims: named by loss and report date. Layer: orchestrator.
    #[tokio::test]
    async fn two_claims_are_named_by_loss_and_date() {
        let d = desk("known-ins-claim-two", &SPACES, &OPS).await;
        let world: World = vec![(
            "claim.list",
            Read::Rows(json!({
                "claim_number": ["CLM-2026-0412", "CLM-2026-0533"],
                "loss_type": ["Water damage", "Theft"],
                "reported_at": ["2026-09-12", "2026-10-03"],
            })),
        )];
        let run = drive(&d, "CS-1", CLAIM, &[], &world).await;
        assert_never_asked(&run, &["claim.list"]);
        expect_choice(
            &run,
            "Is this about your Water damage claim reported 12 September or your Theft claim reported 3 October?",
        );
    }
}

// ---- healthcare (the Oakline shape) ------------------------------------------------------------------------------

mod healthcare {
    use super::*;

    const STATUS: &str = "healthcare:prescriptions:prescription_status";
    const OPS: [&str; 1] = ["prescription.list"];
    const SPACES: [OwnSpace; 1] = [space(STATUS, &["prescription_ref"])];

    fn one() -> World<'static> {
        vec![(
            "prescription.list",
            Read::Rows(json!({"prescription_id": ["RX-40117"], "medication": ["Amoxicillin"]})),
        )]
    }

    fn two() -> World<'static> {
        vec![(
            "prescription.list",
            Read::Rows(json!({
                "prescription_id": ["RX-40117", "RX-40298"],
                "medication": ["Amoxicillin", "Ramipril"],
            })),
        )]
    }

    /// One prescription: filled, nobody is asked. Layer: orchestrator.
    #[tokio::test]
    async fn one_prescription_is_filled_without_a_question() {
        let d = desk("known-hc-one", &SPACES, &OPS).await;
        let run = drive(&d, "CS-1", STATUS, &[], &one()).await;
        assert_never_asked(&run, &["prescription.list"]);
        expect_filled(&run, &[("prescription_ref", "RX-40117")]);
    }

    /// Several prescriptions: told apart by medication, never by reference. Layer: orchestrator.
    #[tokio::test]
    async fn several_prescriptions_are_named_by_medication() {
        let d = desk("known-hc-two", &SPACES, &OPS).await;
        let run = drive(&d, "CS-1", STATUS, &[], &two()).await;
        assert_never_asked(&run, &["prescription.list"]);
        expect_choice(
            &run,
            "Is this about your Amoxicillin prescription or your Ramipril prescription?",
        );
    }

    /// "my amoxicillin" picks the one prescription: filled, no question. Layer: orchestrator.
    #[tokio::test]
    async fn the_hint_my_amoxicillin_picks_one_prescription() {
        let d = desk("known-hc-hint", &SPACES, &OPS).await;
        let said = [("item_hint", "my amoxicillin")];
        let run = drive(&d, "CS-1", STATUS, &said, &two()).await;
        assert_never_asked(&run, &["prescription.list"]);
        expect_filled(&run, &[("prescription_ref", "RX-40117")]);
    }

    /// A pharmacy read the connector refuses: handover, not a question. Layer: orchestrator.
    #[tokio::test]
    async fn an_unrouted_prescription_list_hands_over_and_never_asks() {
        let d = desk("known-hc-unrouted", &SPACES, &OPS).await;
        let world: World = vec![("prescription.list", Read::NotOffered)];
        let run = drive(&d, "CS-1", STATUS, &[], &world).await;
        assert_never_asked(&run, &["prescription.list"]);
        let res = run.only();
        assert_eq!(res.outcome, CaseOutcome::Handover);
        assert_eq!(res.code.as_deref(), Some("unresolvable_slot"));
    }
}

// ---- HR ----------------------------------------------------------------------------------------------------------

mod hr {
    use super::*;

    // No seeded job compiles `base_dept/hr`: a department's `base_dept` layer folds only for a selected
    // `industry_dept/<industry>/<dept>` that `extends` it, and no seeded department extends `hr` (only billing,
    // customer_service and it_service_desk are extended). So `hr:pay:payslip_question` (seed v5, layer
    // `base_dept/hr`, slots `payslip_id` + `pay_line`) is a space of no job design, and its `payslip.list`
    // operation is in no job catalogue. These tests therefore drive the layer below the orchestrator, like the IT
    // desk tests: the real qualifier fold (`qualify::step`) over a catalogue holding the space exactly as seeded, and
    // the real resolver (`resolve_records`, `record_choice_question`) over a context parsed from the connector's
    // columnar `payslip.list` answer.
    const PAYSLIP: &str = "hr:pay:payslip_question";
    /// The pay line is the employee's to name; the payslip is not.
    const LINE: (&str, &str) = ("pay_line", "overtime");

    pub(super) fn space() -> SpaceCatalog {
        SpaceCatalog::new(vec![ContextSpace {
            key: PAYSLIP.to_owned(),
            label: "Payslip or pay question".to_owned(),
            description: String::new(),
            area: "pay".to_owned(),
            knowledge_kinds: vec![
                "guide.payslip".to_owned(),
                "employment.pay_transparency".to_owned(),
            ],
            slots: vec!["payslip_id".to_owned(), "pay_line".to_owned()],
            activity: String::new(),
        }])
    }

    pub(super) fn today() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 27).unwrap()
    }

    /// The employee's context once `payslip.list` answered, parsed by the code the orchestrator uses.
    pub(super) fn payslips() -> CustomerContext {
        let answer = json!({
            "payslip_id": ["PS-202608", "PS-202609"],
            "period": ["August 2026", "September 2026"],
            "issue_date": ["2026-08-28", "2026-09-25"],
        });
        let mut c = CustomerContext {
            party: PARTY.to_owned(),
            ..CustomerContext::default()
        };
        c.records.insert(
            "payslip.list".to_owned(),
            cc::parse_records(answer.as_object().unwrap()),
        );
        c
    }

    /// The hint the qualifier keeps for `said`, after asserting the payslip id is never asked or kept.
    fn hint_of(said: &[(&str, &str)]) -> Option<String> {
        let draft = qualify_in(&space(), PAYSLIP, said);
        assert!(!draft.slots.contains_key("payslip_id"), "{:?}", draft.slots);
        assert_eq!(
            draft.slots.get("pay_line").map(String::as_str),
            Some("overtime")
        );
        draft.slots.get(resolve::HINT_SLOT).cloned()
    }

    /// "my September payslip" picks the September payslip by its period: filled, no question.
    /// Layer: qualify::step + resolve_records.
    #[test]
    fn my_september_payslip_is_picked_by_period() {
        let hint = hint_of(&[LINE, ("item_hint", "my September payslip")]);
        let kind = resolve::record_kind("payslip_id").unwrap();
        match resolve::resolve_records(kind, &payslips(), hint.as_deref(), today()) {
            Resolved::One(p) => assert_eq!(p.order_id, "PS-202609"),
            other => panic!("{other:?}"),
        }
    }

    /// "the payslip from 25 September" picks it by its issue date. Layer: qualify::step + resolve_records.
    #[test]
    fn the_payslip_issued_on_a_date_is_picked_by_date() {
        let hint = hint_of(&[LINE, ("item_hint", "the payslip from 25 September")]);
        let kind = resolve::record_kind("payslip_id").unwrap();
        match resolve::resolve_records(kind, &payslips(), hint.as_deref(), today()) {
            Resolved::One(p) => assert_eq!(p.order_id, "PS-202609"),
            other => panic!("{other:?}"),
        }
    }

    /// No hint: the question names both payslips by period and issue date, never by id.
    /// Layer: qualify::step + resolve_records + record_choice_question.
    #[test]
    fn two_payslips_are_named_by_period_and_date() {
        let hint = hint_of(&[LINE]);
        assert!(hint.is_none(), "{hint:?}");
        let kind = resolve::record_kind("payslip_id").unwrap();
        let Resolved::Ambiguous { picks, more } =
            resolve::resolve_records(kind, &payslips(), None, today())
        else {
            panic!("two payslips fit")
        };
        assert!(!more);
        let q = record_choice_question(kind, &picks, more);
        assert_eq!(
            q,
            "Is this about your August 2026 payslip issued 28 August or your September 2026 payslip issued 25 September?"
        );
        assert!(!contains_id_pattern(&q), "{q}");
        assert!(!q.contains("PS-"), "{q}");
        // the answer maps back to the payslip's value for the slot, server-side
        assert_eq!(
            picks[1]
                .slots(&["payslip_id"])
                .get("payslip_id")
                .map(String::as_str),
            Some("PS-202609")
        );
    }
}

mod hr_customer {
    use super::hr::{payslips, space, today};
    use super::*;

    const PAYSLIP: &str = "hr:pay:payslip_question";

    /// A customer-audience conversation reaching the payslip space never asks for a payslip id: the qualifier skips
    /// the key and keeps nothing a customer typed; with the list not offered (degraded) the records are unavailable
    /// (the orchestrator hands over with `unresolvable_slot`, no question), with an empty list the answer is none
    /// (`no_matching_record`). Nothing the resolver builds holds an id. (The orchestrator's own handover cannot be
    /// driven here: no job design holds this space, see mod `hr`.) Layer: qualify::step + resolve_records.
    #[test]
    fn a_customer_job_never_asks_for_a_payslip_id_and_hands_over() {
        let d = qualify_in(&space(), PAYSLIP, &[("pay_line", "overtime")]);
        assert!(!d.slots.contains_key("payslip_id"), "{:?}", d.slots);
        let kind = resolve::record_kind("payslip_id").unwrap();
        assert!(never_asked("payslip_id"));

        // not offered: the read was refused, so the records are unavailable (handover), never a question
        let refused = CustomerContext {
            party: PARTY.to_owned(),
            degraded: vec!["payslip.list".to_owned()],
            ..CustomerContext::default()
        };
        assert!(!resolve::records_available(kind, &refused));

        // an empty list is a real "none"
        let mut empty = CustomerContext {
            party: PARTY.to_owned(),
            ..CustomerContext::default()
        };
        empty.records.insert(
            "payslip.list".to_owned(),
            cc::parse_records(
                json!({"payslip_id": [], "period": [], "issue_date": []})
                    .as_object()
                    .unwrap(),
            ),
        );
        assert!(resolve::records_available(kind, &empty));
        assert!(matches!(
            resolve::resolve_records(kind, &empty, None, today()),
            Resolved::Nothing
        ));

        // with records, the choice is in plain words and holds no id
        let Resolved::Ambiguous { picks, more } =
            resolve::resolve_records(kind, &payslips(), None, today())
        else {
            panic!("two payslips fit")
        };
        let q = record_choice_question(kind, &picks, more);
        assert!(!contains_id_pattern(&q), "{q}");
    }
}

// ---- billing -----------------------------------------------------------------------------------------------------

mod billing {
    use super::*;

    const DISPUTE: &str = "billing:charges:dispute_a_charge";
    const OPS: [&str; 2] = ["account.list", "invoice.list"];
    const SPACES: [OwnSpace; 1] = [space(DISPUTE, &["account_ref", "document_ref"])];

    fn one_account() -> (&'static str, Read) {
        (
            "account.list",
            Read::Rows(json!({
                "account_id": ["ACC-3301"], "account_ref": ["ACC-3301"],
                "kind": ["Energy"], "opened_at": ["2024-02-01"],
            })),
        )
    }

    /// One account and one invoice: both slots are filled, nobody is asked. Layer: orchestrator.
    #[tokio::test]
    async fn one_account_and_one_invoice_are_filled_automatically() {
        let d = desk("known-bill-one", &SPACES, &OPS).await;
        let world: World = vec![
            one_account(),
            (
                "invoice.list",
                Read::Rows(json!({
                    "document_ref": ["INV-88210"], "kind": ["Gas"], "issue_date": ["2026-09-03"],
                })),
            ),
        ];
        let run = drive(&d, "CS-1", DISPUTE, &[], &world).await;
        assert_never_asked(&run, &["account.list", "invoice.list"]);
        expect_filled(
            &run,
            &[("account_ref", "ACC-3301"), ("document_ref", "INV-88210")],
        );
    }

    /// The one account is filled silently; two invoices are a question by kind and date, oldest first, that never
    /// mentions the account. Layer: orchestrator.
    #[tokio::test]
    async fn two_invoices_are_a_question_by_kind_and_date() {
        let d = desk("known-bill-two", &SPACES, &OPS).await;
        let world: World = vec![
            one_account(),
            (
                "invoice.list",
                Read::Rows(json!({
                    "document_ref": ["INV-88210", "INV-90417"],
                    "kind": ["Gas", "Electricity"],
                    "issue_date": ["2026-09-03", "2026-10-01"],
                })),
            ),
        ];
        let run = drive(&d, "CS-1", DISPUTE, &[], &world).await;
        assert_never_asked(&run, &["account.list", "invoice.list"]);
        expect_choice(
            &run,
            "Is this about your Gas invoice issued 3 September or your Electricity invoice issued 1 October?",
        );
    }

    /// No account on file: the plain no-record sentence, never a request for an account number.
    /// Layer: orchestrator.
    #[tokio::test]
    async fn no_account_says_so_in_plain_words() {
        let d = desk("known-bill-none", &SPACES, &OPS).await;
        let world: World = vec![("account.list", Read::Rows(json!({})))];
        let run = drive(&d, "CS-1", DISPUTE, &[], &world).await;
        assert_never_asked(&run, &["account.list"]);
        expect_no_match(&run, "account");
        assert_eq!(run.reply(), "I can't find an account on your account.");
    }
}

// ---- IT desk -----------------------------------------------------------------------------------------------------
// No seeded IT-desk space declares a ticket slot (v1..v14 ask asset_serial / impact), so the `ticket_id` and `case_id`
// kinds are tested at the resolver: `resolve_records` over a context parsed from the connector's columnar answer, and
// the question `record_choice_question` builds.

mod it_desk {
    use super::*;

    fn today() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 27).unwrap()
    }

    /// A context whose `case.list` read answered, parsed by the same code the orchestrator uses.
    fn tickets() -> CustomerContext {
        let answer = json!({
            "case_id": ["TKT-3301", "TKT-3342"],
            "title": ["Broken laptop screen", "VPN keeps dropping"],
            "status": ["open", "open"],
            "updated_at": ["2026-10-20T09:00:00Z", "2026-10-25T09:00:00Z"],
        });
        let mut c = CustomerContext {
            party: PARTY.to_owned(),
            ..CustomerContext::default()
        };
        c.records.insert(
            "case.list".to_owned(),
            cc::parse_records(answer.as_object().unwrap()),
        );
        c
    }

    /// The ticket keys are never asked of the employee.
    #[test]
    fn ticket_keys_are_never_asked() {
        for slot in ["ticket_id", "incident_ref", "case_id"] {
            assert!(never_asked(slot), "{slot}");
            assert!(resolve::record_kind(slot).is_some(), "{slot}");
        }
    }

    /// "my laptop is still broken" resolves by title; nobody is asked. Layer: resolve_records.
    #[test]
    fn my_laptop_is_still_broken_resolves_by_title() {
        let ctx = tickets();
        for slot in ["ticket_id", "incident_ref", "case_id"] {
            let kind = resolve::record_kind(slot).unwrap();
            assert!(resolve::records_available(kind, &ctx), "{slot}");
            match resolve::resolve_records(kind, &ctx, Some("my laptop is still broken"), today()) {
                Resolved::One(p) => assert_eq!(p.order_id, "TKT-3301", "{slot}"),
                other => panic!("{slot}: {other:?}"),
            }
        }
    }

    /// Without a hint both tickets fit: the question names them by title and last update, never by id.
    /// Layer: resolve_records + record_choice_question.
    #[test]
    fn two_tickets_are_named_by_title_and_update_never_by_id() {
        let ctx = tickets();
        let kind = resolve::record_kind("ticket_id").unwrap();
        let Resolved::Ambiguous { picks, more } =
            resolve::resolve_records(kind, &ctx, None, today())
        else {
            panic!("two tickets fit")
        };
        assert!(!more);
        let q = record_choice_question(kind, &picks, more);
        assert_eq!(
            q,
            "Is this about your Broken laptop screen ticket updated 20 October or your VPN keeps dropping ticket updated 25 October?"
        );
        assert!(!contains_id_pattern(&q), "{q}");
        assert!(!q.contains("TKT"), "{q}");
        // the answer maps back to the ticket's value for the slot, server-side
        assert_eq!(
            picks[0]
                .slots(&["ticket_id"])
                .get("ticket_id")
                .map(String::as_str),
            Some("TKT-3301")
        );
    }

    /// A hint that fits no ticket is "none", a plain sentence, not a request for a number. A `case.list` that never
    /// answered is not "none": the records are unavailable (handover, not a question). Layer: resolve_records.
    #[test]
    fn no_ticket_is_none_and_an_unread_list_is_unavailable() {
        let ctx = tickets();
        let kind = resolve::record_kind("ticket_id").unwrap();
        assert!(matches!(
            resolve::resolve_records(kind, &ctx, Some("my kettle"), today()),
            Resolved::Nothing
        ));
        let unread = CustomerContext {
            party: PARTY.to_owned(),
            degraded: vec!["case.list".to_owned()],
            ..CustomerContext::default()
        };
        assert!(!resolve::records_available(kind, &unread));
    }
}
