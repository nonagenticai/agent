//! The minimal platform offering every test that creates a job needs, authored through the platform's own HTTP
//! routes as super_admin (no boot seeder, no compiled-in content: a fresh database holds no platform at all).
//!
//! What a job from `job/customer_care_agent@v4` needs, and nothing else:
//!
//! - the `base` layer, carrying the `job` row `customer_care_agent` (`compile` refuses `unknown_job` without it)
//!   and the `profile` row `retail_ecommerce` (`compile_pinned` refuses `profile_not_found` without a profile row
//!   in a pinned family; `release::compile` reads the profile's values from the same row);
//! - one industry layer `industry/retail` and one department layer `industry_dept/retail/customer_service`
//!   (D106/D115: a template's selection and pins hold one industry and one or more of its departments), each with
//!   one `job_setting` row so it can be tagged (`nothing_to_tag` otherwise);
//! - the job layer `job/customer_care_agent` with `template_selection` {profile, industry, departments}, tagged four
//!   times (v1..v4, each tag needs one new untagged row) so the version the callers' literals name (`@v4`) exists;
//! - the `region` dimension with the values the selections name (`region_refusal` reads `dimension_value`).
//!
//! The keys (`retail`, `customer_service`, `customer_care_agent`, `retail_ecommerce`, area `order-support`) are
//! identifiers, not content: the selection options come from these rows (`release::compile::db_options`). Everything else a test asserts about
//! seeded content (activities, procedures, operation expectations, knowledge kinds, gates, other industries) is not
//! here: such a test creates its own rows.
//!
//! [`offering`] is idempotent per database (a per-database `OnceCell`; a database that already holds the tag is left
//! alone). `env()` / `env_with()` / `try_env()` / `env_for()` install it, the contract helpers call it again.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use axum::http::StatusCode;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use uuid::Uuid;

use super::{TestEnv, json as req, send, super_admin};

pub const INDUSTRY: &str = "retail";
pub const DEPARTMENT: &str = "customer_service";
pub const JOB: &str = "customer_care_agent";
pub const PROFILE: &str = "retail_ecommerce";
pub const AREA: &str = "order-support";
pub const INDUSTRY_LAYER: &str = "industry/retail";
pub const DEPARTMENT_LAYER: &str = "industry_dept/retail/customer_service";
pub const JOB_LAYER: &str = "job/customer_care_agent";
/// The job layer's newest tag (what `design_contract::TEMPLATE` and `conv::LIVE_TEMPLATE` name).
pub const VERSION: i32 = 4;
/// The `region` dimension's values (D111): what selections in the tests name.
pub const REGIONS: [&str; 5] = ["eu", "us", "nl", "be", "de"];

/// What [`offering`] made: layer keys and the tagged job template.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offering {
    pub base: &'static str,
    pub industry: &'static str,
    pub department: &'static str,
    pub job_layer: &'static str,
    pub job: &'static str,
    pub profile: &'static str,
    pub version: i32,
}

impl Offering {
    /// The template as `POST /api/jobs` takes it: (layer, version).
    pub fn tag(&self) -> (String, i32) {
        (self.job_layer.to_owned(), self.version)
    }

    /// The template as `POST /api/tenants/{id}/jobs` takes it: `layer@vN`.
    pub fn template(&self) -> String {
        format!("{}@v{}", self.job_layer, self.version)
    }
}

const OFFERING: Offering = Offering {
    base: "base",
    industry: INDUSTRY_LAYER,
    department: DEPARTMENT_LAYER,
    job_layer: JOB_LAYER,
    job: JOB,
    profile: PROFILE,
    version: VERSION,
};

type Cells = Mutex<HashMap<String, Arc<OnceCell<Offering>>>>;

fn cells() -> &'static Cells {
    static CELLS: OnceLock<Cells> = OnceLock::new();
    CELLS.get_or_init(Cells::default)
}

/// Make the platform offering on `e`'s database (once; later calls return at once) and return it. Panics when a
/// route refuses: a test that needs a job needs this.
pub async fn offering(e: &TestEnv) -> Offering {
    try_offering(e)
        .await
        .unwrap_or_else(|m| panic!("platform offering: {m}"))
}

