//! The design-target contract (plan B2 T16): what every registered target must satisfy, run against the
//! production registry (`AppState.design`) on a real job created through `POST /api/jobs` (so B2's creation hook
//! has run: `job_target` row + baseline).
//!
//! 1. `get` after `set` round-trips (the write goes through `design::track` / `design::track_tenant`);
//! 2. `places` ⊇ the refs written, and `places()` is `None` exactly for a projection kind (`is_projection()`);
//! 3. `key()` equals every exported fixture sample (`tests/fixtures/design-kinds.json`), scope and projection
//!    flag included;
//! 4. a tracked write, then a tracked write back to the live value, leaves no ledger row;
//! 5. a cut release's stored snapshot (`release_snapshot` rows) is unchanged by a later write;
//! 6. apply and rollback (and re-apply) restore the working rows of a job kind, and leave a tenant-wide kind's
//!    working rows alone (D69);
//! 7. `text(None)` is the fixture's empty text.
//!
//! [`run`] asserts 1–4 and 7, [`run_release`] 5 and 6, [`run_all`] all seven. Each owning plan calls [`run_all`]
//! once per kind it registers (one line per kind; B2's six: `tests/design_contract_b2.rs`). A kind whose samples
//! need fixture rows on the fresh contract job (a system a mapping override names, the routes a cut needs) calls
//! [`run_all_seeded`] with a [`Seed`] instead (B7: `tests/design_contract_b7.rs`).
use agent_api::auth::{Caller, RoleSet, TenantStatus};
use agent_api::design::{
    self, DesignOf, DesignTarget, DraftRef, DraftWrite, JobRef, Label, Place, Scope, Snap, Tracked,
    snap_to_json,
};
use agent_api::tenancy::{JobCtx, TenantCtx};
use axum::http::StatusCode;
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use uuid::Uuid;

use super::{
    TestEnv, get as http_get, insert_tenant, insert_user, json as req, person, send, super_admin,
};

/// The job template every contract job is created from: the tag `common::platform::offering` authors.
pub const TEMPLATE: (&str, i32) = (super::platform::JOB_LAYER, super::platform::VERSION);
/// The sub every contract write is made as.
pub const DESIGNER: &str = "u-designer";

/// Fixture rows a kind's samples need on each fresh contract job, written (owner pool) right after the job is
/// created and before any tracked write.
pub type Seed = for<'a> fn(&'a TestEnv, &'a ContractJob) -> BoxFuture<'a, ()>;

/// A new contract job ([`new_job`]), then `seed`.
async fn seeded_job(e: &TestEnv, seed: Option<Seed>) -> ContractJob {
    let f = new_job(e).await;
    if let Some(s) = seed {
        s(e, &f).await;
    }
    f
}

/// A tenant with one job created through the API (the creation hook ran), and the context to write it as.
pub struct ContractJob {
    pub tenant: Uuid,
    pub job: JobCtx,
}

impl ContractJob {
    pub fn job_ref(&self) -> JobRef {
        JobRef {
            id: self.job.job_id,
            key: self.job.job_key.clone(),
        }
    }
}

fn caller() -> Caller {
    Caller {
        sub: DESIGNER.to_string(),
        actor: DESIGNER.to_string(),
        home: None,
        super_admin: false,
        session: "s-contract".to_string(),
        verified_email: None,
    }
}

/// A new tenant and a job created from [`TEMPLATE`] by `POST /api/jobs` (a super_admin acting in the tenant).
pub async fn new_job(e: &TestEnv) -> ContractJob {
    new_job_for(e, Uuid::new_v4()).await
}

/// [`new_job`] for a tenant id chosen beforehand (one an env variable already names); the tenant must not exist.
pub async fn new_job_for(e: &TestEnv, tenant: Uuid) -> ContractJob {
    new_job_for_on(e, tenant, (TEMPLATE.0.to_owned(), TEMPLATE.1)).await
}

