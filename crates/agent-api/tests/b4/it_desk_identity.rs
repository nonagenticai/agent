//! The IT desk's "customer" is the signed-in employee: their verified Keycloak email is the case's verified party
//! (`customer_context::staff_party`), so the desk never asks them for their login (`upn`). "I'm locked out" folds to
//! `Outcome::Ready` (never `Outcome::Ask`), and the `slots` step fills `upn` from the verified party alone: not from
//! anything typed. Layer: qualifier fold + orchestrator over Postgres 18 + Redis.
use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, OnceLock};

use agent_api::design::DesignOf;
use agent_api::runtime::design::RuntimeDesign;
use agent_api::runtime::domain::{OpCall, OpState, QualifiedRequest};
use agent_api::runtime::orchestrator::{DesignProcedures, Orchestrator};
use agent_api::runtime::{Env, Scope};
use agent_api::turn::qualify::{self, Draft, Extraction, Outcome};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::Mutex as AsyncMutex;

use crate::b4::flow::{self as fl, EveryOpOffered, Fx};
use crate::common;
use crate::common::design_contract::{self, ContractJob};
use crate::common::platform;
use crate::common::{TestEnv, json as req, send, super_admin};
use crate::redis_env as r;

const EMPLOYEE: &str = "anna.kowalski@northwind-it.example";
const SPACE: &str = "retail:order-support:unlock_account";

/// One context space a test authors for its own job: its D61 key, its slots, and the knowledge kinds it answers from
/// (a space with a knowledge kind is served by the knowledge step, so a case in it has a plan without any
/// procedure, activity or operation).
pub struct OwnSpace {
    pub key: &'static str,
    pub slots: &'static [&'static str],
    pub kinds: &'static [&'static str],
}

fn authored() -> &'static AsyncMutex<std::collections::HashSet<String>> {
    static DONE: OnceLock<AsyncMutex<std::collections::HashSet<String>>> = OnceLock::new();
    DONE.get_or_init(Default::default)
}

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, req(method, uri, Some(&super_admin()), body)).await
}

async fn layer_id(e: &TestEnv, key: &str) -> Option<String> {
    sqlx::query_scalar("SELECT id::text FROM layer WHERE key = $1")
        .bind(key)
        .fetch_optional(&e.owner)
        .await
        .unwrap()
}

/// A job layer `job/<name>` of the platform offering's industry and department holding the job row, `spaces` (and
/// the knowledge kinds they name), tagged: the platform's own routes, as a super_admin authors it. Idempotent per
/// database and name (a name always means the same spaces).
pub async fn own_template(e: &TestEnv, name: &str, spaces: &[OwnSpace]) -> (String, i32) {
    platform::offering(e).await;
    let layer_key = format!("job/{name}");
    // the process-wide set is only a lock: tests run on separate databases, so "authored" is read from this database
    let mut done = authored().lock().await;
    let tagged: Option<i32> = sqlx::query_scalar(
        "SELECT max(version) FROM job_template_tag WHERE layer = $1 AND status = 'tagged'",
    )
    .bind(&layer_key)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    if tagged.is_none() {
        let id = match layer_id(e, &layer_key).await {
            Some(id) => id,
            None => {
                let (s, b) = sa(
                    e,
                    "POST",
                    "/api/layers",
                    json!({"scope": "job", "job": name, "label": name,
                    "template_selection": {
                        "profile": platform::PROFILE,
                        "industry": platform::INDUSTRY,
                        "departments": [platform::DEPARTMENT_LAYER],
                    }}),
                )
                .await;
                assert_eq!(s, StatusCode::CREATED, "layer {layer_key}: {b}");
                b["id"].as_str().expect("layer id").to_owned()
            }
        };
        let mut rows: Vec<(&str, String, Value)> = vec![(
            "job",
            name.to_owned(),
            json!({"label": name, "actor": "customer", "stance": "concise",
                   "preamble": "You are a helpful assistant."}),
        )];
        let mut kinds: Vec<&str> = Vec::new();
        for sp in spaces {
            let area = sp.key.split(':').nth(1).expect("a D61 space key");
            rows.push((
                "context_space",
                sp.key.to_owned(),
                json!({"label": sp.key, "area": area, "slots": sp.slots, "knowledge_kinds": sp.kinds}),
            ));
            for k in sp.kinds {
                if !kinds.contains(k) {
                    kinds.push(k);
                }
            }
        }
        for k in kinds {
            rows.push((
                "stage_knowledge_kind",
                format!("*|{k}"),
                json!({"label": k}),
            ));
        }
        for (kind, key, fields) in rows {
            let (s, b) = sa(
                e,
                "POST",
                &format!("/api/layers/{id}/rows"),
                json!({"kind": kind, "key": key, "op": "add", "fields": fields}),
            )
            .await;
            let dup = s == StatusCode::CONFLICT && b["error"]["code"] == "duplicate_add";
            assert!(s == StatusCode::CREATED || dup, "row {kind} {key}: {s} {b}");
        }
        let (s, b) = sa(
            e,
            "POST",
            &format!(
                "/api/platform/templates/{}/tags",
                layer_key.replace('/', ".")
            ),
            json!({}),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "tag {layer_key}: {b}");
        done.insert(layer_key.clone());
    }
    let version: i32 = sqlx::query_scalar(
        "SELECT max(version) FROM job_template_tag WHERE layer = $1 AND status = 'tagged'",
    )
    .bind(&layer_key)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    (layer_key, version)
}