/// [`offering`] with the failure as a message (a database whose auth or schema a test deliberately bent).
pub async fn try_offering(e: &TestEnv) -> Result<Offering, String> {
    let db: String = sqlx::query_scalar("SELECT current_database()")
        .fetch_one(&e.owner)
        .await
        .map_err(|x| x.to_string())?;
    let cell = cells()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .entry(db)
        .or_default()
        .clone();
    cell.get_or_try_init(|| build(e)).await.copied()
}

/// The harness hook (`env*`): the offering, best effort. A failure is reported on stderr and the test goes on; the
/// first helper that needs the offering ([`offering`]) fails loudly with the same message.
pub async fn install(e: &TestEnv) {
    if let Err(m) = try_offering(e).await {
        eprintln!("platform offering not installed: {m}");
    }
}

async fn sa(e: &TestEnv, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
    send(&e.app, req(method, uri, Some(&super_admin()), body)).await
}

fn code(b: &Value) -> &str {
    b["error"]["code"].as_str().unwrap_or("")
}

/// POST `body` to `uri`: the 201 body, or a refusal whose `error.code` is in `tolerated` (already there) as `None`.
async fn post(
    e: &TestEnv,
    uri: &str,
    body: Value,
    tolerated: &[&str],
) -> Result<Option<Value>, String> {
    let (s, b) = sa(e, "POST", uri, body.clone()).await;
    if s == StatusCode::CREATED {
        return Ok(Some(b));
    }
    if s == StatusCode::CONFLICT && tolerated.contains(&code(&b)) {
        return Ok(None);
    }
    Err(format!("POST {uri} {body}: {s} {b}"))
}

/// The id of layer `key`, creating it from `body` when it does not exist.
async fn layer(e: &TestEnv, key: &str, body: Value) -> Result<String, String> {
    let found: Option<String> = sqlx::query_scalar("SELECT id::text FROM layer WHERE key = $1")
        .bind(key)
        .fetch_optional(&e.owner)
        .await
        .map_err(|x| x.to_string())?;
    if let Some(id) = found {
        return Ok(id);
    }
    let v = post(e, "/api/layers", body, &[])
        .await?
        .ok_or_else(|| format!("layer {key}: no body"))?;
    v["id"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("layer {key}: no id in {v}"))
}

/// Add one row to a layer (a row already there is kept).
async fn row(
    e: &TestEnv,
    layer_id: &str,
    kind: &str,
    key: &str,
    fields: Value,
) -> Result<(), String> {
    post(
        e,
        &format!("/api/layers/{layer_id}/rows"),
        json!({"kind": kind, "key": key, "op": "add", "fields": fields}),
        &["duplicate_add"],
    )
    .await
    .map(|_| ())
}

/// Tag family `layer` (a layer key; `/` is `.` in the URL slug). Nothing to tag is fine.
async fn tag(e: &TestEnv, layer: &str) -> Result<(), String> {
    post(
        e,
        &format!("/api/platform/templates/{}/tags", layer.replace('/', ".")),
        json!({}),
        &["nothing_to_tag"],
    )
    .await
    .map(|_| ())
}

fn setting(label: &str) -> Value {
    json!({"label": label})
}