/// The newest tagged `job/customer_care_agent`: the stack that carries seed v8's `activity` rows (operations,
/// objects) and the retail spaces' `activity` (R2 — the fold is the one activity source; the older tags pin the
/// revisions from before v8, which have neither).
pub async fn latest_template(e: &TestEnv) -> (String, i32) {
    super::platform::offering(e).await;
    sqlx::query_as(
        "SELECT layer, version FROM job_template_tag \
          WHERE layer = 'job/customer_care_agent' AND status = 'tagged' ORDER BY version DESC LIMIT 1",
    )
    .fetch_one(&e.owner)
    .await
    .expect("a tagged customer-care template")
}

/// [`new_job`] on [`latest_template`] (what a tenant creating a job today gets).
pub async fn new_job_latest(e: &TestEnv) -> ContractJob {
    let t = latest_template(e).await;
    new_job_for_on(e, Uuid::new_v4(), t).await
}

/// [`new_job_for`] on the template `template` = (layer, version).
pub async fn new_job_for_on(e: &TestEnv, tenant: Uuid, template: (String, i32)) -> ContractJob {
    super::platform::offering(e).await;
    // tenant names are unique (tenants_name_lower) and the tests in one binary share a database
    insert_tenant(&e.owner, tenant, &format!("Contract {tenant}")).await;
    // B7 gate 2 fails closed: the job's tenant runs turns, so it holds a fresh active licence verdict
    super::licence_active(&e.owner, tenant).await;
    let mut r = req(
        "POST",
        "/api/jobs",
        Some(&super_admin()),
        json!({"template": format!("{}@v{}", template.0, template.1)}),
    );
    r.headers_mut()
        .insert("x-active-tenant-id", tenant.to_string().parse().unwrap());
    let (status, body) = send(&e.app, r).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let key = body["key"].as_str().expect("JobSummary.key").to_string();
    let id: Uuid = sqlx::query_scalar("SELECT id FROM jobs WHERE tenant_id = $1 AND key = $2")
        .bind(tenant)
        .bind(&key)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let f = contract_job(tenant, id, &key);
    // R5 / D110: the template's selection uses three Production operations and required knowledge kinds; unrouted
    // and unbound, no release of it could be applied
    publishable(e, &f).await;
    f
}

/// What every contract job needs before a release of it can be applied: [`route_required_prod_ops`] and
/// [`bind_required_knowledge`]. Idempotent.
pub async fn publishable(e: &TestEnv, f: &ContractJob) {
    route_required_prod_ops(e, f).await;
    bind_required_knowledge(e, f).await;
}

/// The write context of an existing job `id` (`key`) of `tenant`, acting as [`DESIGNER`] with every role.
pub fn contract_job(tenant: Uuid, id: Uuid, key: &str) -> ContractJob {
    let t = TenantCtx::new(
        tenant,
        TenantStatus::Active,
        RoleSet::ALL,
        None,
        caller(),
        false,
    );
    ContractJob {
        tenant,
        job: JobCtx {
            t,
            job_id: id,
            job_key: key.to_string(),
        },
    }
}

/// One tracked write of `kind` at `r` = `v` (`None` = remove), through `target.set`, committed.
pub async fn tracked_set(
    e: &TestEnv,
    f: &ContractJob,
    kind: &'static str,
    r: &DraftRef,
    v: Snap,
) -> Tracked<()> {
    let reg = e.state.design.clone();
    let (tenant, jr, place_ref) = (f.tenant, f.job_ref(), r.clone());
    let w = DraftWrite {
        area: "contract",
        page: "J7",
        kind,
        r#ref: r.clone(),
        label: Label::Now(format!("contract write of {kind}")),
    };
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let out = design::track::<(), _>(&mut tx, &e.state.design, &f.job, w, move |c| {
        Box::pin(async move {
            let at = Place {
                tenant,
                job: Some(&jr),
            };
            reg.target(kind)?.set(c, at, &place_ref, &v).await
        })
    })
    .await
    .unwrap_or_else(|err| panic!("tracked write of {kind}: {err:?}"));
    tx.commit().await.unwrap();
    out
}

