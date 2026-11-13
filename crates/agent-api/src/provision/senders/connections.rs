//! The `connections_put` outbox sender (B3 T4; plan §0 R5, §5.1, G2; interim until W5 A2 versioned connections):
//! push the provider slots a job published to strata. One sender for both targets: the row's target names the
//! service (`strata_rag` holds embed/rerank/chat, `strata_ingest` ocr/vision/transcribe).
//!
//! strata's `PUT /api/tenant/providers` replaces the tenant's WHOLE set (a capability left out is removed), so a push
//! is a read-modify-write: `GET` the current set, keep every other tenant registration as strata reports it (without
//! `api_key`: strata keeps a stored key for the same `base_url`), replace the published slots, `PUT` the result.
//! That read-modify-write runs under `pg_advisory_xact_lock` on `connections_put:<tenant>:<service>`, in a
//! transaction of its own (never the request's), holding no row lock; the lock serialises agent's own pushes only —
//! a writer outside agent can still land between the GET and the PUT (G2, until A2's `expected_version`).
//!
//! - A slot entry with a `key_ref` sends that sealed key (opened here, AAD `<tenant>:slot_key:<id>`) as `api_key`;
//!   without one (an adopted drift, or the same key kept) no `api_key` is sent. The key appears in no log, payload,
//!   result or `last_error`: every upstream body is redacted of it before the drainer stores anything.
//! - 422 from strata (its live probe or validation refused the registration) fails the row as `slot_test_failed`
//!   with strata's message and code; any other answer is classified by the drainer (06 §4.5).
//! - No URL for the service in this deployment → failed `strata_unconfigured`; no token pair for the tenant yet (G1)
//!   → waits (`provision_pending`).
//! - `on_success` records, per pushed slot, the fingerprint strata now holds (`provider_slot_remote`): the baseline
//!   drift is measured against, and the key it sent (`pushed_key_ref`, 0057).
//! - An embed change never retargets an existing store (T4a ruling 2026-10-21; R7: one embed model per tree, and a
//!   root without a store binding follows the tenant row): before the tenant PUT, still under the lock, every strata
//!   root that inherits the tenant's embed registration and was embedded with it is bound to that registration at
//!   store level (`PUT /api/stores/{id}/providers`, its own rerank/chat bindings kept), so only roots created later
//!   take the new default; existing roots move only through a model change (crate::knowledge::model_changes).
//!   Idempotent: a root already bound at store level, or already in the new model, is left alone. A root that
//!   cannot be pinned (strata refuses, agent does not hold the key strata holds, or it has content in a model the
//!   tenant row no longer names) fails the row `embed_pin_failed` before the tenant row is touched.
//! - strata cannot pin its DEPLOYMENT DEFAULT per store (ruling 2026-10-21, revised): while the tenant row has no
//!   embed registration (`source: default`) and a strata root without a store binding holds content that a job still
//!   reads (its draft or live pin, crate::knowledge::generations; a root agent did not make counts as read), the push
//!   WITHHOLDS the embed change: the tenant `embed` stays as strata has it, the other slots go out, and the result
//!   carries `withheld: [<the embed entry>]` and `withheld_roots: [<strata root names>]` (not recorded as pushed; the
//!   embed slot view and the providers readiness area show it). The slot save fanned out model changes for those
//!   roots (crate::knowledge::model_changes::on_slot_saved); when a job accepts or publishes one, on every
//!   model-change sweep and on `POST /api/providers/slots/embed/push`, [`slots::requeue_withheld`] queues the
//!   withheld entry again and the next push sends it once nothing reads a default-following root any more.
use std::collections::HashSet;
use std::time::Duration;

use futures_util::future::BoxFuture;
use reqwest::Method;
use serde_json::{Map, Value, json};
use sqlx::PgConnection;

use crate::design::{DesignOf, JobRef};
use crate::error::{ApiError, ApiResult};
use crate::knowledge::{generations, stores};
use crate::netguard;
use crate::providers::kinds::Slot;
use crate::providers::secret::{self, SecretKind, SecretText};
use crate::providers::slots::{self, OP, PushPayload, PushSlot, WITHHELD, WITHHELD_ROOTS};
use crate::provision::{OutboxRow, SendResult, Sender, Target};
use crate::state::AppState;
use crate::strata::{self, Plane, Service, rag};
use crate::tenancy::begin_scoped;
use crate::upstream::{Auth, DEFAULT_TIMEOUT, LONG_TIMEOUT};

