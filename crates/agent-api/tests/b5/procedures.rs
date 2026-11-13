//! B5 T7 — J12 context spaces and procedures (plan "Route table"): `GET /api/jobs/{job}/context-spaces`, B2's
//! `PUT /api/jobs/{job}/context-spaces/{key}` as `saveContextSpaceRequired` (kind `space_required`, `null` = the
//! template's slots), and the procedure routes (`procedures`, `procedureVersions`, `createProcedureVersion`,
//! `approveProcedure`, `setProcedureStatus`) with integer versions (R6) and D88 (no reviewer count; publishing needs
//! `release_manager`). Real Postgres 18, a Demo Retail job created through `POST /api/jobs`, through the HTTP router
//! as `agent_app`. The draft keeps the template's default selection (`areas: []` = every area), so the logistics and
//! FAQ spaces the procedure tests use are in it and the ledger starts empty; [`design_contract::publish`] writes the
//! cuttable selection itself when a test releases.
use axum::http::StatusCode;
use serde_json::{Value, json};

use crate::common::design_contract::{self, ContractJob, DESIGNER};
use crate::common::{self, TestEnv, get, insert_user, person, send};

/// The space the context-space tests use.
const SPACE: &str = "retail:order-support:returns";
/// The space the procedure tests use: seed v8 (R15) names `delivery:where_is_my_order` on it, an activity the Demo
/// Retail fold has with operations, so its grant comes from template data alone.
const PROC_SPACE: &str = "retail:logistics:track_parcel";
/// `delivery:where_is_my_order`'s operations (seed v8, from the old track_parcel grant, plus seed v9's optional
/// `delivery.list`), as grant labels.
const GRANT: [&str; 5] = [
    "delivery.read{by_tracking_ref}",
    "order.read",
    "knowledge.search",
    "delivery.read",
    "delivery.list",
];
/// A base space of the Demo Retail pins the seed maps no activity to: its procedures have an empty grant.
const UNMAPPED_SPACE: &str = "retail:faq:shipping";
/// A tenant procedure name no template uses.
const PROC: &str = "start_return";

struct Fx {
    e: TestEnv,
    f: ContractJob,
    /// `job_designer` only.
    designer: String,
    /// A tenant admin (every role).
    admin: String,
}

async fn setup() -> Fx {
    let e = common::env().await;
    let f = crate::b5::support::new_job(&e).await;
    insert_user(&e.owner, DESIGNER, f.tenant, "job_designer").await;
    let admin = design_contract::release_manager(&e, &f).await;
    Fx {
        e,
        f,
        designer: person(DESIGNER, &[]),
        admin,
    }
}

fn uri(x: &Fx, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", x.f.job.job_key)
}

async fn read(x: &Fx, tail: &str, tok: &str) -> (StatusCode, Value) {
    send(&x.e.app, get(&uri(x, tail), Some(tok))).await
}

async fn write(x: &Fx, method: &str, tail: &str, tok: &str, body: Value) -> (StatusCode, Value) {
    send(
        &x.e.app,
        common::json(method, &uri(x, tail), Some(tok), body),
    )
    .await
}

#[track_caller]
fn ok(got: (StatusCode, Value)) -> Value {
    assert_eq!(got.0, StatusCode::OK, "{}", got.1);
    got.1
}

#[track_caller]
fn refused(got: (StatusCode, Value), status: StatusCode, code: &str) -> Value {
    assert_eq!(got.0, status, "{}", got.1);
    assert_eq!(got.1["error"]["code"], json!(code), "{}", got.1);
    got.1
}

/// `(key, kind, page, label)` of every ledger row of the job.
async fn ledger(x: &Fx) -> Vec<(String, String, String, String)> {
    sqlx::query_as("SELECT key, kind, page, label FROM draft_ledger WHERE job_id = $1 ORDER BY key")
        .bind(x.f.job.job_id)
        .fetch_all(&x.e.owner)
        .await
        .unwrap()
}