/// The draft value of `kind` at `r` (`target.get`), in its own job-scoped transaction.
pub async fn get(e: &TestEnv, f: &ContractJob, kind: &str, r: &DraftRef) -> Snap {
    let jr = f.job_ref();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let at = Place {
        tenant: f.tenant,
        job: Some(&jr),
    };
    let t = e.state.design.target(kind).unwrap();
    let v = t.get(&mut tx, at, r).await.unwrap();
    tx.commit().await.unwrap();
    v
}

/// The places of `kind` (`target.places`), in its own job-scoped transaction.
pub async fn places(e: &TestEnv, f: &ContractJob, kind: &str) -> Option<Vec<DraftRef>> {
    let jr = f.job_ref();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let at = Place {
        tenant: f.tenant,
        job: Some(&jr),
    };
    let t = e.state.design.target(kind).unwrap();
    let v = t.places(&mut tx, at).await.unwrap();
    tx.commit().await.unwrap();
    v
}

async fn ledger_keys(e: &TestEnv, job: Uuid) -> Vec<String> {
    sqlx::query_scalar("SELECT key FROM draft_ledger WHERE job_id = $1 ORDER BY key")
        .bind(job)
        .fetch_all(&e.owner)
        .await
        .unwrap()
}

/// Item 3: every way `t` disagrees with the exported fixture (`tests/fixtures/design-kinds.json`).
fn fixture_mismatches(t: &dyn DesignTarget) -> Vec<String> {
    let all: Value = serde_json::from_str(include_str!("../fixtures/design-kinds.json"))
        .expect("design-kinds.json is JSON");
    let kind = t.kind();
    let Some(entry) = all
        .as_array()
        .expect("design-kinds.json is an array")
        .iter()
        .find(|k| k["kind"] == kind)
    else {
        return vec![format!("{kind}: no fixture entry")];
    };
    let mut out = Vec::new();
    let scope = match t.scope() {
        Scope::Job => "job",
        Scope::Tenant => "tenant",
    };
    if entry["scope"] != scope {
        out.push(format!("{kind}: scope {scope}, fixture {}", entry["scope"]));
    }
    if entry["projection"] != t.is_projection() {
        out.push(format!("{kind}: is_projection differs from the fixture"));
    }
    let samples = entry["samples"].as_array().cloned().unwrap_or_default();
    if samples.is_empty() {
        out.push(format!("{kind}: the fixture has no samples"));
    }
    for s in samples {
        let r: DraftRef = s["ref"].as_object().cloned().unwrap_or_default();
        let got = t.key(&r);
        if s["key"] != got.as_str() {
            out.push(format!(
                "{kind}: key({}) = {got:?}, fixture {}",
                s["ref"], s["key"]
            ));
        }
    }
    out
}

/// Run contract items 1–4 and 7 for `kind` (job-scoped or tenant-wide). `samples` = places and values to write; each value must
/// differ from the place's live value on a fresh job. `empty_text` = the fixture `text` of "nothing at the place".
pub async fn run(
    e: &TestEnv,
    kind: &'static str,
    samples: &[(DraftRef, Value)],
    empty_text: Option<&str>,
) {
    run_seeded(e, kind, samples, empty_text, None).await;
}

