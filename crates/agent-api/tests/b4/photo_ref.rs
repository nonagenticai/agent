//! SW-9 `photo_ref` (owner rule: a known customer is never asked for an id): when a known customer reports a delivery
//! problem and has sent an image in THIS conversation, the runtime passes `photo_ref` = that attachment's id to the
//! step that binds `$photo_ref` (the slots step; `delivery.report_problem` declares the input). It is the newest
//! IMAGE the CUSTOMER sent here: never staff's, never a PDF, never another conversation's. With none the slot stays
//! absent and the case runs unasked. A `photo_ref` a call carries must be such an image, else `not_found`.
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_api::attachments::store::{Direction, is_customer_image, newest_customer_image};
use agent_api::attachments::validate::Accepted;
use agent_api::runtime::Env;
use agent_api::runtime::design::{DesignCache, RuntimeDesign};
use agent_api::runtime::domain::QualifiedRequest;
use agent_api::runtime::domain::{OpCall, OpState as ResultState, Phase, ProcedureRef, Step};
use agent_api::runtime::orchestrator::ProcedureSource;
use agent_api::runtime::routes::ResolvedTarget;
use agent_api::runtime::runner::Runner;
use agent_api::runtime::runner::ops::{
    Executors, OpExecutor, OpOutcome, OpRequest, OpState, outcome,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::b4::design::put_target;
use crate::b4::flow::{
    self as fl, DELIVERY_AREA, EveryOpOffered, Fx, current_design, delivery_request,
    insert_conversation,
};
use crate::redis_env as r;

fn file(media_type: &'static str, name: &str) -> Accepted {
    let pdf = media_type == "application/pdf";
    Accepted {
        media_type,
        bytes: vec![7, 7, 7],
        width: (!pdf).then_some(2),
        height: (!pdf).then_some(2),
        display_name: name.to_owned(),
    }
}

/// Record `a` in `conv` as `direction` (its own transaction: `created_at` rises with each call). Only the metadata row
/// matters to `photo_ref`, so the row is written as the store writes it, with an artifact id no service holds.
async fn put(fx: &Fx, conv: &str, direction: Direction, a: Accepted) -> Uuid {
    let mut tx = fx.d.f.job.begin(&fx.d.e.state.db).await.unwrap();
    let by = match direction {
        Direction::Customer => "customer",
        Direction::Staff => "staff-1",
    };
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO conversation_attachment \
            (id, conversation_id, direction, media_type, byte_size, sha256, display_name, artifact_id, width, \
             height, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
    )
    .bind(id)
    .bind(conv)
    .bind(direction.as_str())
    .bind(a.media_type)
    .bind(a.bytes.len() as i32)
    .bind(vec![0u8; 32])
    .bind(&a.display_name)
    .bind(Uuid::new_v4())
    .bind(a.width.and_then(|w| i32::try_from(w).ok()))
    .bind(a.height.and_then(|h| i32::try_from(h).ok()))
    .bind(by)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();
    id
}

async fn png(fx: &Fx, conv: &str, direction: Direction) -> Uuid {
    put(fx, conv, direction, file("image/png", "parcel.png")).await
}

// ---- the slots step ------------------------------------------------------------------------------------------------

/// A one-step plan: `delivery.report_problem{by_order}` binding `$photo_ref` (the seed's delivery_problem@2 step
/// binds order_id and problem; a seed revision adds `"photo_ref": "$photo_ref"`).
struct ReportWithPhoto;

impl ProcedureSource for ReportWithPhoto {
    fn plan(
        &self,
        _design: &RuntimeDesign,
        _req: &QualifiedRequest,
    ) -> Option<(Option<ProcedureRef>, Vec<Phase>)> {
        Some((
            Some(ProcedureRef {
                name: "delivery_problem".into(),
                version: 2,
            }),
            vec![Phase {
                steps: vec![Step::op(
                    "report",
                    "delivery.report_problem",
                    Some("by_order"),
                    json!({"order_id": "$order_id", "problem": "$problem", "photo_ref": "$photo_ref"}),
                )],
            }],
        ))
    }
}

/// Run the delivery problem of `case` (conversation `conv-<case>`, its attachments already stored) to its first
/// call: the call, and whether the customer was asked anything.
async fn run_case(fx: &Fx, case: &str) -> (OpCall, bool) {
    fl::insert_case(fx, case, &current_design(fx).await).await;
    let o = fl::orch(fx, ReportWithPhoto).with_routes(Arc::new(EveryOpOffered));
    o.on_qualified(fl::qualified(&delivery_request(case)))
        .await
        .unwrap();
    let calls = fl::calls(fx, case).await;
    assert_eq!(calls.len(), 1, "{calls:?}");
    let asked = !fl::resolutions_in(fx, DELIVERY_AREA, case).await.is_empty();
    (calls.into_iter().next().unwrap(), asked)
}

fn photo(c: &OpCall) -> Option<&str> {
    c.input.get("photo_ref").and_then(Value::as_str)
}

#[tokio::test]
async fn the_newest_customer_image_of_the_conversation_fills_photo_ref() {
    let fx = fl::setup("photo-fills", Env::Test).await;
    insert_conversation(&fx, "conv-CS-1").await;
    let _older = png(&fx, "conv-CS-1", Direction::Customer).await;
    let newest = png(&fx, "conv-CS-1", Direction::Customer).await;
    let (c, asked) = run_case(&fx, "CS-1").await;
    assert_eq!(photo(&c), Some(newest.to_string().as_str()), "{c:?}");
    assert!(!asked, "the customer is never asked for it");
}

#[tokio::test]
async fn a_staff_upload_and_another_conversations_image_do_not() {
    let fx = fl::setup("photo-foreign", Env::Test).await;
    insert_conversation(&fx, "conv-CS-1").await;
    insert_conversation(&fx, "conv-other").await;
    let mine = png(&fx, "conv-CS-1", Direction::Customer).await;
    // newer, but not the customer's own in this conversation
    let _staff = png(&fx, "conv-CS-1", Direction::Staff).await;
    let _elsewhere = png(&fx, "conv-other", Direction::Customer).await;
    let (c, _) = run_case(&fx, "CS-1").await;
    assert_eq!(photo(&c), Some(mine.to_string().as_str()), "{c:?}");

    // with only those two, nothing fills
    let fx = fl::setup("photo-foreign-only", Env::Test).await;
    insert_conversation(&fx, "conv-CS-2").await;
    insert_conversation(&fx, "conv-other").await;
    let _ = png(&fx, "conv-CS-2", Direction::Staff).await;
    let _ = png(&fx, "conv-other", Direction::Customer).await;
    let (c, asked) = run_case(&fx, "CS-2").await;
    assert_eq!(photo(&c), None, "{c:?}");
    assert!(!asked);
}

#[tokio::test]
async fn a_pdf_is_not_a_photo() {
    let fx = fl::setup("photo-pdf", Env::Test).await;
    insert_conversation(&fx, "conv-CS-1").await;
    let img = png(&fx, "conv-CS-1", Direction::Customer).await;
    let _pdf = put(
        &fx,
        "conv-CS-1",
        Direction::Customer,
        file("application/pdf", "invoice.pdf"),
    )
    .await;
    let (c, _) = run_case(&fx, "CS-1").await;
    assert_eq!(
        photo(&c),
        Some(img.to_string().as_str()),
        "the newer PDF is skipped"
    );

    let fx = fl::setup("photo-pdf-only", Env::Test).await;
    insert_conversation(&fx, "conv-CS-2").await;
    let _ = put(
        &fx,
        "conv-CS-2",
        Direction::Customer,
        file("application/pdf", "invoice.pdf"),
    )
    .await;
    let (c, _) = run_case(&fx, "CS-2").await;
    assert_eq!(photo(&c), None, "{c:?}");
}

#[tokio::test]
async fn no_image_leaves_photo_ref_absent_and_the_customer_unasked() {
    let fx = fl::setup("photo-none", Env::Test).await;
    insert_conversation(&fx, "conv-CS-1").await;
    let (c, asked) = run_case(&fx, "CS-1").await;
    assert_eq!(photo(&c), None, "{c:?}");
    assert!(!asked, "the procedure runs; nobody asks for the photo");
    assert_eq!(
        c.input.get("order_id").and_then(Value::as_str),
        Some("ORD-58790")
    );
}

#[tokio::test]
async fn the_reads_agree_with_the_resolution() {
    let fx = fl::setup("photo-reads", Env::Test).await;
    insert_conversation(&fx, "conv-A").await;
    insert_conversation(&fx, "conv-B").await;
    let a = png(&fx, "conv-A", Direction::Customer).await;
    let s = png(&fx, "conv-A", Direction::Staff).await;
    let mut tx = fx.d.f.job.begin(&fx.d.e.state.db).await.unwrap();
    assert_eq!(
        newest_customer_image(&mut tx, "conv-A").await.unwrap(),
        Some(a)
    );
    assert_eq!(
        newest_customer_image(&mut tx, "conv-B").await.unwrap(),
        None
    );
    assert!(is_customer_image(&mut tx, "conv-A", a).await.unwrap());
    assert!(
        !is_customer_image(&mut tx, "conv-A", s).await.unwrap(),
        "staff's"
    );
    assert!(
        !is_customer_image(&mut tx, "conv-B", a).await.unwrap(),
        "another conversation's"
    );
    assert!(
        !is_customer_image(&mut tx, "conv-A", Uuid::new_v4())
            .await
            .unwrap()
    );
}

// ---- the ownership rule ---------------------------------------------------------------------------------------------

struct Order;

impl OpExecutor for Order {
    fn execute<'a>(
        &'a self,
        _r: &'a OpRequest,
        _t: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async {
            outcome(
                OpState::Ok,
                None,
                None,
                json!({"status": "shipped", "customer_id": "C-1"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
        })
    }
}

struct Report(AtomicUsize);

impl OpExecutor for Report {
    fn execute<'a>(
        &'a self,
        _r: &'a OpRequest,
        _t: &'a ResolvedTarget,
    ) -> BoxFuture<'a, OpOutcome> {
        Box::pin(async {
            self.0.fetch_add(1, Ordering::SeqCst);
            outcome(
                OpState::Ok,
                None,
                None,
                json!({"case_id": "DP-1", "status": "open"})
                    .as_object()
                    .cloned()
                    .unwrap(),
            )
        })
    }
}