/// A new tenant's job on a job layer of the test's own (`name`, `spaces`); `employees` = the job serves the signed-in
/// employee (`jobs.audience`, D129, through `PATCH /api/jobs/{job}`), else customers.
pub async fn own_job(e: &TestEnv, name: &str, spaces: &[OwnSpace], employees: bool) -> ContractJob {
    let template = own_template(e, name, spaces).await;
    let f = design_contract::new_job_for_on(e, uuid::Uuid::new_v4(), template).await;
    if employees {
        let mut q = req(
            "PATCH",
            &format!("/api/jobs/{}", f.job.job_key),
            Some(&super_admin()),
            json!({"audience": "employees"}),
        );
        q.headers_mut()
            .insert("x-active-tenant-id", f.tenant.to_string().parse().unwrap());
        let (s, b) = send(&e.app, q).await;
        assert_eq!(s, StatusCode::OK, "{b}");
    }
    f
}

/// The flow fixture over [`own_job`]: a scope of it in `env` on a fresh Redis prefix and the `agent_runtime` pool.
pub async fn own_fx(test: &str, name: &str, spaces: &[OwnSpace], employees: bool, env: Env) -> Fx {
    let e = common::env_with({
        let vars = r::api_vars(test).await;
        move |v| v.extend(vars.clone())
    })
    .await;
    let f = own_job(&e, name, spaces, employees).await;
    let sub = format!("u-b4-own-{}", f.tenant.simple());
    common::insert_user(&e.owner, &sub, f.tenant, "tenant_admin").await;
    let tok = common::person(&sub, &[]);
    let d = crate::b4::design::Fx { e, f, tok };
    let root = r::root(test).await;
    let scope = Scope::new(d.f.tenant, d.f.job.job_key.clone(), env).unwrap();
    let bus = root.scoped(&scope);
    let rt = common::runtime_pool(&d.e.owner).await;
    Fx {
        d,
        root,
        scope,
        bus,
        rt,
    }
}

struct Desk {
    fx: Fx,
    o: Orchestrator<DesignProcedures>,
    design: RuntimeDesign,
}