async fn build(e: &TestEnv) -> Result<Offering, String> {
    let o = OFFERING;
    let done: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM job_template_tag WHERE layer = $1 AND version = $2 AND status = 'tagged')",
    )
    .bind(o.job_layer)
    .bind(o.version)
    .fetch_one(&e.owner)
    .await
    .map_err(|x| x.to_string())?;
    if done {
        return Ok(o);
    }

    // the region vocabulary (insert-only; no seeded dimension exists any more)
    post(
        e,
        "/api/dimensions",
        json!({"key": "region", "label": "Region", "kind": "placeholder"}),
        &["dimension_exists"],
    )
    .await?;
    for r in REGIONS {
        post(
            e,
            "/api/dimensions/region/values",
            json!({"key": r, "label": r.to_uppercase()}),
            &["value_exists"],
        )
        .await?;
    }

    // the channel vocabulary (every value the tests select; insert-only)
    post(
        e,
        "/api/dimensions",
        json!({"key": "channel", "label": "Channel", "kind": "dimension"}),
        &["dimension_exists"],
    )
    .await?;
    for c in ["chat", "whatsapp", "email"] {
        post(
            e,
            "/api/dimensions/channel/values",
            json!({"key": c, "label": c}),
            &["value_exists"],
        )
        .await?;
    }

    // the layers (the job layer exists before any tag so the department projects no template of its own)
    let base = layer(e, o.base, json!({"scope": "base", "label": "Base"})).await?;
    let industry = layer(
        e,
        o.industry,
        json!({"scope": "industry", "industry": INDUSTRY, "label": "Retail"}),
    )
    .await?;
    let dept = layer(
        e,
        o.department,
        json!({"scope": "industry_dept", "industry": INDUSTRY, "department": DEPARTMENT,
               "label": "Customer service", "areas": [AREA]}),
    )
    .await?;
    let job = layer(
        e,
        o.job_layer,
        json!({"scope": "job", "job": JOB, "label": "Customer care agent",
        "template_selection": {
            "profile": PROFILE,
            "industry": INDUSTRY,
            "departments": [DEPARTMENT_LAYER],
        }}),
    )
    .await?;

    // base: the job and the profile; one setting each on the industry and the department
    row(
        e,
        &base,
        "job",
        JOB,
        json!({"label": "Customer care agent", "actor": "customer", "stance": "concise",
               "preamble": "You are a helpful assistant."}),
    )
    .await?;
    row(
        e,
        &base,
        "profile",
        PROFILE,
        json!({"label": "Retail e-commerce", "industry": INDUSTRY,
               "selection": {"order_type": ["discrete"]}}),
    )
    .await?;
    row(
        e,
        &industry,
        "job_setting",
        "industry_default",
        setting("Industry default"),
    )
    .await?;
    row(
        e,
        &dept,
        "job_setting",
        "department_default",
        setting("Department default"),
    )
    .await?;
    for family in [o.base, o.industry, o.department] {
        tag(e, family).await?;
    }

    // the job: one new setting per tag, v1..v4
    for (n, key) in ["north_star", "tone", "pace", "scope_note"]
        .into_iter()
        .enumerate()
    {
        row(
            e,
            &job,
            "job_setting",
            key,
            setting(&format!("Setting {}", n + 1)),
        )
        .await?;
        tag(e, o.job_layer).await?;
    }

    let newest: Option<i32> = sqlx::query_scalar(
        "SELECT max(version) FROM job_template_tag WHERE layer = $1 AND status = 'tagged'",
    )
    .bind(o.job_layer)
    .fetch_one(&e.owner)
    .await
    .map_err(|x| x.to_string())?;
    if newest != Some(o.version) {
        return Err(format!(
            "{} is tagged at {newest:?}, expected v{}",
            o.job_layer, o.version
        ));
    }
    Ok(o)
}

// ---- a test connector: one published platform template on the stub transport + a stub instance bound to a job ----

const OPDEFS: &str = include_str!("../../contracts/opdefs.json");

/// The id of the stub instance [`connector`] creates for a tenant.
pub const CONNECTOR_INSTANCE: &str = "test-shop";
/// The key and version of the platform template [`connector`] publishes (once per database).
pub const CONNECTOR_KEY: &str = "test_shop";
pub const CONNECTOR_VERSION: &str = "1";

/// One operation of a test connector: `(object, verb, variant)` from `contracts/opdefs.json`, and the canonical
/// answer its 2xx stub fixture returns (a flat body, one `$.<field>` mapping per output field of the op).
#[derive(Clone, Debug)]
pub struct Op {
    pub object: String,
    pub verb: String,
    pub variant: String,
    output: Option<Value>,
    extra: Vec<Value>,
    path: Option<String>,
    request: Option<String>,
    verify_read: Option<String>,
    field_map: Option<Vec<(String, String)>>,
}

impl Op {
    pub fn new(object: &str, verb: &str, variant: &str) -> Op {
        Op {
            object: object.to_owned(),
            verb: verb.to_owned(),
            variant: variant.to_owned(),
            output: None,
            extra: Vec::new(),
            path: None,
            request: None,
            verify_read: None,
            field_map: None,
        }
    }

    /// `object.verb` with variant `*`.
    pub fn star(object: &str, verb: &str) -> Op {
        Op::new(object, verb, "*")
    }

    /// The flat canonical answer of the default 2xx fixture (keys are output fields of the op; a key the op does
    /// not define is ignored by the mapping). Without it every output field answers `"test"`.
    pub fn returns(mut self, output: Value) -> Op {
        self.output = Some(output);
        self
    }