/// Ann's (C-1) web-chat case `CS-1` in `conv-1`, with one inbound message; its id.
async fn ann_case(fx: &Fx) -> String {
    let (t, j) = (fx.d.f.tenant, fx.d.f.job.job_id);
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel, customer_id) \
         VALUES ($1, $2, 'conv-1', $3, 'web_chat', 'tg:42')",
    )
    .bind(t)
    .bind(j)
    .bind(fx.scope.env.as_str())
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    let draft_rev = match current_design(fx).await {
        agent_api::design::CaseDesign::Draft { draft_rev } => Some(draft_rev),
        agent_api::design::CaseDesign::Live { .. } => None,
    };
    sqlx::query(
        "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, release_id, draft_rev, \
                                 qualified_release_id, qualified_draft_rev, status) \
         VALUES ($1, $2, 'CS-1', 'conv-1', 'web_chat', $3, NULL, $4, NULL, $4, 'open')",
    )
    .bind(t)
    .bind(j)
    .bind(fx.scope.env.as_str())
    .bind(draft_rev)
    .execute(&fx.d.e.owner)
    .await
    .unwrap();
    crate::common::state::identify(&fx.d.e.owner, t, j, "conv-1", "C-1", None).await;
    sqlx::query_scalar(
        "INSERT INTO conversation_message (tenant_id, job_id, conversation_id, role, text, case_id, at_ms) \
         VALUES ($1, $2, 'conv-1', 'customer', 'my parcel arrived broken', 'CS-1', 1) RETURNING message_id::text",
    )
    .bind(t)
    .bind(j)
    .fetch_one(&fx.d.e.owner)
    .await
    .unwrap()
}