async fn spaces(x: &Fx) -> Vec<Value> {
    let b = ok(read(x, "context-spaces", &x.designer).await);
    assert_eq!(b["next"], Value::Null);
    b["items"].as_array().expect("items").clone()
}

async fn space(x: &Fx, key: &str) -> Value {
    spaces(x)
        .await
        .into_iter()
        .find(|s| s["key"] == json!(key))
        .unwrap_or_else(|| panic!("no context space {key}"))
}

async fn save_required(x: &Fx, key: &str, required: Value) -> (StatusCode, Value) {
    write(
        x,
        "PUT",
        &format!("context-spaces/{key}"),
        &x.designer,
        json!({ "required": required }),
    )
    .await
}

fn step(op: &str, variant: Option<&str>) -> Value {
    json!({
        "step_number": 7,
        "phase": 0,
        "name": format!("Run {op}"),
        "operation": op,
        "variant": variant,
        "modality": "observe",
        "side_effect": "R",
        "identity_level": "0",
        "confirm_back": false,
        "gate": false,
        "args": {"order_id": "$order_id"},
    })
}

fn input(name: &str, version: Value, steps: Vec<Value>) -> Value {
    json!({
        "name": name,
        "version": version,
        "context_space": PROC_SPACE,
        "description": "  Returns with a pickup option.  ",
        "steps": steps,
    })
}

async fn create_as(x: &Fx, tok: &str, name: &str, version: u32, steps: Vec<Value>) -> Value {
    ok(write(
        x,
        "POST",
        "procedures",
        tok,
        input(name, json!(version), steps),
    )
    .await)
}

async fn create(x: &Fx, version: u32) -> Value {
    create_as(
        x,
        &x.designer,
        PROC,
        version,
        vec![step("order.read", None)],
    )
    .await
}

async fn set_status(x: &Fx, tok: &str, name: &str, v: u32, to: &str) -> (StatusCode, Value) {
    write(
        x,
        "POST",
        &format!("procedures/{name}/versions/{v}/status"),
        tok,
        json!({ "to": to }),
    )
    .await
}

async fn approve(x: &Fx, tok: &str, name: &str, v: u32) -> (StatusCode, Value) {
    write(
        x,
        "POST",
        &format!("procedures/{name}/versions/{v}/approve"),
        tok,
        json!({}),
    )
    .await
}

/// `name@v` → PUBLISHED as the admin (DRAFT → CANARY → PUBLISHED).
async fn publish_version(x: &Fx, name: &str, v: u32) -> Value {
    ok(set_status(x, &x.admin, name, v, "CANARY").await);
    ok(set_status(x, &x.admin, name, v, "PUBLISHED").await)
}

async fn versions(x: &Fx, name: &str) -> Vec<Value> {
    ok(read(x, &format!("procedures/{name}"), &x.designer).await)
        .as_array()
        .expect("a list")
        .clone()
}

async fn version(x: &Fx, name: &str, v: u32) -> Value {
    versions(x, name)
        .await
        .into_iter()
        .find(|p| p["version"] == json!(v))
        .unwrap_or_else(|| panic!("no {name}@{v}"))
}

/// A base version of the draft (name, version).
async fn base_version(x: &Fx) -> (String, u32) {
    let b = ok(read(x, "procedures", &x.designer).await);
    let p = b["items"]
        .as_array()
        .expect("items")
        .iter()
        .find(|p| p["origin"] == json!("base"))
        .unwrap_or_else(|| panic!("the template has base procedures: {b}"))
        .clone();
    (
        p["name"].as_str().unwrap().to_string(),
        u32::try_from(p["version"].as_u64().expect("an integer version")).unwrap(),
    )
}

// ---- context spaces -------------------------------------------------------------------------------------------------