    /// One more stub fixture (`{name, status, outcome, body, match?}`), tried before the default one when its
    /// `match` fits the request (input-keyed fixtures).
    pub fn fixture(mut self, fixture: Value) -> Op {
        self.extra.push(fixture);
        self
    }

    /// The request path template (default `/<object>/<verb>[/<variant>]` plus the inputs as a query for reads).
    pub fn path(mut self, path: &str) -> Op {
        self.path = Some(path.to_owned());
        self
    }

    /// The request body template of a non-read op (default: every input field as a string).
    pub fn request(mut self, request: &str) -> Op {
        self.request = Some(request.to_owned());
        self
    }

    /// The read op (`object.verb`) an X/W op confirms its effect with.
    pub fn verify_read(mut self, op: &str) -> Op {
        self.verify_read = Some(op.to_owned());
        self
    }

    /// `(json path, output field)` pairs replacing the default `$.<field>` mapping (the fixture body is then
    /// whatever [`Op::returns`] says).
    pub fn map(mut self, field_map: &[(&str, &str)]) -> Op {
        self.field_map = Some(
            field_map
                .iter()
                .map(|(p, f)| ((*p).to_owned(), (*f).to_owned()))
                .collect(),
        );
        self
    }

    fn def(&self) -> Value {
        let defs: Vec<Value> = serde_json::from_str(OPDEFS).expect("opdefs.json");
        defs.into_iter()
            .find(|d| {
                d["object"] == self.object.as_str()
                    && d["verb"] == self.verb.as_str()
                    && d["variant"] == self.variant.as_str()
            })
            .unwrap_or_else(|| {
                panic!(
                    "{}.{} {} is not in contracts/opdefs.json",
                    self.object, self.verb, self.variant
                )
            })
    }