/// [`run`] on a contract job prepared by `seed`.
pub async fn run_seeded(
    e: &TestEnv,
    kind: &'static str,
    samples: &[(DraftRef, Value)],
    empty_text: Option<&str>,
    seed: Option<Seed>,
) {
    assert!(!samples.is_empty(), "{kind}: the contract needs samples");
    let t = e.state.design.target(kind).unwrap();

    // 3. key / scope / projection flag = the fixture's
    let bad = fixture_mismatches(t);
    assert!(bad.is_empty(), "{bad:#?}");

    // 7. text of nothing
    assert_eq!(
        t.text(&None, &samples[0].0).as_deref(),
        empty_text,
        "{kind}: text(None) must be the fixture's empty text"
    );

    let f = seeded_job(e, seed).await;
    for (r, value) in samples {
        let key = t.key(r);
        let before = get(e, &f, kind, r).await;
        assert_ne!(
            before.as_ref(),
            Some(value),
            "{kind}: sample {key} must differ from the live value"
        );

        // 1. get after set round-trips
        let first = write(e, &f, kind, r, Some(value.clone())).await;
        assert!(
            first.change.is_some(),
            "{kind}: the write records a ledger row for {key}"
        );
        assert_eq!(
            get(e, &f, kind, r).await.as_ref(),
            Some(value),
            "{kind}: get after set round-trips at {key}"
        );

        // 2. places ⊇ refs written; places() is None exactly for a projection kind
        let ps = places(e, &f, kind).await;
        assert_eq!(
            ps.is_none(),
            t.is_projection(),
            "{kind}: places() None ⇔ is_projection()"
        );
        // a projection kind lives in its parent's place: the parent's places hold it
        let (pkind, pref) = place(e, kind, r);
        let ps = match ps {
            Some(ps) => ps,
            None => places(e, &f, pkind).await.unwrap_or_default(),
        };
        assert!(
            ps.contains(&pref),
            "{kind}: places of {pkind} {ps:?} must contain the written {pref:?}"
        );

        // 4. a write back to the live value leaves no ledger row
        assert!(ledger_keys(e, f.job.job_id).await.contains(&key));
        let back = write(e, &f, kind, r, before.clone()).await;
        assert!(
            back.change.is_none(),
            "{kind}: a write back to live drops the row for {key}"
        );
        assert!(
            !ledger_keys(e, f.job.job_id).await.contains(&key),
            "{kind}: no ledger row is left for {key}"
        );
        assert_eq!(
            get(e, &f, kind, r).await,
            before,
            "{kind}: the write back restored {key}"
        );
    }
}

/// The snapshot place `(kind, ref)` that `(kind, r)` is stored in: itself, or a projection kind's parent (fixture
/// `placeOf`).
fn place(e: &TestEnv, kind: &'static str, r: &DraftRef) -> (&'static str, DraftRef) {
    let t = e.state.design.target(kind).unwrap();
    t.parent(r).unwrap_or_else(|| (kind, r.clone()))
}

/// One tracked tenant-wide (D69) write of `kind` at `r` = `v` (`None` = remove), through `target.set` at the
/// tenant place, committed.
pub async fn tracked_tenant_set(
    e: &TestEnv,
    f: &ContractJob,
    kind: &'static str,
    r: &DraftRef,
    v: Snap,
) -> Tracked<()> {
    let reg = e.state.design.clone();
    let (tenant, place_ref) = (f.tenant, r.clone());
    let w = DraftWrite {
        area: "contract",
        page: "J7",
        kind,
        r#ref: r.clone(),
        label: Label::Now(format!("contract write of {kind}")),
    };
    let t = f.job.t.clone();
    let mut tx = t.begin(&e.state.db).await.unwrap();
    let out = design::track_tenant::<(), _>(&mut tx, &e.state.design, &t, w, move |c| {
        Box::pin(async move {
            let at = Place { tenant, job: None };
            reg.target(kind)?.set(c, at, &place_ref, &v).await
        })
    })
    .await
    .unwrap_or_else(|err| panic!("tracked tenant write of {kind}: {err:?}"));
    tx.commit().await.unwrap();
    out
}

/// One tracked write of `kind` by its scope: [`tracked_set`] (job) or [`tracked_tenant_set`] (tenant, D69).
pub async fn write(
    e: &TestEnv,
    f: &ContractJob,
    kind: &'static str,
    r: &DraftRef,
    v: Snap,
) -> Tracked<()> {
    match e.state.design.target(kind).unwrap().scope() {
        Scope::Job => tracked_set(e, f, kind, r, v).await,
        Scope::Tenant => tracked_tenant_set(e, f, kind, r, v).await,
    }
}