/// The path both services serve the tenant's provider set on.
const PROVIDERS: &str = "/api/tenant/providers";
/// The failure code of a root store that could not keep its embed model.
pub const PIN_FAILED: &str = "embed_pin_failed";
/// The `Keys` label of the key a pin sends (never a slot name, so [`Keys::of`] cannot pick it).
const PIN_KEY: &str = "embed:pin";

/// The `connections_put` sender (op [`OP`], targets `strata_rag` and `strata_ingest`).
pub struct ConnectionsPut;

impl Sender for ConnectionsPut {
    fn op(&self) -> &'static str {
        OP
    }

    fn send<'a>(&'a self, st: &'a AppState, row: &'a OutboxRow) -> BoxFuture<'a, SendResult> {
        Box::pin(send(st, row))
    }

    fn on_success<'a>(
        &'a self,
        tx: &'a mut PgConnection,
        row: &'a OutboxRow,
        result: &'a Value,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(record(tx, row, result))
    }
}

/// The service a row's target names.
fn service_of(target: &str) -> Option<Service> {
    match Target::parse(target)? {
        Target::StrataRag => Some(Service::Rag),
        Target::StrataIngest => Some(Service::Ingest),
        Target::Licences => None,
    }
}

/// The opened keys of a push, by slot. Never printed.
struct Keys(Vec<(String, SecretText)>);

impl Keys {
    fn of(&self, slot: &str) -> Option<&SecretText> {
        self.0.iter().find(|(s, _)| s == slot).map(|(_, k)| k)
    }

    /// `v` with every key replaced by `[redacted]`.
    fn redact(&self, v: Value) -> Value {
        self.0
            .iter()
            .fold(v, |v, (_, k)| netguard::redact(v, k.expose()))
    }

    fn redact_text(&self, t: &str) -> String {
        self.0.iter().fold(t.to_string(), |t, (_, k)| {
            netguard::redact_text(&t, k.expose())
        })
    }
}

/// One answer of strata, as read.
struct Answer {
    status: u16,
    retry_after: Option<u64>,
    body: Value,
}

/// One call to the service (no error mapping: the drainer classifies the answer). `Err` = no answer.
#[allow(clippy::too_many_arguments)]
async fn call(
    st: &AppState,
    service: Service,
    base: &str,
    auth: Auth<'_>,
    renew: Option<&crate::tenants::app_licence::Renewal>,
    renewed: &mut Option<String>,
    method: Method,
    body: Option<&Value>,
    timeout: Duration,
    op: &'static str,
) -> Result<Answer, String> {
    // a token re-minted by an earlier call of this send replaces the plane's refused one
    let current = Auth {
        token: renewed.as_deref().or(auth.token),
        ..auth
    };
    let first = call_once(
        st,
        service,
        base,
        current,
        method.clone(),
        body,
        timeout,
        op,
    )
    .await?;
    // a tenant bundle token refused by strata: drop it, mint a fresh one and resend ONCE (upstream::Call::send_any)
    if first.status == 401
        && let Some(renew) = renew
        && let Some(fresh) = renew.fresh().await
    {
        let token = renewed.insert(fresh);
        let auth = Auth {
            token: Some(token.as_str()),
            ..auth
        };
        return call_once(st, service, base, auth, method, body, timeout, op).await;
    }
    Ok(first)
}

/// One round trip of [`call`].
#[allow(clippy::too_many_arguments)]
async fn call_once(
    st: &AppState,
    service: Service,
    base: &str,
    auth: Auth<'_>,
    method: Method,
    body: Option<&Value>,
    timeout: Duration,
    op: &'static str,
) -> Result<Answer, String> {
    let url = format!("{}{PROVIDERS}", base.trim_end_matches('/'));
    let mut req = auth.apply(st.http.request(method, url)).timeout(timeout);
    if let Some(b) = body {
        req = req.json(b);
    }
    let resp = crate::observability::upstream_http(service.name(), op.to_string(), req.send())
        .await
        .map_err(|e| {
            if e.is_timeout() {
                format!(
                    "{} did not answer within {} s",
                    service.name(),
                    timeout.as_secs()
                )
            } else {
                format!("{} could not be reached", service.name())
            }
        })?;
    let status = resp.status().as_u16();
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.trim().parse().ok());
    let bytes = resp
        .bytes()
        .await
        .map_err(|_| format!("{} answered an unreadable body", service.name()))?;
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Ok(Answer {
        status,
        retry_after,
        body,
    })
}