    fn strings(v: &Value) -> Vec<String> {
        v.as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The template op: a GET (reads) or POST (the rest) with the op's input fields rendered from the canonical
    /// input, the output fields mapped one to one from a flat body.
    pub fn template_op(&self) -> Value {
        let def = self.def();
        let side = def["side_effect"].as_str().unwrap_or("R").to_owned();
        let inputs = Self::strings(&def["input_fields"]);
        let outputs = Self::strings(&def["output_fields"]);
        let mut path = format!("/{}/{}", self.object, self.verb);
        if self.variant != "*" {
            path.push('/');
            path.push_str(&self.variant.to_lowercase().replace('_', "-"));
        }
        let (method, path, request) = if side == "R" {
            let query: Vec<String> = inputs
                .iter()
                .map(|f| format!("{f}={{{{canonical.{f}}}}}"))
                .collect();
            let path = if query.is_empty() {
                path
            } else {
                format!("{path}?{}", query.join("&"))
            };
            ("GET", path, String::new())
        } else {
            let body: Vec<String> = inputs
                .iter()
                .map(|f| format!("\"{f}\":\"{{{{canonical.{f}}}}}\""))
                .collect();
            ("POST", path, format!("{{{}}}", body.join(",")))
        };
        // every output field is present in the sample (default "test"); `returns` overrides per key
        let mut body: serde_json::Map<String, Value> =
            outputs.iter().map(|f| (f.clone(), json!("test"))).collect();
        if let Some(Value::Object(o)) = &self.output {
            for (k, v) in o {
                body.insert(k.clone(), v.clone());
            }
        }
        let body = Value::Object(body);
        let mapped: Vec<&String> = outputs.iter().collect();
        let field_map: Vec<Value> = match &self.field_map {
            Some(m) => m
                .iter()
                .map(|(p, f)| json!({"path": p, "field": f}))
                .collect(),
            None => mapped
                .iter()
                .map(|f| json!({"path": format!("$.{f}"), "field": f}))
                .collect(),
        };
        let path = self.path.clone().unwrap_or(path);
        let request = self.request.clone().unwrap_or(request);
        let name = format!("{}.{}", self.object, self.verb);
        let mut fixtures = self.extra.clone();
        fixtures.push(
            json!({"name": format!("{name} ok"), "outcome": "ok", "status": 200, "body": body}),
        );
        fixtures.push(
            json!({"name": "not found", "outcome": "not_found", "status": 404,
                             "body": {"errors": [{"message": "Not Found"}]}}),
        );
        fixtures.push(
            json!({"name": "expired credential", "outcome": "401 -> connector_auth_failed",
                             "status": 401, "body": {"errors": "Unauthorized"}}),
        );
        fixtures.push(
            json!({"name": "upstream 5xx", "outcome": "upstream_unavailable", "status": 503,
                             "body": {"errors": "Unavailable"}}),
        );
        fixtures.push(
            json!({"name": "upstream timeout", "outcome": "timeout", "status": 0, "body": null}),
        );
        let idempotency = if side == "R" {
            json!({"kind": "nat"})
        } else {
            json!({"kind": "dup_search", "on": "case ref"})
        };
        json!({
            "object": self.object, "verb": self.verb, "variant": self.variant, "side_effect": side,
            "op_kind": "standard", "method": method, "path": path, "request": request,
            "field_map": field_map, "status_map": [],
            "fault_map": [
                {"code": "timeout", "system": "fault=timeout"},
                {"code": "upstream_unavailable", "system": "fault=5xx"},
                {"code": "not_found", "system": "HTTP 404"},
                {"code": "connector_auth_failed", "system": "HTTP 401"}
            ],
            "pagination": null, "idempotency": idempotency, "verify_read": self.verify_read,
            "timeout_ms": 3000, "fixtures": fixtures
        })
    }
}

/// What [`connector`] made.
#[derive(Clone, Debug)]
pub struct TestConnector {
    pub key: String,
    pub version: String,
    /// The tenant's stub instance (`env = test`, `stub = true`) on the published template.
    pub instance: String,
    pub tenant: Uuid,
    /// The job whose env-`test` routes point at the instance (`None`: the instance is not routed).
    pub job: Option<Uuid>,
}

impl TestConnector {
    pub fn template(&self) -> String {
        format!("{}@{}", self.key, self.version)
    }
}

/// A platform connector template `test_shop@1` with `ops` on the stub transport (created and published through
/// the super_admin routes, once per database: a second call reuses the template and its first op list), the
/// tenant's stub instance `test-shop` on it, and an `operation_route` row of the job and env `test` per op
/// pointing at the instance (an existing route row of that op is replaced). `job` is a `jobs.id` of `tenant`.
/// [`connector_unrouted`] makes the template and instance without routes.
pub async fn connector(e: &TestEnv, tenant: Uuid, job: Uuid, ops: &[Op]) -> TestConnector {
    try_connector(e, tenant, job, ops)
        .await
        .unwrap_or_else(|m| panic!("test connector: {m}"))
}

pub async fn try_connector(
    e: &TestEnv,
    tenant: Uuid,
    job: Uuid,
    ops: &[Op],
) -> Result<TestConnector, String> {
    connector_at(
        e,
        tenant,
        Some(job),
        CONNECTOR_KEY,
        CONNECTOR_VERSION,
        CONNECTOR_INSTANCE,
        ops,
    )
    .await
}

/// The template and the tenant's stub instance only: no `operation_route` row (the test routes what it needs).
pub async fn connector_unrouted(e: &TestEnv, tenant: Uuid, ops: &[Op]) -> TestConnector {
    connector_at(
        e,
        tenant,
        None,
        CONNECTOR_KEY,
        CONNECTOR_VERSION,
        CONNECTOR_INSTANCE,
        ops,
    )
    .await
    .unwrap_or_else(|m| panic!("test connector: {m}"))
}

/// [`connector`] under another template key / version / instance id (a test that pins an older version, or needs
/// two templates in one database).
pub async fn connector_at(
    e: &TestEnv,
    tenant: Uuid,
    job: Option<Uuid>,
    key: &str,
    version: &str,
    instance: &str,
    ops: &[Op],
) -> Result<TestConnector, String> {
    let c = TestConnector {
        key: key.to_owned(),
        version: version.to_owned(),
        instance: instance.to_owned(),
        tenant,
        job,
    };
    publish_template(e, key, version, ops).await?;
    sqlx::query(
        "INSERT INTO connector_instance (tenant_id, id, label, template_scope, template_key, template_version, \
                                         env, stub, platform, created_by) \
         VALUES ($1, $2, 'Test shop', 'platform', $3, $4, 'test', true, false, 'u-test') \
         ON CONFLICT (tenant_id, id) DO NOTHING",
    )
    .bind(tenant)
    .bind(&c.instance)
    .bind(&c.key)
    .bind(&c.version)
    .execute(&e.owner)
    .await
    .map_err(|x| x.to_string())?;
    for (op, job) in ops.iter().filter_map(|o| job.map(|j| (o, j))) {
        sqlx::query(
            "INSERT INTO operation_route (tenant_id, job_id, object, verb, variant, env, target_kind, instance_id, \
                                          updated_by) \
             VALUES ($1, $2, $3, $4, $5, 'test', 'instance', $6, 'u-test') \
             ON CONFLICT (tenant_id, job_id, object, verb, variant, env) \
             DO UPDATE SET target_kind = 'instance', instance_id = EXCLUDED.instance_id",
        )
        .bind(tenant)
        .bind(job)
        .bind(&op.object)
        .bind(&op.verb)
        .bind(&op.variant)
        .bind(&c.instance)
        .execute(&e.owner)
        .await
        .map_err(|x| x.to_string())?;
    }
    Ok(c)
}

/// Create and publish the platform template `key@version` (transport `rest`, `bearer_static`, credential field
/// `service_token`) with `ops` through the super_admin routes. A template already there is left alone.
pub async fn publish_template(
    e: &TestEnv,
    key: &str,
    version: &str,
    ops: &[Op],
) -> Result<(), String> {
    if draft_template(e, key, version, ops).await? {
        let (s, b) = sa(
            e,
            "POST",
            &format!("/api/platform/connector-templates/{key}/{version}/publish"),
            json!({}),
        )
        .await;
        if s != StatusCode::OK {
            return Err(format!("publish {key}@{version}: {s} {b}"));
        }
    }
    Ok(())
}

/// Create the DRAFT platform template `key@version` (not published). `false` when it already exists.
pub async fn draft_template(
    e: &TestEnv,
    key: &str,
    version: &str,
    ops: &[Op],
) -> Result<bool, String> {
    let body = json!({
        "key": key, "version": version, "label": "Test shop", "vendor": "Test", "system_class": "commerce",
        "wave": "v1", "transport": "rest", "auth_kind": "bearer_static", "vendor_api_pin": "test pin",
        "credential_schema": [{"name": "service_token", "label": "Service token", "secret": true}],
        "ops": ops.iter().map(Op::template_op).collect::<Vec<_>>(),
    });
    Ok(post(
        e,
        "/api/platform/connector-templates",
        body,
        &["template_exists"],
    )
    .await?
    .is_some())
}

/// The declarative base every tenant template builds on, `generic_rest@2` (no operations of its own), created and
/// published through the platform route. Idempotent.
pub async fn generic_rest(e: &TestEnv) -> Result<(), String> {
    let body = json!({
        "key": "generic_rest", "version": "2", "label": "Generic REST", "vendor": "Nonagentic",
        "system_class": "generic", "wave": "base", "transport": "rest", "auth_kind": "api_key_header",
        "description": "Declarative base of every tenant template", "ops": []
    });
    if post(
        e,
        "/api/platform/connector-templates",
        body,
        &["template_exists"],
    )
    .await?
    .is_some()
    {
        let (s, b) = sa(
            e,
            "POST",
            "/api/platform/connector-templates/generic_rest/2/publish",
            json!({}),
        )
        .await;
        if s != StatusCode::OK {
            return Err(format!("publish generic_rest@2: {s} {b}"));
        }
    }
    Ok(())
}

/// The operations of [`stub_shop`]: the order and delivery reads, the delivery problem report and the case creates.
pub fn shop_ops() -> Vec<Op> {
    vec![
        Op::star("order", "read"),
        Op::star("order", "list"),
        Op::star("delivery", "read"),
        Op::star("delivery", "report_problem"),
        Op::star("case", "create"),
        Op::new("case", "create", "return"),
    ]
}

/// The platform template `test_shop@1` ([`shop_ops`], published once per database) and `tenant`'s stub instance
/// `instance` (env `test`) on it. No route rows: the test inserts the ones it needs.
pub async fn stub_shop(e: &TestEnv, tenant: Uuid, instance: &str) -> TestConnector {
    connector_at(e, tenant, None, "test_shop", "1", instance, &shop_ops())
        .await
        .unwrap_or_else(|m| panic!("stub shop: {m}"))
}