#[tokio::test]
async fn context_spaces_carry_live_required_from_live_design() {
    let x = setup().await;
    // production = the cuttable selection's design (the space is live with the template's slots)
    crate::b5::support::publish(&x.e, &x.f).await;
    let before = space(&x, SPACE).await;
    assert_eq!(before["origin"], json!("base"));
    assert_eq!(before["required"], before["base_required"]);
    assert_eq!(before["live_required"], before["required"]);
    assert_eq!(before["tenant_id"], json!(x.f.tenant.to_string()));
    assert_eq!(before["job"], json!(x.f.job.job_key));

    ok(save_required(&x, SPACE, json!(["order_id"])).await);
    let after = space(&x, SPACE).await;
    assert_eq!(after["required"], json!(["order_id"]));
    assert_eq!(after["overridden"], json!(true));
    assert_eq!(
        after["live_required"], before["required"],
        "production keeps the template's slots until a release"
    );
    assert_ne!(after["required"], after["live_required"]);

    crate::b5::support::publish(&x.e, &x.f).await;
    let live = space(&x, SPACE).await;
    assert_eq!(live["live_required"], json!(["order_id"]));
}

#[tokio::test]
async fn required_only_patch_is_space_required_kind() {
    let x = setup().await;
    let label = space(&x, SPACE).await["label"]
        .as_str()
        .unwrap()
        .to_string();
    let b = ok(save_required(&x, SPACE, json!([" order_id ", "", "reason"])).await);
    assert!(b["draft_rev"].as_i64().is_some(), "{b}");
    assert_eq!(
        ledger(&x).await,
        [(
            format!("space:{SPACE}"),
            "space_required".to_string(),
            "J12".to_string(),
            format!("{label}: required fields order_id, reason"),
        )]
    );
    assert_eq!(
        space(&x, SPACE).await["required"],
        json!(["order_id", "reason"]),
        "trimmed, blanks dropped"
    );
}

#[tokio::test]
async fn null_required_reverts_to_template() {
    let x = setup().await;
    let base = space(&x, SPACE).await["base_required"].clone();
    ok(save_required(&x, SPACE, json!(["order_id"])).await);
    assert_eq!(ledger(&x).await.len(), 1);
    ok(save_required(&x, SPACE, Value::Null).await);
    let s = space(&x, SPACE).await;
    assert_eq!(s["required"], base);
    assert_eq!(s["overridden"], json!(false));
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM job_overlay_row WHERE job_id = $1 AND object = 'context_space' AND key = $2",
    )
    .bind(x.f.job.job_id)
    .bind(SPACE)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(rows, 0, "the override was the base again: it goes");
    assert!(ledger(&x).await.is_empty(), "back to live: no ledger row");

    // on a row that also carries a label, null keeps the row and drops only `required`
    ok(write(
        &x,
        "PUT",
        &format!("context-spaces/{SPACE}"),
        &x.designer,
        json!({"label": "Mine"}),
    )
    .await);
    ok(save_required(&x, SPACE, json!(["order_id"])).await);
    ok(save_required(&x, SPACE, Value::Null).await);
    let s = space(&x, SPACE).await;
    assert_eq!(s["label"], json!("Mine"));
    assert_eq!(s["required"], base, "the template's slots again");
    let item: Value = sqlx::query_scalar(
        "SELECT item FROM job_overlay_row WHERE job_id = $1 AND object = 'context_space' AND key = $2",
    )
    .bind(x.f.job.job_id)
    .bind(SPACE)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert!(item.get("required").is_none(), "{item}");
}

#[tokio::test]
async fn label_patch_still_overlay_kind() {
    let x = setup().await;
    ok(write(
        &x,
        "PUT",
        &format!("context-spaces/{SPACE}"),
        &x.designer,
        json!({"label": "Mine"}),
    )
    .await);
    assert_eq!(
        ledger(&x).await,
        [(
            format!("overlay:context_space:{SPACE}"),
            "overlay".to_string(),
            "J12".to_string(),
            format!("Context space {SPACE} changed"),
        )]
    );
}