fn report_call(fx: &Fx, msg: &str, input: Value) -> OpCall {
    serde_json::from_value(json!({
        "call_id": agent_api::runtime::domain::call_id("CS-1", 0, "report", 1),
        "case_id": "CS-1", "conversation_id": "conv-1", "phase": 0, "step": "report",
        "attempt": 1, "agent_role": "procedure", "operation": "delivery.report_problem", "variant": null,
        "input": input, "idempotency_key": "CS-1:0:report", "env": fx.scope.env,
        "release_id": null, "draft_rev": null, "confirm_ref": msg, "initiator": "procedure@test",
        "identity_level": 2, "deadline_ms": 0
    }))
    .unwrap()
}

/// A `photo_ref` the call carries must be a customer image of THIS conversation: the customer's own reaches the
/// executor; staff's, another conversation's, a PDF, a made-up id and a list answer `not_found` / are refused with
/// no executor call. Mutation: dropping `Owned::OwnImageIfPresent` from the table lets all of them through.
#[tokio::test]
async fn a_foreign_photo_ref_is_refused() {
    let fx = fl::setup("photo-owner", Env::Test).await;
    put_target(&fx.d).await;
    let msg = ann_case(&fx).await;
    insert_conversation(&fx, "conv-other").await;
    let own = png(&fx, "conv-1", Direction::Customer).await;
    let staff = png(&fx, "conv-1", Direction::Staff).await;
    let elsewhere = png(&fx, "conv-other", Direction::Customer).await;
    let pdf = put(
        &fx,
        "conv-1",
        Direction::Customer,
        file("application/pdf", "i.pdf"),
    )
    .await;
    let report = Arc::new(Report(AtomicUsize::new(0)));
    let mut st = fx.d.e.state.clone();
    st.db = fx.rt.clone();
    let runner = Runner::new(fx.bus.clone(), st, Arc::new(DesignCache::default()))
        .unwrap()
        .with_consumer("runner-photo")
        .with_consume(r::fast())
        .with_executors(
            Executors::default()
                .register("order.read", Arc::new(Order))
                .register("delivery.report_problem", report.clone()),
        );
    let base = |photo: Value, key: &str| {
        let mut c = report_call(
            &fx,
            &msg,
            json!({"order_id": "RET-01", "problem": "broken", "photo_ref": photo}),
        );
        c.call_id = agent_api::runtime::domain::call_id("CS-1", 0, key, 1);
        c.step = key.to_owned();
        c.idempotency_key = Some(format!("CS-1:0:{key}"));
        c
    };
    for (key, bad) in [
        ("staff", json!(staff.to_string())),
        ("elsewhere", json!(elsewhere.to_string())),
        ("pdf", json!(pdf.to_string())),
        ("made-up", json!(Uuid::new_v4().to_string())),
        ("junk", json!("not-a-uuid")),
    ] {
        let res = runner.handle(&base(bad, key)).await.unwrap();
        assert_eq!(res.state, ResultState::Error, "{key}: {res:?}");
        assert_eq!(
            res.outcome_code.as_deref(),
            Some("not_found"),
            "{key}: {res:?}"
        );
    }
    let res = runner
        .handle(&base(json!([own.to_string()]), "list"))
        .await
        .unwrap();
    assert_ne!(res.state, ResultState::Ok, "a list is refused: {res:?}");
    assert_eq!(
        report.0.load(Ordering::SeqCst),
        0,
        "no refused call reached the executor"
    );

    let res = runner
        .handle(&base(json!(own.to_string()), "own"))
        .await
        .unwrap();
    assert_eq!(res.state, ResultState::Ok, "{res:?}");
    assert_eq!(report.0.load(Ordering::SeqCst), 1);
}