/// A complete selection of the seed options (compiles with no violation): [`run_release`] cuts with it.
pub fn cuttable_selection() -> Value {
    json!({
        "profile": "retail_ecommerce",
        "industry": "retail",
        "departments": ["industry_dept/retail/customer_service"],
        "areas": ["order-support"],
        "dimensions": {"channel": ["chat"]}, "region": "eu",
    })
}

/// A release manager of `f`'s tenant (`tenant_admin`); returns its token.
pub async fn release_manager(e: &TestEnv, f: &ContractJob) -> String {
    let sub = format!("u-contract-rm-{}", f.tenant.simple());
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM users WHERE sub = $1)")
        .bind(&sub)
        .fetch_one(&e.owner)
        .await
        .unwrap();
    if !exists {
        insert_user(&e.owner, &sub, f.tenant, "tenant_admin").await;
    }
    person(&sub, &[])
}

fn job_uri(f: &ContractJob, tail: &str) -> String {
    format!("/api/jobs/{}/{tail}", f.job.job_key)
}

/// Approve (cut) the draft through `POST …/releases`; `apply` = approve and apply in one call. Returns the new
/// release id (the cut must be new).
pub async fn cut(e: &TestEnv, f: &ContractJob, tok: &str, apply: bool) -> String {
    let (s, b) = approve(e, f, tok, apply).await;
    assert!(
        s == StatusCode::CREATED || s == StatusCode::OK,
        "approve: {s}: {b}"
    );
    assert_eq!(b["created"], true, "approve cuts a new release: {b}");
    assert_eq!(b["applied"], apply, "{b}");
    b["release"]["id"].as_str().expect("release id").to_string()
}