async fn desk(test: &str) -> Desk {
    let fx = own_fx(
        test,
        "staff_desk",
        &[OwnSpace {
            key: SPACE,
            slots: &["upn"],
            kinds: &["how_to"],
        }],
        true,
        Env::Test,
    )
    .await;
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

/// The qualifier's fold of "I'm locked out" (the model extracted the space and, wrongly, a typed login).
fn locked_out(d: &RuntimeDesign, typed_login: Option<&str>) -> Draft {
    let ex = Extraction {
        context_space: Some(SPACE.to_owned()),
        slots: typed_login
            .map(|v| ("upn".to_owned(), v.to_owned()))
            .into_iter()
            .collect(),
    };
    match qualify::step(None, ex, &d.spaces, "m-1").0 {
        Outcome::Ready(draft) => draft,
        Outcome::Ask(s) => panic!("the IT desk asked the signed-in employee for {s}"),
        Outcome::Unclear => panic!("{SPACE} is not a space of the IT desk job"),
    }
}

async fn run_case(
    d: &Desk,
    case: &str,
    draft: &Draft,
    level: i16,
) -> Option<BTreeMap<String, String>> {
    let fx = &d.fx;
    let sp = d.design.spaces.by_key(&draft.context_space).unwrap();
    let area = if sp.area.is_empty() {
        draft.context_space.split(':').nth(1).unwrap().to_owned()
    } else {
        sp.area.clone()
    };
    let req: QualifiedRequest = serde_json::from_value(json!({
        "case_id": case, "conversation_id": format!("conv-{case}"), "area": area,
        "context_space": draft.context_space, "request_type": "unlock_account",
        "slots": draft.slots, "customer_id": EMPLOYEE, "identity_level": level,
        "message_id": format!("msg-{case}")
    }))
    .unwrap();
    fl::insert_conversation(fx, &req.conversation_id).await;
    fl::insert_case(fx, case, &fl::current_design(fx).await).await;
    // a case below level 2 has no known customer on its conversation; at level 2 the signed-in employee is it
    if level >= 2 {
        fl::identify_conversation(fx, &req.conversation_id, EMPLOYEE, Some(EMPLOYEE)).await;
    }
    d.o.on_qualified(fl::qualified(&req)).await.unwrap();

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
            d.o.on_result(fl::answer(&c, OpState::Ok, "ok", json!({})))
                .await
                .unwrap();
        }
    }
    let raw: Option<String> = redis::cmd("HGET")
        .arg(d.o.cases().key(case))
        .arg("slots:resolved")
        .query_async(&mut fx.bus.conn())
        .await
        .unwrap();
    raw.map(|r| serde_json::from_str(&r).unwrap())
}

/// "I'm locked out": never `Outcome::Ask` for `upn`, and the resolved `upn` is the verified email, not a typed value.
#[tokio::test]
async fn locked_out_employee_is_never_asked_for_a_login() {
    let d = desk("itdesk-upn").await;
    // the model even extracted a login from the message: it is never kept
    let draft = locked_out(&d.design, Some("mallory@evil.example"));
    assert!(!draft.slots.contains_key("upn"), "{:?}", draft.slots);
    let resolved = run_case(&d, "CS-1", &draft, 2).await;
    assert_eq!(
        resolved
            .as_ref()
            .and_then(|m| m.get("upn"))
            .map(String::as_str),
        Some(EMPLOYEE),
        "{resolved:?}"
    );
    let sp = d.design.spaces.by_key(SPACE).unwrap();
    let area = if sp.area.is_empty() {
        SPACE.split(':').nth(1).unwrap().to_owned()
    } else {
        sp.area.clone()
    };
    let res = fl::resolutions_in(&d.fx, &area, "CS-1").await;
    assert!(res.is_empty(), "nothing is asked or answered: {res:?}");
}

/// Without a verified party (level below 2) nothing is guessed: the case is handed over, never asked.
#[tokio::test]
async fn an_unverified_case_is_not_given_a_login() {
    let d = desk("itdesk-upn-unverified").await;
    let draft = locked_out(&d.design, None);
    let resolved = run_case(&d, "CS-2", &draft, 1).await;
    assert!(resolved.is_none(), "{resolved:?}");
}