#[tokio::test]
async fn invalid_and_duplicate_slot_422() {
    let x = setup().await;
    let b = refused(
        save_required(&x, SPACE, json!(["order_id", "Bad-Slot"])).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_slot",
    );
    assert_eq!(
        b["error"]["message"],
        json!("A slot is snake_case, e.g. order_id.")
    );
    let b = refused(
        save_required(&x, SPACE, json!(["order_id", " order_id"])).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "duplicate_slot",
    );
    assert_eq!(b["error"]["message"], json!("Slots must be unique."));
    refused(
        save_required(&x, "retail:nope:nope", json!(["order_id"])).await,
        StatusCode::NOT_FOUND,
        "context_space_not_found",
    );
    assert!(ledger(&x).await.is_empty());
}

// ---- createProcedureVersion ---------------------------------------------------------------------------------------

#[tokio::test]
async fn create_version_fills_side_effect_from_catalogue() {
    let x = setup().await;
    let v = create_as(
        &x,
        &x.designer,
        PROC,
        1,
        vec![step("order.read", None), step("order.cancel", None)],
    )
    .await;
    assert_eq!(v["origin"], json!("tenant"));
    assert_eq!(v["status"], json!("DRAFT"));
    assert_eq!(v["version"], json!(1), "an integer (R6)");
    assert_eq!(v["description"], json!("Returns with a pickup option."));
    assert_eq!(v["reviewers"], json!([]));
    let steps = v["steps"].as_array().unwrap();
    assert_eq!(steps[0]["step_number"], json!(1));
    assert_eq!(steps[1]["step_number"], json!(2));
    assert_eq!(steps[0]["side_effect"], json!("R"));
    assert_eq!(steps[0]["confirm_back"], json!(false));
    // the body said R / observe / 0: the catalogue's order.cancel is X, a `do`, and needs a confirm-back
    assert_eq!(steps[1]["side_effect"], json!("X"));
    assert_eq!(steps[1]["modality"], json!("do"));
    assert_eq!(steps[1]["confirm_back"], json!(true));
    assert_ne!(steps[1]["identity_level"], json!("0"));
    assert_eq!(steps[1]["args"], json!({"order_id": "$order_id"}));
    // the history and the ledger
    let h = v["history"].as_array().unwrap();
    assert_eq!(h.len(), 1, "{h:?}");
    assert_eq!(
        (&h[0]["action"], &h[0]["to"], &h[0]["ok"], &h[0]["actor"]),
        (
            &json!("create"),
            &json!("DRAFT"),
            &json!(true),
            &json!(DESIGNER)
        )
    );
    assert_eq!(
        ledger(&x).await,
        [(
            format!("procedure:{PROC}"),
            "procedure".to_string(),
            "J12".to_string(),
            format!("{PROC}@1 drafted"),
        )]
    );
    // the space's activity comes from the seed (R15); order.cancel is not delivery:where_is_my_order's
    assert_eq!(v["activity_grant"], json!(GRANT), "{v}");
    assert_eq!(v["outside_grant"], json!(["order.cancel"]));
    assert_eq!(
        (v["runs_7d"].clone(), v["success_rate"].clone()),
        (json!(0), Value::Null)
    );
}