/// `POST …/releases` (approve; `apply` = and apply) as `tok`, with the selection as `GET …/target` shows it: the
/// raw answer (a refusal is a result).
pub async fn approve(e: &TestEnv, f: &ContractJob, tok: &str, apply: bool) -> (StatusCode, Value) {
    let (s, b) = send(&e.app, http_get(&job_uri(f, "target"), Some(tok))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let seen = b["selection_updated_at"].clone();
    let body = json!({"apply": apply, "selection_seen_at": seen});
    send(
        &e.app,
        req("POST", &job_uri(f, "releases"), Some(tok), body),
    )
    .await
}

/// Give `f`'s job the [`cuttable_selection`] (a tracked write) unless it already has it, then approve and apply its
/// draft as a release manager: Production = the draft. Returns the release id.
pub async fn publish(e: &TestEnv, f: &ContractJob) -> String {
    let sel = DraftRef::new();
    if get(e, f, "selection", &sel).await != Some(cuttable_selection()) {
        write(e, f, "selection", &sel, Some(cuttable_selection())).await;
    }
    bind_required_knowledge(e, f).await;
    let tok = release_manager(e, f).await;
    cut(e, f, &tok, true).await
}

/// The fixture tenant leaf [`bind_required_knowledge`] binds (keys root-first).
pub const FIXTURE_LEAF: &str = "kh-fixture/kh-required";
/// The languages the fixture leaf holds content in (one enabled `file` source each): every primary language a test
/// job serves.
pub const FIXTURE_LANGUAGES: [&str; 4] = ["en", "de", "fr", "nl"];

/// D110 (KH T8): give `f`'s job what the knowledge publish gate asks of it — every required kind its template folds
/// for its selection (the stored one, else [`cuttable_selection`]) is a working row bound to the tenant leaf
/// [`FIXTURE_LEAF`] (no strata store: nothing is searched at runtime), and the leaf has one source per
/// [`FIXTURE_LANGUAGES`] language with a completed run that produced a document (primary-language coverage and a fed
/// leaf; no upload, so coverage counts and language levels are unchanged). Owner-pool / job-scoped fixture rows written before any cut: no ledger row. Idempotent ([`new_job`]
/// and [`publish`] call it); a kind already bound keeps its binding.
pub async fn bind_required_knowledge(e: &TestEnv, f: &ContractJob) {
    use agent_api::knowledge::kinds;
    use agent_api::platform::templates::TemplatePin;
    use agent_model::compile::Selection;

    // the leaf: root group `kh-fixture`, leaf `kh-required` (no strata store)
    let root = fixture_store(e, f, "kh-fixture", "group", None).await;
    let leaf = fixture_store(e, f, "kh-required", "leaf", Some(root)).await;
    for lang in FIXTURE_LANGUAGES {
        sqlx::query(
            "INSERT INTO source (tenant_id, job_id, id, name, type, target_store_id, language, updated_by) \
             VALUES ($1, $2, gen_random_uuid(), $3, 'file', $4, $5, 'test') \
             ON CONFLICT (tenant_id, job_id, name) DO NOTHING",
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .bind(format!("kh-fixture-{lang}"))
        .bind(leaf)
        .bind(lang)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    // each fixture source ran once and produced a document (what makes a source feed its leaf, D110)
    sqlx::query(
        "INSERT INTO source_run (tenant_id, job_id, id, source_id, status, finished_at, started_by, documents) \
         SELECT s.tenant_id, s.job_id, gen_random_uuid(), s.id, 'completed', now(), 'test', 1 FROM source s \
          WHERE s.job_id = $1 AND s.name LIKE 'kh-fixture-%' \
            AND NOT EXISTS (SELECT 1 FROM source_run r WHERE r.source_id = s.id)",
    )
    .bind(f.job.job_id)
    .execute(&e.owner)
    .await
    .unwrap();
    // the template's kinds under the job's selection, as working rows
    let (pins, stored): (Value, Option<Value>) = sqlx::query_as(
        "SELECT j.pins, t.selection FROM jobs j LEFT JOIN job_target t ON t.job_id = j.id WHERE j.id = $1",
    )
    .bind(f.job.job_id)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    let pins: Vec<TemplatePin> = serde_json::from_value(pins).unwrap();
    let sel: Selection = stored
        .and_then(|v| serde_json::from_value::<Selection>(v).ok())
        .filter(|s| !s.industry.is_empty() && !s.departments.is_empty())
        .unwrap_or_else(|| serde_json::from_value(cuttable_selection()).unwrap());
    let jr = f.job_ref();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let seeds = kinds::template_seeds(&mut tx, &pins, Some(&sel), Some(&jr))
        .await
        .unwrap();
    kinds::materialise(&mut tx, &jr, &seeds).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query(
        "UPDATE job_knowledge_kind SET store_path = $2 \
          WHERE job_id = $1 AND required AND store_path IS NULL",
    )
    .bind(f.job.job_id)
    .bind(FIXTURE_LEAF)
    .execute(&e.owner)
    .await
    .unwrap();
}

/// A fixture knowledge store of `f`'s tenant (`key` under `parent`), created once; its id.
async fn fixture_store(
    e: &TestEnv,
    f: &ContractJob,
    key: &str,
    kind: &str,
    parent: Option<Uuid>,
) -> Uuid {
    let found: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM knowledge_store WHERE tenant_id = $1 AND key = $2 \
            AND parent_id IS NOT DISTINCT FROM $3",
    )
    .bind(f.tenant)
    .bind(key)
    .bind(parent)
    .fetch_optional(&e.owner)
    .await
    .unwrap();
    if let Some(id) = found {
        return id;
    }
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO knowledge_store (tenant_id, id, key, label, parent_id, kind, region, updated_by) \
         VALUES ($1, $2, $3, $3, $4, $5, 'eu', 'test')",
    )
    .bind(f.tenant)
    .bind(id)
    .bind(key)
    .bind(parent)
    .bind(kind)
    .execute(&e.owner)
    .await
    .unwrap();
    id
}

/// B7 (D67, R5): route the three required Production operations of [`cuttable_selection`] (`knowledge.search`,
/// `case.read`, `case.create{return}` on prod) to fallbacks that run (owner pool, fixture rows: no ledger row), so the
/// job passes the routing publish gate. Idempotent ([`new_job`] already calls it): a row already there is kept.
pub async fn route_required_prod_ops(e: &TestEnv, f: &ContractJob) {
    for (object, verb, kind) in [
        ("knowledge", "search", "builtin"),
        ("case", "read", "agent_cases"),
        ("case", "create", "agent_cases"),
    ] {
        sqlx::query(
            "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, updated_by) \
             VALUES ($1, $2, $3, $4, '*', 'prod', $5, 'test') \
             ON CONFLICT (tenant_id, job_id, object, verb, variant, env) DO NOTHING",
        )
        .bind(f.tenant)
        .bind(f.job.job_id)
        .bind(object)
        .bind(verb)
        .bind(kind)
        .execute(&e.owner)
        .await
        .unwrap();
    }
}

/// Apply release `id` (`POST …/releases/{id}/apply`); returns the `direction` (`apply` | `rollback` | `reapply`).
async fn apply(e: &TestEnv, f: &ContractJob, tok: &str, id: &str) -> String {
    let path = job_uri(f, &format!("releases/{id}/apply"));
    let (s, b) = send(&e.app, req("POST", &path, Some(tok), json!({}))).await;
    assert_eq!(s, StatusCode::OK, "apply {id}: {b}");
    assert_eq!(b["applied"], true, "{b}");
    b["direction"].as_str().unwrap_or("").to_string()
}

/// Every stored place of release `id` as `(key, snap)`, straight from `release_snapshot` (no read cache).
async fn stored(e: &TestEnv, f: &ContractJob, id: &str) -> Vec<(String, Option<Value>)> {
    sqlx::query_as(
        "SELECT key, snap FROM release_snapshot WHERE job_id = $1 AND release_id = $2 ORDER BY key",
    )
    .bind(f.job.job_id)
    .bind(id)
    .fetch_all(&e.owner)
    .await
    .unwrap()
}

/// The value of `kind` at `r` in the job's design `of` (`design::value`).
async fn design_value(
    e: &TestEnv,
    f: &ContractJob,
    of: DesignOf,
    kind: &str,
    r: &DraftRef,
) -> Snap {
    let jr = f.job_ref();
    let mut tx = f.job.begin(&e.state.db).await.unwrap();
    let v = design::value(&mut tx, &e.state.design, &jr, &of, kind, r)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    v
}

/// Run contract items 5 and 6 for `kind` at `r` with `value` (it must differ from the place's live value, and a
/// job whose draft holds it must still cut: no bundle violation). On a fresh job: R0 = [`cuttable_selection`],
/// approved and applied; write `value`; cut R1; write back the live value; apply R1; roll back to R0; re-apply R1.
pub async fn run_release(e: &TestEnv, kind: &'static str, r: &DraftRef, value: Value) {
    run_release_seeded(e, kind, r, value, None).await;
}

/// [`run_release`] on a contract job prepared by `seed`.
pub async fn run_release_seeded(
    e: &TestEnv,
    kind: &'static str,
    r: &DraftRef,
    value: Value,
    seed: Option<Seed>,
) {
    let t = e.state.design.target(kind).unwrap();
    let key = t.key(r);
    let (pkind, pref) = place(e, kind, r);
    let place_key = e.state.design.target(pkind).unwrap().key(&pref);
    let f = seeded_job(e, seed).await;
    let tok = release_manager(e, &f).await;
    write(
        e,
        &f,
        "selection",
        &DraftRef::new(),
        Some(cuttable_selection()),
    )
    .await;
    let r0 = cut(e, &f, &tok, true).await;
    let live0 = get(e, &f, kind, r).await;
    assert_ne!(
        live0.as_ref(),
        Some(&value),
        "{kind}: the release sample {key} must differ from the live value"
    );

    // 5. the cut snapshot is unchanged by a later write
    write(e, &f, kind, r, Some(value.clone())).await;
    let r1 = cut(e, &f, &tok, false).await;
    let snap = stored(e, &f, &r1).await;
    let row = snap.iter().find(|(k, _)| *k == place_key);
    assert!(
        row.is_some(),
        "{kind}: release {r1} stores the place {place_key}: {snap:?}"
    );
    if pkind == kind {
        assert_eq!(
            row.map(|(_, s)| s.clone()),
            Some(snap_to_json(&Some(value.clone()))),
            "{kind}: release {r1} stores the draft value at {key}"
        );
    }
    let in_r1 = || design_value(e, &f, DesignOf::Release(r1.clone()), kind, r);
    assert_eq!(in_r1().await, Some(value.clone()), "{kind}: {r1} at {key}");
    write(e, &f, kind, r, live0.clone()).await;
    assert_eq!(get(e, &f, kind, r).await, live0, "{kind}: the later write");
    assert_eq!(
        stored(e, &f, &r1).await,
        snap,
        "{kind}: a later write leaves release {r1}'s snapshot unchanged"
    );
    assert_eq!(in_r1().await, Some(value.clone()), "{kind}: {r1} at {key}");

    // 6. apply / rollback / re-apply: job kinds follow live, tenant kinds are left alone (D69)
    write(e, &f, kind, r, Some(value.clone())).await;
    assert_eq!(apply(e, &f, &tok, &r1).await, "apply");
    assert_eq!(
        get(e, &f, kind, r).await,
        Some(value.clone()),
        "{kind}: applied"
    );
    assert!(
        !ledger_keys(e, f.job.job_id).await.contains(&key),
        "{kind}: publishing {key} clears its ledger row"
    );
    assert_eq!(apply(e, &f, &tok, &r0).await, "rollback");
    let kept = match t.scope() {
        Scope::Job => live0.clone(),
        Scope::Tenant => Some(value.clone()),
    };
    assert_eq!(
        get(e, &f, kind, r).await,
        kept,
        "{kind}: a rollback restores a job kind's working rows and never rewrites a tenant kind's"
    );
    assert_eq!(
        design_value(e, &f, DesignOf::Live, kind, r).await,
        live0,
        "{kind}: live is {r0}'s value after the rollback"
    );
    assert_eq!(apply(e, &f, &tok, &r1).await, "reapply");
    assert_eq!(
        get(e, &f, kind, r).await,
        Some(value),
        "{kind}: a re-apply restores the working rows"
    );
}

/// The whole contract (items 1–7) for `kind`: [`run`] over `samples`, then [`run_release`] with `samples[0]` (which
/// must therefore also cut). One call per registered kind.
pub async fn run_all(
    e: &TestEnv,
    kind: &'static str,
    samples: &[(DraftRef, Value)],
    empty_text: Option<&str>,
) {
    run_all_seeded(e, kind, samples, empty_text, None).await;
}

/// [`run_all`] on contract jobs prepared by `seed`.
pub async fn run_all_seeded(
    e: &TestEnv,
    kind: &'static str,
    samples: &[(DraftRef, Value)],
    empty_text: Option<&str>,
    seed: Option<Seed>,
) {
    run_seeded(e, kind, samples, empty_text, seed).await;
    let (r, value) = samples.first().expect("the contract needs samples");
    run_release_seeded(e, kind, r, value.clone(), seed).await;
}

/// T3 ruling: `is_projection()` ⇔ `places()` returns `None`, for every kind registered in `e`'s registry.
pub async fn projection_agrees_with_places(e: &TestEnv) {
    let f = new_job(e).await;
    let mut bad = Vec::new();
    for kind in e.state.design.kinds() {
        let t = e.state.design.target(kind).unwrap();
        if places(e, &f, kind).await.is_none() != t.is_projection() {
            bad.push(kind);
        }
    }
    assert!(
        bad.is_empty(),
        "is_projection() disagrees with places() == None for {bad:?}"
    );
}