/// strata's error code of an answer.
fn code_of(body: &Value) -> Option<String> {
    body.pointer("/error/code")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// An answer for the drainer to classify, every key redacted from it.
fn http(a: Answer, keys: &Keys) -> SendResult {
    let code = if (200..300).contains(&a.status) {
        None
    } else {
        code_of(&a.body)
    };
    SendResult::Http {
        status: a.status,
        code,
        retry_after: a.retry_after,
        body: keys.redact(a.body),
    }
}

/// strata refused the registration (422: its live probe or its validation): the row fails `slot_test_failed`, with
/// strata's message and code (keys redacted).
fn refused(a: Answer, keys: &Keys) -> SendResult {
    let msg = a
        .body
        .pointer("/error/message")
        .or_else(|| a.body.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("the provider registration was refused");
    let code = code_of(&a.body).unwrap_or_else(|| "http_422".to_string());
    SendResult::Http {
        status: a.status,
        code: Some("slot_test_failed".to_string()),
        retry_after: None,
        body: json!({"error": {
            "code": "slot_test_failed",
            "message": format!("{} ({code})", keys.redact_text(msg)),
        }}),
    }
}

/// The registration of a published slot: the PUT body entry, with `api_key` when the slot names a key.
fn entry_input(slot: Slot, e: &PushSlot, key: Option<&SecretText>, residency: &str) -> Value {
    let kind = e.kind.clone().unwrap_or_default();
    let dialect = slot
        .dialect(&kind)
        .map_or_else(|| kind.clone(), str::to_string);
    let mut m = Map::new();
    m.insert(
        "base_url".into(),
        json!(e.base_url.clone().unwrap_or_default()),
    );
    m.insert("dialect".into(), json!(dialect));
    m.insert("model".into(), json!(e.model.clone().unwrap_or_default()));
    m.insert("regions".into(), json!([residency]));
    m.insert("zdr".into(), json!(true));
    if let Some(d) = slot.dims() {
        m.insert("dims".into(), json!(d));
    }
    if let Some(k) = key {
        m.insert("api_key".into(), json!(k.expose()));
    }
    Value::Object(m)
}

/// Another capability of the set, kept as strata reports it (a registration of `source` only — `tenant` in the
/// tenant set, `store` in a store's; no `api_key`: strata keeps its stored key for the same `base_url`). `null` = not
/// such a registration (left out = removed / inherited / default).
fn kept_input(service: Service, cap: Option<&Value>, source: &str) -> Value {
    let Some(c) = cap.filter(|c| c.get("source").and_then(Value::as_str) == Some(source)) else {
        return Value::Null;
    };
    let mut fields = vec![
        "base_url",
        "dialect",
        "model",
        "model_id",
        "regions",
        "zdr",
        "timeout_ms",
        "concurrency",
    ];
    if service == Service::Rag {
        // strata_rag `ProviderInput` (deny_unknown_fields): `dims`, and chat's `thinking` / `temperature` (review M2:
        // a push must not reset what another app set)
        fields.extend(["dims", "thinking", "temperature"]);
    }
    let mut m = Map::new();
    for f in fields {
        if let Some(v) = c.get(f).filter(|v| !v.is_null()) {
            m.insert(f.to_string(), v.clone());
        }
    }
    Value::Object(m)
}

/// The PUT body: every capability of `service`, the published slots replaced, the others kept.
fn put_body(
    service: Service,
    current: &Value,
    entries: &[PushSlot],
    keys: &Keys,
    residency: &str,
) -> Value {
    let mut out = Map::new();
    for slot in Slot::of(service) {
        let name = slot.as_str();
        let v = match entries.iter().find(|e| e.slot == name) {
            Some(e) if e.configured => entry_input(slot, e, keys.of(name), residency),
            Some(_) => Value::Null,
            None => kept_input(service, current.get(name), "tenant"),
        };
        out.insert(name.to_string(), v);
    }
    Value::Object(out)
}

async fn send(st: &AppState, row: &OutboxRow) -> SendResult {
    let Some(service) = service_of(&row.target) else {
        return SendResult::Fail("unknown_target");
    };
    let Ok(payload) = serde_json::from_value::<PushPayload>(row.payload.clone()) else {
        return SendResult::Fail("invalid_payload");
    };
    let plane = match strata::plane(st, row.tenant_id, service).await {
        Ok(p) => p,
        Err(e) if e.code() == "provision_pending" => return SendResult::Wait("provision_pending"),
        Err(_) => return SendResult::Fail("strata_unconfigured"),
    };
    let base = match service {
        Service::Rag => st.cfg.strata.rag_url.as_deref(),
        Service::Ingest => st.cfg.strata.ingest_url.as_deref(),
    }
    .unwrap_or_default();
    let mut tx = match begin_scoped(&st.db, row.tenant_id).await {
        Ok(t) => t,
        Err(_) => return SendResult::Transport("the database is unavailable".into()),
    };
    let r = locked(st, &mut tx, row, &plane, base, &payload).await;
    // releases the advisory lock; nothing was written in this transaction
    if tx.commit().await.is_err() {
        tracing::warn!(row = %row.id, "connections_put lock transaction did not commit");
    }
    r
}

/// The read-modify-write of strata's set, under the advisory lock of the tenant's service.
async fn locked(
    st: &AppState,
    c: &mut PgConnection,
    row: &OutboxRow,
    plane: &Plane<'_>,
    base: &str,
    payload: &PushPayload,
) -> SendResult {
    let tenant = row.tenant_id;
    let service = plane.service();
    let auth = plane.auth();
    let mut renewed = None;
    let lock = sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(slots::push_lock(tenant, service.target()))
        .execute(&mut *c)
        .await;
    if lock.is_err() {
        return SendResult::Transport("the database is unavailable".into());
    }
    let residency: Option<Option<String>> = match sqlx::query_scalar::<_, Option<String>>(
        "SELECT residency FROM tenants WHERE id = $1",
    )
    .bind(tenant)
    .fetch_optional(&mut *c)
    .await
    {
        Ok(r) => r,
        Err(_) => return SendResult::Transport("the database is unavailable".into()),
    };
    let residency = residency.flatten().unwrap_or_else(|| "eu".to_string());
    let mut keys = Keys(Vec::new());
    for e in payload.slots.iter().filter(|e| e.configured) {
        let Some(id) = e.key_ref else {
            continue;
        };
        let sealed: Option<String> = match sqlx::query_scalar::<_, String>(
            "SELECT key_sealed FROM provider_slot_secret WHERE tenant_id = $1 AND id = $2",
        )
        .bind(tenant)
        .bind(id)
        .fetch_optional(&mut *c)
        .await
        {
            Ok(s) => s,
            Err(_) => return SendResult::Transport("the database is unavailable".into()),
        };
        let Some(sealed) = sealed else {
            return SendResult::Fail("secret_missing");
        };
        match secret::open(
            st.provider_keyring.as_deref(),
            tenant,
            SecretKind::SlotKey,
            id,
            &sealed,
        ) {
            Ok(k) => keys.0.push((e.slot.clone(), k)),
            Err(err) if err.code() == "provider_secret_key_unconfigured" => {
                return SendResult::Fail("provider_secret_key_unconfigured");
            }
            Err(_) => return SendResult::Fail("secret_unreadable"),
        }
    }
    let current = match call(
        st,
        service,
        base,
        auth,
        plane.renewal(),
        &mut renewed,
        Method::GET,
        None,
        DEFAULT_TIMEOUT,
        "providers_get",
    )
    .await
    {
        Err(m) => return SendResult::Transport(m),
        Ok(a) if !(200..300).contains(&a.status) => return http(a, &keys),
        Ok(a) => a.body,
    };
    // the T4a ruling: existing roots keep their embed model before the tenant row changes it; roots on the
    // deployment default that a job still reads withhold the embed change (module doc)
    let mut withheld: Option<(&PushSlot, Vec<String>)> = None;
    if service == Service::Rag
        && let Some(e) = payload
            .slots
            .iter()
            .find(|e| e.slot == Slot::Embed.as_str())
    {
        match pin_roots(st, c, plane, &current, e, &mut keys).await {
            Err(r) => return r,
            Ok(blocking) if !blocking.is_empty() => {
                tracing::warn!(
                    tenant = %tenant,
                    roots = ?blocking,
                    "embed change withheld: knowledge on the deployment default is still in use"
                );
                withheld = Some((e, blocking));
            }
            Ok(_) => {}
        }
    }
    let sent: Vec<PushSlot> = payload
        .slots
        .iter()
        .filter(|e| withheld.as_ref().is_none_or(|(w, _)| w.slot != e.slot))
        .cloned()
        .collect();
    let body = put_body(service, &current, &sent, &keys, &residency);
    let answer = match call(
        st,
        service,
        base,
        auth,
        plane.renewal(),
        &mut renewed,
        Method::PUT,
        Some(&body),
        LONG_TIMEOUT,
        "providers_put",
    )
    .await
    {
        Err(m) => return SendResult::Transport(keys.redact_text(&m)),
        Ok(a) if a.status == 422 => return refused(a, &keys),
        Ok(a) => a,
    };
    let mut r = http(answer, &keys);
    if let (Some((w, roots)), SendResult::Http { status, body, .. }) = (withheld, &mut r)
        && (200..300).contains(&*status)
    {
        let entry = serde_json::to_value(w).unwrap_or(Value::Null);
        if !body.is_object() {
            *body = Value::Object(Map::new());
        }
        if let Value::Object(m) = body {
            m.insert(WITHHELD.into(), json!([entry]));
            m.insert(WITHHELD_ROOTS.into(), json!(roots));
        }
    }
    r
}

/// The embed model the tenant row runs once `e` is pushed: its model, or the deployment default's when the push
/// removes the registration (F-08 reset).
fn pushed_embed_id(e: &PushSlot) -> Option<String> {
    if e.configured {
        e.model.as_deref().map(Slot::embed_model_id)
    } else {
        Slot::Embed.deployment_default().map(Slot::embed_model_id)
    }
}

/// A root store that could not keep its embed model: the row fails, nothing of the tenant row changed.
fn pin_failed(store: &str, why: &str) -> SendResult {
    SendResult::Http {
        status: 409,
        code: Some(PIN_FAILED.to_string()),
        retry_after: None,
        body: json!({"error": {
            "code": PIN_FAILED,
            "message": format!(
                "The embedding model was not changed: knowledge store {store} could not keep the model its vectors \
                 were made with ({why}). Nothing was pushed."
            ),
        }}),
    }
}

/// A strata refusal while pinning: a 5xx / unreachable strata is retried, anything else fails the row.
fn pin_error(store: &str, e: &ApiError) -> SendResult {
    let (status, code) = e.status_and_code();
    if status.is_server_error() {
        return SendResult::Transport(format!(
            "strata_rag could not pin knowledge store {store} ({code})"
        ));
    }
    pin_failed(store, code)
}

/// The key of the tenant embed registration strata holds now (`reg`): the one agent last pushed, when what strata
/// reports is still that push (`pushed_fpr`). `None` = the registration has no key.
async fn held_key(
    st: &AppState,
    c: &mut PgConnection,
    tenant: uuid::Uuid,
    reg: &Value,
    store: &str,
) -> Result<Option<SecretText>, SendResult> {
    if reg.get("has_api_key").and_then(Value::as_bool) != Some(true) {
        return Ok(None);
    }
    let db = |_| SendResult::Transport("the database is unavailable".into());
    let pushed: Option<(String, Option<uuid::Uuid>)> = sqlx::query_as(
        "SELECT pushed_fpr, pushed_key_ref FROM provider_slot_remote WHERE tenant_id = $1 AND slot = 'embed'",
    )
    .bind(tenant)
    .fetch_optional(&mut *c)
    .await
    .map_err(db)?;
    let Some(id) = pushed
        .filter(|(fpr, _)| *fpr == slots::remote_fpr(Some(reg)))
        .and_then(|(_, id)| id)
    else {
        return Err(pin_failed(
            store,
            "strata holds a key agent did not push; save the embedding slot with its key first",
        ));
    };
    let sealed: Option<String> = sqlx::query_scalar(
        "SELECT key_sealed FROM provider_slot_secret WHERE tenant_id = $1 AND id = $2",
    )
    .bind(tenant)
    .bind(id)
    .fetch_optional(&mut *c)
    .await
    .map_err(db)?;
    let Some(sealed) = sealed else {
        return Err(SendResult::Fail("secret_missing"));
    };
    match secret::open(
        st.provider_keyring.as_deref(),
        tenant,
        SecretKind::SlotKey,
        id,
        &sealed,
    ) {
        Ok(k) => Ok(Some(k)),
        Err(err) if err.code() == "provider_secret_key_unconfigured" => {
            Err(SendResult::Fail("provider_secret_key_unconfigured"))
        }
        Err(_) => Err(SendResult::Fail("secret_unreadable")),
    }
}

/// The store PUT that pins a root to the tenant embed registration `reg` (with `key`), keeping the root's own
/// rerank / chat bindings as strata reports them (`view` = its `GET /api/stores/{id}/providers`).
fn pin_body(view: &Value, reg: &Value, key: Option<&SecretText>) -> Value {
    let mut out = Map::new();
    let mut embed = kept_input(Service::Rag, Some(reg), "tenant");
    if let (Some(k), Value::Object(m)) = (key, &mut embed) {
        m.insert("api_key".into(), json!(k.expose()));
    }
    out.insert("embed".into(), embed);
    for cap in ["rerank", "chat"] {
        let own = kept_input(Service::Rag, view.get(cap), "store");
        if !own.is_null() {
            out.insert(cap.into(), own);
        }
    }
    Value::Object(out)
}

/// (strata root ids a job reads, every strata root agent made).
type RootSets = (HashSet<uuid::Uuid>, HashSet<uuid::Uuid>);

/// The strata root ids a job still reads (draft or live pin; no pin = the store's `initial` generation, no generation
/// = the store's own binding), and every strata root agent made (`known`).
async fn roots_in_use(c: &mut PgConnection) -> ApiResult<RootSets> {
    let roots: Vec<stores::StoreRow> = stores::all(&mut *c)
        .await?
        .into_iter()
        .filter(|s| s.parent_id.is_none())
        .collect();
    let mut known: HashSet<uuid::Uuid> = sqlx::query_scalar::<_, uuid::Uuid>(
        "SELECT strata_store_id FROM store_generation WHERE strata_store_id IS NOT NULL",
    )
    .fetch_all(&mut *c)
    .await?
    .into_iter()
    .collect();
    known.extend(roots.iter().filter_map(|r| r.strata_store_id));
    let jobs: Vec<(uuid::Uuid, String)> = sqlx::query_as("SELECT id, key FROM jobs ORDER BY id")
        .fetch_all(&mut *c)
        .await?;
    let mut used = HashSet::new();
    for (id, key) in jobs {
        let jr = JobRef { id, key };
        for root in &roots {
            for of in [DesignOf::Draft, DesignOf::Live] {
                let sid = match generations::pinned(&mut *c, &jr, root, &of).await? {
                    Some(g) => g.root_sid(root).map(|s| s.0),
                    None => root.strata_store_id,
                };
                used.extend(sid);
            }
        }
    }
    Ok((used, known))
}

/// The T4a ruling (module doc): bind every existing strata root still following the tenant's embed registration
/// to it at store level before `e` replaces it. Returns the names of the roots that follow the DEPLOYMENT DEFAULT,
/// hold content and are still read (ruling 2026-10-21, revised): non-empty = the embed change is withheld. Runs under
/// the push's advisory lock.
async fn pin_roots(
    st: &AppState,
    c: &mut PgConnection,
    plane: &Plane<'_>,
    current: &Value,
    e: &PushSlot,
    keys: &mut Keys,
) -> Result<Vec<String>, SendResult> {
    let cur = current.get("embed");
    let cur_id = slots::current_embed_id(cur);
    let Some(new_id) = pushed_embed_id(e) else {
        return Ok(Vec::new());
    };
    if cur_id.as_deref() == Some(new_id.as_str()) {
        // the vectors' model does not move
        return Ok(Vec::new());
    }
    let reg = cur.filter(|c| c.get("source").and_then(Value::as_str) == Some("tenant"));
    let on_default = reg.is_none() && slots::follows_default(cur);
    let roots = rag::stores(&st.http, plane)
        .await
        .map_err(|err| pin_error("listing", &err))?;
    let mut pinned = 0;
    let mut in_use: Option<RootSets> = None;
    let mut blocking = Vec::new();
    for root in roots.iter().filter(|s| s.parent_id.is_none()) {
        let view = rag::store_providers_get(&st.http, plane, root.id)
            .await
            .map_err(|err| pin_error(&root.name, &err))?;
        if view.pointer("/embed/source").and_then(Value::as_str) == Some("store") {
            continue;
        }
        let tag = view
            .get("embed_model")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if tag == new_id {
            continue;
        }
        match reg {
            Some(reg) if cur_id.as_deref() == Some(tag) => {
                let key = held_key(st, &mut *c, plane.tenant(), reg, &root.name).await?;
                let body = pin_body(&view, reg, key.as_ref());
                if let Some(k) = key {
                    keys.0.push((PIN_KEY.to_string(), k));
                }
                rag::store_providers_put(&st.http, plane, root.id, body)
                    .await
                    .map_err(|err| pin_error(&root.name, &err))?;
                pinned += 1;
            }
            // nothing embedded yet: no vectors to keep
            _ if root.chunk_count == 0 && root.children.is_empty() => {}
            // strata cannot pin its deployment default per store: withhold while a job still reads the root
            _ if on_default => {
                if in_use.is_none() {
                    in_use = Some(roots_in_use(&mut *c).await.map_err(|_| {
                        SendResult::Transport("the database is unavailable".into())
                    })?);
                }
                let sid = root.id.0;
                if let Some((used, known)) = &in_use
                    && (used.contains(&sid) || !known.contains(&sid))
                {
                    blocking.push(root.name.clone());
                }
            }
            _ => {
                return Err(pin_failed(
                    &root.name,
                    "it holds content of a model the tenant row no longer names; re-embed it first",
                ));
            }
        }
    }
    if pinned > 0 {
        tracing::info!(tenant = %plane.tenant(), pinned, "embed change: existing roots pinned to their model");
    }
    Ok(blocking)
}

/// `on_success`: what strata now holds for each pushed slot (the PUT's answer) is the drift baseline; a withheld
/// entry was not pushed and is not recorded.
async fn record(c: &mut PgConnection, row: &OutboxRow, result: &Value) -> ApiResult<()> {
    let Ok(payload) = serde_json::from_value::<PushPayload>(row.payload.clone()) else {
        return Ok(());
    };
    let withheld: Vec<&str> = result
        .get(WITHHELD)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|w| w.get("slot").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    for e in payload
        .slots
        .iter()
        .filter(|e| !withheld.contains(&e.slot.as_str()))
    {
        slots::record_pushed(&mut *c, row.tenant_id, e, result.get(&e.slot)).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(slot: &str, kind: &str, key_ref: bool) -> PushSlot {
        PushSlot {
            slot: slot.into(),
            configured: true,
            kind: Some(kind.into()),
            base_url: Some("https://p.example/v1".into()),
            model: Some("m-1".into()),
            key_ref: key_ref.then(uuid::Uuid::new_v4),
            version: 2,
            release: Some("cfg-2026-10-21.1".into()),
        }
    }

    use crate::providers::slots::{current_embed_id, embed_id, follows_default};

    #[test]
    fn embed_ids_follow_strata() {
        let reg = json!({"source": "tenant", "base_url": "https://e.example/v1", "dialect": "openai",
                         "model": "m-1", "model_id": null, "dims": 1024, "has_api_key": true});
        assert_eq!(embed_id(Some(&reg)).as_deref(), Some("m-1@1024"));
        let with_id = json!({"source": "store", "model": "m-1", "model_id": "acme/m-1-v2"});
        assert_eq!(embed_id(Some(&with_id)).as_deref(), Some("acme/m-1-v2"));
        assert_eq!(embed_id(Some(&json!({"source": "unconfigured"}))), None);
        assert_eq!(embed_id(None), None);
        // a default that names no model is the deployment default's
        let def = json!({"source": "default"});
        assert_eq!(
            current_embed_id(Some(&def)).as_deref(),
            Some("BAAI/bge-m3@1024")
        );
        assert!(follows_default(Some(&def)));
        assert!(!follows_default(Some(&reg)));
        assert_eq!(
            current_embed_id(Some(&json!({"source": "unconfigured"}))),
            None
        );
        let mut e = entry("embed", "openai_compatible", false);
        assert_eq!(pushed_embed_id(&e).as_deref(), Some("m-1@1024"));
        e.configured = false;
        assert_eq!(
            pushed_embed_id(&e).as_deref(),
            Some("BAAI/bge-m3@1024"),
            "a removal falls back to the deployment default"
        );
    }

    #[test]
    fn pin_body_binds_the_registration_and_keeps_own_bindings() {
        let reg = json!({"source": "tenant", "base_url": "https://e.example/v1", "dialect": "openai",
                         "model": "m-1", "model_id": null, "dims": 1024, "regions": ["eu"], "zdr": true,
                         "timeout_ms": 30000, "concurrency": 8, "has_api_key": true, "updated_at": "t"});
        let view = json!({
            "store_id": "s", "embed_model": "m-1@1024", "embed": reg.clone(),
            "rerank": {"source": "store", "base_url": "https://r.example/v2", "dialect": "cohere", "model": "r",
                       "regions": ["eu"], "zdr": true, "has_api_key": true},
            "chat": {"source": "tenant", "base_url": "https://c.example/v1", "dialect": "openai", "model": "c"},
        });
        // (the key case is covered by tests/b3/provider_slots.rs: SecretText is only made by opening a sealed key)
        let body = pin_body(&view, &reg, None);
        assert_eq!(
            body["embed"],
            json!({"base_url": "https://e.example/v1", "dialect": "openai", "model": "m-1", "dims": 1024,
                   "regions": ["eu"], "zdr": true, "timeout_ms": 30000, "concurrency": 8})
        );
        assert_eq!(
            body["rerank"]["model"], "r",
            "the store's own binding is kept"
        );
        assert!(body["rerank"].get("api_key").is_none());
        assert!(
            body.get("chat").is_none(),
            "an inherited capability stays inherited"
        );
    }

    #[test]
    fn kept_chat_keeps_thinking_and_temperature() {
        let chat = json!({"source": "tenant", "base_url": "https://c.example/v1", "dialect": "openai",
                          "model": "c", "regions": ["eu"], "zdr": true, "thinking": false, "temperature": 0.2,
                          "has_api_key": true, "updated_at": "t"});
        let kept = kept_input(Service::Rag, Some(&chat), "tenant");
        assert_eq!(
            kept["thinking"], false,
            "review M2: another app's setting survives a push"
        );
        assert_eq!(kept["temperature"], 0.2);
        assert!(kept.get("has_api_key").is_none() && kept.get("updated_at").is_none());
        let ocr = json!({"source": "tenant", "base_url": "https://o.example", "dialect": "mistral", "model": "o",
                         "regions": ["eu"], "zdr": true, "temperature": 0.1});
        assert!(
            kept_input(Service::Ingest, Some(&ocr), "tenant")
                .get("temperature")
                .is_none(),
            "strata_ingest takes no temperature"
        );
    }

    #[test]
    fn put_body_replaces_one_and_keeps_the_others_without_keys() {
        let current = json!({
            "embed": {"source": "tenant", "base_url": "https://old.example/v1", "dialect": "openai",
                      "model": "old", "model_id": null, "dims": 1024, "regions": ["eu"], "zdr": true,
                      "timeout_ms": 30000, "concurrency": 8, "has_api_key": true, "updated_at": "t"},
            "rerank": {"source": "tenant", "base_url": "https://r.example/v2", "dialect": "cohere",
                       "model": "r", "dims": null, "regions": ["eu"], "zdr": true, "has_api_key": true},
            "chat": {"source": "default", "dialect": "openai", "model": "local-llm"},
        });
        let keys = Keys(Vec::new());
        let body = put_body(
            Service::Rag,
            &current,
            &[entry("embed", "voyage", false)],
            &keys,
            "eu",
        );
        assert_eq!(
            body["embed"],
            json!({"base_url": "https://p.example/v1", "dialect": "openai", "model": "m-1",
                   "regions": ["eu"], "zdr": true, "dims": 1024})
        );
        assert_eq!(
            body["rerank"],
            json!({"base_url": "https://r.example/v2", "dialect": "cohere", "model": "r",
                   "regions": ["eu"], "zdr": true})
        );
        assert_eq!(body["chat"], Value::Null, "a default is not re-registered");
        assert!(!body.to_string().contains("api_key"));
        let ing = put_body(
            Service::Ingest,
            &json!({}),
            &[entry("ocr", "mistral_ocr", false)],
            &keys,
            "us",
        );
        assert_eq!(ing["ocr"]["dialect"], "mistral");
        assert_eq!(ing["ocr"]["regions"], json!(["us"]));
        assert!(ing["ocr"].get("dims").is_none(), "ingest takes no dims");
        assert_eq!(ing["vision"], Value::Null);
    }
}