/// A runtime built-in needs no operation row and no connector route: the fixture catalogue has no `knowledge.answer`
/// row, yet a tenant can author the step (and a needed-tool entry); an op that is neither a built-in nor a row stays 422.
#[tokio::test]
async fn create_version_accepts_a_runtime_builtin_step() {
    let x = setup().await;
    let mut ask = step("knowledge.answer", None);
    ask["args"] = json!({"question": "$question"});
    let mut body = input(PROC, json!(1), vec![ask, step("knowledge.search", None)]);
    body["tool_requirements"] = json!(["knowledge.answer"]);
    let v = ok(write(&x, "POST", "procedures", &x.designer, body).await);
    assert_eq!(v["steps"][0]["operation"], json!("knowledge.answer"), "{v}");
    assert_eq!(v["steps"][0]["side_effect"], json!("R"), "{v}");
    assert_eq!(v["steps"][0]["identity_level"], json!("0"), "{v}");
    assert_eq!(v["tool_requirements"], json!(["knowledge.answer"]), "{v}");
    // neither a built-in nor in the catalogue
    refused(
        write(
            &x,
            "POST",
            "procedures",
            &x.designer,
            input("other", json!(1), vec![step("knowledge.nope", None)]),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown_operation",
    );
}

#[tokio::test]
async fn create_version_unknown_operation_422() {
    let x = setup().await;
    let b = refused(
        write(
            &x,
            "POST",
            "procedures",
            &x.designer,
            input(PROC, json!(1), vec![step("nope.nope", None)]),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown_operation",
    );
    assert_eq!(
        b["error"]["message"],
        json!("Every step names an operation from the catalogue.")
    );
    refused(
        write(
            &x,
            "POST",
            "procedures",
            &x.designer,
            input(PROC, json!(1), vec![]),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_steps",
    );
    let mut bad_space = input(PROC, json!(1), vec![step("order.read", None)]);
    bad_space["context_space"] = json!("retail:nope:nope");
    refused(
        write(&x, "POST", "procedures", &x.designer, bad_space).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "unknown_context_space",
    );
    refused(
        write(
            &x,
            "POST",
            "procedures",
            &x.designer,
            input("Start", json!(1), vec![step("order.read", None)]),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_name",
    );
    let mut extra = input(PROC, json!(1), vec![step("order.read", None)]);
    extra["surprise"] = json!(true);
    refused(
        write(&x, "POST", "procedures", &x.designer, extra).await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_body",
    );
    assert!(ledger(&x).await.is_empty());
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM procedure_version WHERE job_id = $1")
        .bind(x.f.job.job_id)
        .fetch_one(&x.e.owner)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn version_exists_409() {
    let x = setup().await;
    let (name, top) = base_version(&x).await;
    // a number at or below a base version of the name
    for v in [top, 1] {
        refused(
            write(
                &x,
                "POST",
                "procedures",
                &x.designer,
                input(&name, json!(v), vec![step("order.read", None)]),
            )
            .await,
            StatusCode::CONFLICT,
            "version_exists",
        );
    }
    let next = top + 1;
    create_as(&x, &x.designer, &name, next, vec![step("order.read", None)]).await;
    let b = refused(
        write(
            &x,
            "POST",
            "procedures",
            &x.designer,
            input(&name, json!(next), vec![step("order.read", None)]),
        )
        .await,
        StatusCode::CONFLICT,
        "version_exists",
    );
    assert_eq!(
        b["error"]["message"],
        json!(format!("{name}@{next} already exists."))
    );
    // the base version is listed read-only next to the tenant's
    let vs = versions(&x, &name).await;
    assert_eq!(vs[0]["version"], json!(next), "newest first");
    assert!(
        vs.iter()
            .any(|p| p["origin"] == json!("base") && p["version"] == json!(top))
    );
}

#[tokio::test]
async fn version_must_be_integer_422_invalid_version() {
    let x = setup().await;
    for v in [json!("1.4.0"), json!("2"), json!(1.5), json!(0), json!(-3)] {
        let b = refused(
            write(
                &x,
                "POST",
                "procedures",
                &x.designer,
                input(PROC, v.clone(), vec![step("order.read", None)]),
            )
            .await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_version",
        );
        assert!(b["error"]["message"].is_string(), "{v}: {b}");
    }
    assert!(ledger(&x).await.is_empty());
}

// ---- approve / status ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn approve_needs_canary() {
    let x = setup().await;
    create(&x, 1).await;
    refused(
        approve(&x, &x.admin, PROC, 1).await,
        StatusCode::CONFLICT,
        "not_canary",
    );
    refused(
        approve(&x, &x.admin, PROC, 9).await,
        StatusCode::NOT_FOUND,
        "procedure_not_found",
    );
    // the designer does not hold release_manager
    let b = refused(
        approve(&x, &x.designer, PROC, 1).await,
        StatusCode::FORBIDDEN,
        "forbidden",
    );
    assert_eq!(b["error"]["missing_role"], json!("release_manager"));
    let (name, v) = base_version(&x).await;
    refused(
        approve(&x, &x.admin, &name, v).await,
        StatusCode::CONFLICT,
        "base_version",
    );
}

#[tokio::test]
async fn approve_twice_same_reviewer_409_already_reviewed() {
    let x = setup().await;
    create(&x, 1).await;
    ok(set_status(&x, &x.designer, PROC, 1, "CANARY").await);
    let b = ok(approve(&x, &x.admin, PROC, 1).await);
    let admin_sub = sqlx::query_scalar::<_, String>(
        "SELECT sub FROM users WHERE tenant_id = $1 AND sub LIKE 'u-contract-rm-%'",
    )
    .bind(x.f.tenant)
    .fetch_one(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(b["reviewers"], json!([admin_sub]));
    assert!(
        b["history"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["action"] == json!("approve") && h["actor"] == json!(admin_sub))
    );
    let b = refused(
        approve(&x, &x.admin, PROC, 1).await,
        StatusCode::CONFLICT,
        "already_reviewed",
    );
    assert_eq!(
        b["error"]["message"],
        json!("You already approved this version.")
    );
    let labels: Vec<String> = ledger(&x).await.into_iter().map(|r| r.3).collect();
    assert_eq!(labels, [format!("{PROC}@1 approved by {admin_sub}")]);
}

#[tokio::test]
async fn single_member_tenant_publishes_alone() {
    let x = setup().await;
    // the tenant admin holds every role: it drafts, moves and publishes without any approval (D88)
    create_as(&x, &x.admin, PROC, 1, vec![step("order.read", None)]).await;
    let v = publish_version(&x, PROC, 1).await;
    assert_eq!(v["status"], json!("PUBLISHED"));
    assert_eq!(v["reviewers"], json!([]));
    assert_eq!(v["effective_draft"], json!(true));
}

#[tokio::test]
async fn publish_requires_release_manager() {
    let x = setup().await;
    create(&x, 1).await;
    ok(set_status(&x, &x.designer, PROC, 1, "CANARY").await);
    let b = refused(
        set_status(&x, &x.designer, PROC, 1, "PUBLISHED").await,
        StatusCode::FORBIDDEN,
        "forbidden",
    );
    assert_eq!(b["error"]["missing_role"], json!("release_manager"));
    assert_eq!(version(&x, PROC, 1).await["status"], json!("CANARY"));
    // a Release manager who is not a Job designer publishes (fixture `setProcedureStatus`, web roles.test.ts) …
    insert_user(&x.e.owner, "u-rm-only", x.f.tenant, "release_manager").await;
    let rm = person("u-rm-only", &[]);
    let v = ok(set_status(&x, &rm, PROC, 1, "PUBLISHED").await);
    assert_eq!(v["status"], json!("PUBLISHED"));
    // … but every other move is the designer's
    let b = refused(
        set_status(&x, &rm, PROC, 1, "ARCHIVED").await,
        StatusCode::FORBIDDEN,
        "forbidden",
    );
    assert_eq!(b["error"]["missing_role"], json!("job_designer"));
    ok(set_status(&x, &x.designer, PROC, 1, "ARCHIVED").await);
    // an observer is refused before its body is read
    insert_user(&x.e.owner, "u-observer", x.f.tenant, "tenant_user").await;
    let b = refused(
        write(
            &x,
            "POST",
            &format!("procedures/{PROC}/versions/1/status"),
            &person("u-observer", &[]),
            json!({}),
        )
        .await,
        StatusCode::FORBIDDEN,
        "forbidden",
    );
    assert_eq!(b["error"]["missing_role"], json!("job_designer"));
}

#[tokio::test]
async fn two_reviewer_count_code_is_gone() {
    let code = ["needs", "two", "reviewers"].join("_");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut hits = Vec::new();
    let mut stack = vec![root.join("crates"), root.join("web/src")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if p.is_dir() {
                if name != "target" && name != "node_modules" && !name.starts_with('.') {
                    stack.push(p);
                }
            } else if let Ok(text) = std::fs::read_to_string(&p)
                && text.contains(&code)
            {
                hits.push(p.display().to_string());
            }
        }
    }
    assert!(hits.is_empty(), "{code} is still named in {hits:?}");
}

#[tokio::test]
async fn space_without_activity_mapping_has_empty_grant() {
    let x = setup().await;
    let s = space(&x, UNMAPPED_SPACE).await;
    assert_eq!(s["activity"], json!(""), "{s}");
    assert_eq!(
        space(&x, PROC_SPACE).await["activity"],
        json!("delivery:where_is_my_order")
    );
    let mut body = input(PROC, json!(1), vec![step("order.read", None)]);
    body["context_space"] = json!(UNMAPPED_SPACE);
    ok(write(&x, "POST", "procedures", &x.designer, body).await);
    let v = version(&x, PROC, 1).await;
    assert_eq!(v["activity_grant"], json!([]), "{v}");
    assert_eq!(v["outside_grant"], json!(["order.read"]));
    let b = refused(
        set_status(&x, &x.designer, PROC, 1, "CANARY").await,
        StatusCode::CONFLICT,
        "operation_outside_grant",
    );
    assert_eq!(
        b["error"]["message"],
        json!(format!(
            "order.read is outside the activity grant of {UNMAPPED_SPACE}."
        ))
    );
}

#[tokio::test]
async fn canary_with_step_outside_grant_409_operation_outside_grant() {
    let x = setup().await;
    create_as(
        &x,
        &x.designer,
        PROC,
        1,
        vec![step("order.read", None), step("payment.refund", None)],
    )
    .await;
    let v = version(&x, PROC, 1).await;
    assert_eq!(v["outside_grant"], json!(["payment.refund"]));
    let b = refused(
        set_status(&x, &x.designer, PROC, 1, "CANARY").await,
        StatusCode::CONFLICT,
        "operation_outside_grant",
    );
    assert_eq!(
        b["error"]["message"],
        json!(format!(
            "payment.refund is outside the activity grant of {PROC_SPACE}."
        ))
    );
    // the refusal's history row persists (its own statement), the version did not move, no ledger row for it
    let rows: Vec<(bool, Option<String>, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT ok, reason, from_status, to_status FROM procedure_history \
          WHERE job_id = $1 AND name = $2 AND action = 'transition'",
    )
    .bind(x.f.job.job_id)
    .bind(PROC)
    .fetch_all(&x.e.owner)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [(
            false,
            Some("operation_outside_grant".to_string()),
            Some("DRAFT".to_string()),
            Some("CANARY".to_string())
        )]
    );
    let v = version(&x, PROC, 1).await;
    assert_eq!(v["status"], json!("DRAFT"));
    assert!(
        v["history"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["ok"] == json!(false) && h["reason"] == json!("operation_outside_grant"))
    );
    let labels: Vec<String> = ledger(&x).await.into_iter().map(|r| r.3).collect();
    assert_eq!(labels, [format!("{PROC}@1 drafted")]);
    // a move the UI does not allow is refused too
    refused(
        set_status(&x, &x.designer, PROC, 1, "ARCHIVED").await,
        StatusCode::CONFLICT,
        "invalid_transition",
    );
}

#[tokio::test]
async fn publish_archives_previous_tenant_published() {
    let x = setup().await;
    create(&x, 1).await;
    publish_version(&x, PROC, 1).await;
    create(&x, 2).await;
    publish_version(&x, PROC, 2).await;
    let v1 = version(&x, PROC, 1).await;
    assert_eq!(v1["status"], json!("ARCHIVED"));
    let last = v1["history"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(
        (&last["from"], &last["to"], &last["ok"], &last["reason"]),
        (
            &json!("PUBLISHED"),
            &json!("ARCHIVED"),
            &json!(true),
            &json!("replaced by 2")
        )
    );
    assert_eq!(version(&x, PROC, 2).await["status"], json!("PUBLISHED"));
    let labels: Vec<String> = ledger(&x).await.into_iter().map(|r| r.3).collect();
    assert_eq!(
        labels,
        [format!("{PROC}@2 canary → published")],
        "one ledger row per name, the last write's label"
    );
}

#[tokio::test]
async fn base_version_transition_409() {
    let x = setup().await;
    let (name, v) = base_version(&x).await;
    refused(
        set_status(&x, &x.designer, &name, v, "ARCHIVED").await,
        StatusCode::CONFLICT,
        "base_version",
    );
    refused(
        set_status(&x, &x.designer, &name, 999, "ARCHIVED").await,
        StatusCode::NOT_FOUND,
        "procedure_not_found",
    );
    refused(
        write(
            &x,
            "POST",
            &format!("procedures/{name}/versions/{v}/status"),
            &x.designer,
            json!({"to": "LIVE"}),
        )
        .await,
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_body",
    );
    assert!(ledger(&x).await.is_empty());
}

#[tokio::test]
async fn effective_follows_live_effective_draft_follows_draft() {
    let x = setup().await;
    create(&x, 1).await;
    let v = publish_version(&x, PROC, 1).await;
    assert_eq!(
        (&v["effective_draft"], &v["effective"], &v["live_status"]),
        (&json!(true), &json!(false), &Value::Null),
        "published in the draft only"
    );
    let release = crate::b5::support::publish(&x.e, &x.f).await;
    let v = version(&x, PROC, 1).await;
    assert_eq!(
        (&v["effective_draft"], &v["effective"], &v["live_status"]),
        (&json!(true), &json!(true), &json!("PUBLISHED"))
    );
    assert_eq!(v["released_in"], json!(release));
    // archive it in the draft: production still runs it
    ok(set_status(&x, &x.designer, PROC, 1, "ARCHIVED").await);
    let v = version(&x, PROC, 1).await;
    assert_eq!(
        (&v["effective_draft"], &v["effective"], &v["live_status"]),
        (&json!(false), &json!(true), &json!("PUBLISHED"))
    );
    // the list carries the same flags
    let list = ok(read(&x, "procedures", &x.designer).await);
    let row = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == json!(PROC))
        .unwrap()
        .clone();
    assert_eq!(row["effective"], json!(true));
}

#[tokio::test]
async fn runs_and_success_rate_from_cases() {
    let x = setup().await;
    create(&x, 1).await;
    for (n, (env, status, draft_rev)) in [
        ("prod", "resolved", None),
        ("prod", "not_eligible", None),
        ("prod", "handover", None),
        ("test", "resolved", Some(1_i64)),
        ("test", "open", Some(1_i64)),
    ]
    .into_iter()
    .enumerate()
    {
        sqlx::query(
            "INSERT INTO agent_case (tenant_id, job_id, case_id, conversation_id, channel, env, draft_rev, status, \
                                     title, initiator_sub, procedure_name, procedure_version) \
             VALUES ($1, $2, $3, $4, 'staff', $5, $6, $7, 'I want to return my order', 'u-tester', $8, 1)",
        )
        .bind(x.f.tenant)
        .bind(x.f.job.job_id)
        .bind(format!("CS-{}", n + 1))
        .bind(format!("conv-{n}"))
        .bind(env)
        .bind(draft_rev)
        .bind(status)
        .bind(PROC)
        .execute(&x.e.owner)
        .await
        .unwrap();
    }
    let v = version(&x, PROC, 1).await;
    assert_eq!(v["runs_7d"], json!(3));
    let rate = v["success_rate"].as_f64().expect("a rate with runs");
    assert!((rate - 2.0 / 3.0).abs() < 1e-9, "{rate}");
    let runs = v["test_runs"].as_array().unwrap();
    assert_eq!(
        runs.len(),
        1,
        "an open test case is not a run yet: {runs:?}"
    );
    assert_eq!(
        (
            &runs[0]["outcome"],
            &runs[0]["actor"],
            &runs[0]["utterance"]
        ),
        (
            &json!("resolved"),
            &json!("u-tester"),
            &json!("I want to return my order")
        )
    );
    // a version without cases
    create(&x, 2).await;
    let v2 = version(&x, PROC, 2).await;
    assert_eq!(
        (v2["runs_7d"].clone(), v2["success_rate"].clone()),
        (json!(0), Value::Null)
    );
    assert_eq!(v2["test_runs"], json!([]));
}
