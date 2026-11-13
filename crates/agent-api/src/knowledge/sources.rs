//! A job's knowledge sources (B3 T7; plan §5.4, §4 0053): types, validation, the write-only credentials and the row
//! loaders. Ported from care-bus-agents@96c608c `crates/carebus-api/src/sources.rs` with the tenant + job scope, the
//! D68 `language`, the UI's refusal codes (`web/src/lib/api/areas/tenant.ts` `validateSource`) and §0 R1/R2:
//!
//! - Every source URL passes `providers::check_url` (crate::netguard: https only; no credentials, query or fragment;
//!   no private, loopback, CGNAT, cluster or dotless host) → 422 `invalid_source` / `address_refused`.
//! - Credentials arrive only under the top-level `credentials` key (absent = keep, `null` = clear, `{secret}` =
//!   replace). [`parse`] splits that key off BEFORE `audit::scrub::check` walks the rest of the body, so a secret
//!   anywhere else is 422 `secret_rejected`. They are sealed with the ProviderSecret keyring (AAD
//!   `<tenant>:source_cred:<source id>`) and never selected into a response; a response says `has_credentials`
//!   and `credentials_updated_at` only.
//! - The language (D68) is one of the job's DRAFT languages (B2 kind `languages`, §0 R6) → else 422
//!   `language_not_served`; absent = the job's first language.
//! - The mapping of an api / datatable source names attributes of the job's DRAFT ontology ([`mapping_targets`],
//!   B5 J11) → else 422 `unknown_attribute`.
//! - `schedule` is a five-field cron, validated (422 `invalid_schedule`), stored and shown; nothing runs it (D82).
//! - `supersedes` (PK-8, T6) lists the platform topic keys the source's documents replace: at most 32 keys of 1-128
//!   characters of `[a-z0-9_./-]` (strata's limits, crate::knowledge::topic_keys), stored sorted and de-duplicated
//!   ([`normalize_supersedes`]) → else 422 `invalid_source`. Live on save (sources are the data plane, D69); absent
//!   or `null` on a save keeps the stored list, `[]` clears it. Every run sends it to strata_ingest (#15:
//!   [`forward_supersedes`] top-level in a JSON body, [`supersedes_field`] before the files of a multipart one), and
//!   the leaf-set search also sends the union as the request's `supersedes` (documents ingested before #15).
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sqlx::PgConnection;
use sqlx::types::Json;
use uuid::Uuid;

use crate::audit::scrub;
use crate::crypto::keyring::Keyring;
use crate::design::{self, DesignOf, DraftRef, JobRef, Registry};
use crate::error::{ApiError, ApiResult};
use crate::job_design::base;
use crate::job_design::ontology;
use crate::job_design::types::OntologyEntity;
use crate::knowledge::topic_keys;
use crate::netguard::Policy;
use crate::providers::secret::{self, Sealed, SecretKind, SecretText};
use crate::providers::{self, UrlField};
use crate::release::compile::TemplateCompiler;
use crate::tenancy::JobCtx;

/// The language a source falls back to when the job serves none yet (fixture `firstLanguage`).
pub const DEFAULT_LANGUAGE: &str = "en";
/// Longest source name (characters).
pub const MAX_NAME_CHARS: usize = 120;
/// Longest secret (bytes).
pub const MAX_SECRET_BYTES: usize = 4096;

/// Most topic keys one source or upload supersedes (strata's limit, crate::knowledge::topic_keys).
pub const MAX_SUPERSEDES: usize = topic_keys::MAX_KEYS;
/// Longest superseded topic key (characters).
pub const MAX_SUPERSEDES_CHARS: usize = topic_keys::MAX_KEY_CHARS;

/// A `supersedes` value checked and normalised: a JSON array of strings, then crate::knowledge::topic_keys
/// (strata's limits: at most [`MAX_SUPERSEDES`] keys of 1-[`MAX_SUPERSEDES_CHARS`] characters of `[a-z0-9_./-]`,
/// trimmed), returned sorted and de-duplicated. `Err` is the message naming the broken rule (the caller picks the
/// code: `invalid_source` / `invalid_supersedes`).
pub fn normalize_supersedes(v: &Value) -> Result<Vec<String>, String> {
    let list = v
        .as_array()
        .ok_or_else(|| "supersedes must be a list of topic keys.".to_string())?;
    let keys = list
        .iter()
        .map(|k| {
            k.as_str()
                .ok_or_else(|| "Every supersedes entry must be a topic key string.".to_string())
        })
        .collect::<Result<Vec<&str>, String>>()?;
    topic_keys::normalize("supersedes", keys)
}

/// strata_ingest #15: a JSON job body (a URL job or a source run) carries `supersedes` at the top level, applied to
/// every document the job writes. Always sent, `[]` included: an omitted field on a re-ingest keeps the value strata
/// stored, so a cleared list must travel as `[]`.
pub fn forward_supersedes(body: &mut Value, supersedes: &[String]) {
    if let Some(o) = body.as_object_mut() {
        o.insert("supersedes".into(), json!(supersedes));
    }
}

/// strata_ingest #15: a multipart job carries `supersedes` as a JSON text field, which must come BEFORE the first
/// `file` part (strata_ingest refuses it after). Call this before adding any file.
pub fn supersedes_field(
    form: reqwest::multipart::Form,
    supersedes: &[String],
) -> reqwest::multipart::Form {
    form.text("supersedes", json!(supersedes).to_string())
}

/// 422 `invalid_source` (the UI's code for every shape problem of a source).
pub fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError::Unprocessable("invalid_source", msg.into())
}

/// 404 `source_not_found`.
pub fn source_not_found() -> ApiError {
    ApiError::Missing(
        "source_not_found",
        "No source with that id in this job.".into(),
    )
}

/// 400 `invalid_body`.
pub fn bad_body(msg: impl Into<String>) -> ApiError {
    ApiError::BadRequest("invalid_body", msg.into())
}

/// 409 `run_in_progress`.
pub fn run_in_progress() -> ApiError {
    ApiError::Conflict(
        "run_in_progress",
        "A re-sync of this source is still running. Wait for it to finish.".into(),
    )
}

/// The `source_id` strata_ingest / strata_rag hold for this source's documents: the source's uuid (as care-bus; a
/// direct upload is `upload:<id>`), so a document's language (D68, G5) resolves to `source.language`.
pub fn strata_source_id(id: Uuid) -> String {
    id.to_string()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceType {
    Website,
    Api,
    File,
    Datatable,
}

impl SourceType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Website => "website",
            Self::Api => "api",
            Self::File => "file",
            Self::Datatable => "datatable",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "website" => Some(Self::Website),
            "api" => Some(Self::Api),
            "file" => Some(Self::File),
            "datatable" => Some(Self::Datatable),
            _ => None,
        }
    }

    /// api and datatable go through strata_ingest's structured contract: only they carry a mapping and credentials.
    pub fn structured(self) -> bool {
        matches!(self, Self::Api | Self::Datatable)
    }
}

/// One source field mapped to an `Entity.attribute` of the job's ontology.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MappingRow {
    pub source_field: String,
    pub entity: String,
    pub attribute: String,
}

/// One `Entity.attribute` a source field may map to (UI `MappingTarget`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MappingTarget {
    pub entity: String,
    pub attribute: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub pii: bool,
}

/// `sourceMappingTargets`: every attribute of the job's DRAFT ontology (B5 J11: the bases at the draft's pins with
/// the tenant's rows folded on top, exactly what `ontologyEntities` shows the designer). The draft, not the live
/// release: a source is data plane (D69, live on save) configured beside the ontology it maps into, the same way its
/// language is checked against the draft `languages` (§0 R6); an attribute added in J11 is mappable before the next
/// release. Hidden entities and `parent_missing` attributes are not targets. Runs in the caller's job-scoped
/// transaction.
pub async fn mapping_targets(
    c: &mut PgConnection,
    reg: &Registry,
    compiler: &dyn TemplateCompiler,
    ctx: &JobCtx,
) -> ApiResult<Vec<MappingTarget>> {
    let job = JobRef {
        id: ctx.job_id,
        key: ctx.job_key.clone(),
    };
    design::ensure_job(&mut *c, reg, &job).await?;
    let bases = base::at(&mut *c, reg, compiler, &job, &DesignOf::Draft).await?;
    let rows = ontology::draft_rows(&mut *c, ctx.job_id).await?;
    Ok(targets_of(&ontology::entities(
        &bases,
        &rows,
        ctx.t.tenant,
        &ctx.job_key,
    )))
}

/// The mapping targets of folded ontology entities: one per attribute of a visible entity, `parent_missing`
/// attributes left out, in entity then attribute order.
pub fn targets_of(entities: &[OntologyEntity]) -> Vec<MappingTarget> {
    entities
        .iter()
        .filter(|e| !e.hidden)
        .flat_map(|e| {
            e.attributes
                .iter()
                .filter(|a| a.conflict.is_none())
                .map(move |a| MappingTarget {
                    entity: e.name.clone(),
                    attribute: a.name.clone(),
                    kind: a.data_type.clone(),
                    pii: a.pii,
                })
        })
        .collect()
}

/// The secret of an api or datatable source. Read from requests only, sealed or sent to the source host; no
/// response type contains it, and `Debug` is redacted.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub secret: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials([redacted])")
    }
}

/// `credentials` absent (or `{secret: ""}`) = Keep, `null` = Clear, `{"secret": "…"}` = Set.
#[derive(Debug)]
pub enum CredentialsChange {
    Keep,
    Clear,
    Set(Credentials),
}

/// The top-level `credentials` key of a body.
pub fn credentials_change(v: Option<Value>) -> ApiResult<CredentialsChange> {
    let v = match v {
        None => return Ok(CredentialsChange::Keep),
        Some(Value::Null) => return Ok(CredentialsChange::Clear),
        Some(v) => v,
    };
    let c: Credentials = serde_json::from_value(v)
        .map_err(|_| invalid("credentials must be {\"secret\": \"…\"} or null."))?;
    if c.secret.is_empty() {
        // the UI sends an empty key field as "leave the stored key as it is"
        return Ok(CredentialsChange::Keep);
    }
    if c.secret.trim().is_empty()
        || c.secret.len() > MAX_SECRET_BYTES
        || c.secret.chars().any(char::is_control)
    {
        return Err(invalid(
            "The key must be 1-4096 characters without line breaks or control characters.",
        ));
    }
    Ok(CredentialsChange::Set(c))
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

fn yes() -> bool {
    true
}

/// A `createSource` / `saveSource` body without its `credentials` (UI `SourceInput`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceInput {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: SourceType,
    #[serde(default = "empty_object")]
    pub config: Value,
    #[serde(default)]
    pub mapping: Vec<MappingRow>,
    pub target_store_id: Uuid,
    #[serde(default)]
    pub schedule: Option<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub language: Option<String>,
    /// The platform topic keys replaced (PK-8): absent / `null` = keep (`[]` on create), else a list checked by
    /// [`normalize_supersedes`].
    #[serde(default)]
    pub supersedes: Option<Value>,
}

/// A parsed body.
#[derive(Debug)]
pub struct Parsed {
    pub input: SourceInput,
    pub credentials: CredentialsChange,
}

/// 422 `secret_rejected` naming the field, never its value.
fn secret_rejected(field: &str) -> ApiError {
    ApiError::Unprocessable(
        "secret_rejected",
        format!("{field} looks like a secret or an email address; a key goes in credentials."),
    )
}

/// Splits off `credentials`, refuses a secret anywhere else in the body, then reads the rest as a [`SourceInput`].
pub fn parse(mut raw: Value) -> ApiResult<Parsed> {
    let Some(obj) = raw.as_object_mut() else {
        return Err(bad_body("The request body must be a JSON object."));
    };
    let credentials = credentials_change(obj.remove("credentials"))?;
    scrub::check(&raw).map_err(|f| secret_rejected(&f))?;
    let input: SourceInput = serde_json::from_value(raw)
        .map_err(|e| bad_body(format!("The source is not valid: {e}")))?;
    Ok(Parsed { input, credentials })
}

/// What [`validate`] settled: the values stored.
#[derive(Debug)]
pub struct Checked {
    pub name: String,
    pub language: String,
    pub schedule: Option<String>,
    pub mapping: Vec<MappingRow>,
    /// `None` = keep the stored list (create stores `[]`).
    pub supersedes: Option<Vec<String>>,
}

/// Five whitespace-separated cron fields of digits and `* / , -`. Stored and shown; nothing runs it (D82).
pub fn valid_cron(c: &str) -> bool {
    let fields: Vec<&str> = c.split_whitespace().collect();
    c.len() <= 100
        && fields.len() == 5
        && fields.iter().all(|f| {
            f.bytes()
                .all(|b| b.is_ascii_digit() || b"*/,-".contains(&b))
        })
}

/// A BCP-47-ish language tag (`nl`, `en-GB`), the shape of `source.language`.
pub fn valid_language(l: &str) -> bool {
    let mut parts = l.split('-');
    let head = parts.next().unwrap_or_default();
    (2..=3).contains(&head.len())
        && head.bytes().all(|b| b.is_ascii_lowercase())
        && parts.all(|p| (2..=8).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// The job's draft languages (B2 kind `languages`, §0 R6), `[]` when none.
pub async fn draft_languages(
    c: &mut PgConnection,
    reg: &Registry,
    ctx: &JobCtx,
) -> ApiResult<Vec<String>> {
    let job = JobRef {
        id: ctx.job_id,
        key: ctx.job_key.clone(),
    };
    Ok(design::typed::<Vec<String>>(
        c,
        reg,
        &job,
        &DesignOf::Draft,
        "languages",
        &DraftRef::new(),
    )
    .await?
    .unwrap_or_default())
}

/// What a write needs to check a source.
pub struct Scope<'a> {
    pub reg: &'a Registry,
    /// B2's template compiler (`AppState.compiler`): the draft bases of the mapping targets.
    pub compiler: &'a dyn TemplateCompiler,
    pub policy: &'a Policy,
    pub ctx: &'a JobCtx,
    /// The source being saved (`None` on create).
    pub self_id: Option<Uuid>,
    /// The stored language of the source being saved (kept when the body names none).
    pub stored_language: Option<&'a str>,
}

/// The fixture's `validateSource`, in its order: name, duplicate name (409), target store, URLs + config, language,
/// schedule, mapping. Runs in the write's transaction (scoped to the job's tenant).
pub async fn validate(c: &mut PgConnection, sc: &Scope<'_>, i: &SourceInput) -> ApiResult<Checked> {
    let name = i.name.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(invalid("Give the source a name of at most 120 characters."));
    }
    let taken: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM source WHERE job_id = $1 AND name = $2 AND id IS DISTINCT FROM $3)",
    )
    .bind(sc.ctx.job_id)
    .bind(&name)
    .bind(sc.self_id)
    .fetch_one(&mut *c)
    .await?;
    if taken {
        return Err(source_exists(&name));
    }
    // FOR SHARE: a store delete holds the row FOR UPDATE across its strata call, so a source never binds a store
    // whose strata store is being deleted (it waits, then sees the store gone); one bound first refuses the delete
    let store_kind: Option<String> =
        sqlx::query_scalar("SELECT kind FROM knowledge_store WHERE id = $1 FOR SHARE")
            .bind(i.target_store_id)
            .fetch_optional(&mut *c)
            .await?;
    if store_kind.as_deref() != Some("leaf") {
        return Err(ApiError::Unprocessable(
            "invalid_target_store",
            "Pick a leaf knowledge store for the documents.".into(),
        ));
    }
    validate_config(i.kind, &i.config, sc.policy, sc.ctx.t.tenant)?;
    let served = draft_languages(&mut *c, sc.reg, sc.ctx).await?;
    let language = match i.language.as_deref().map(str::trim) {
        Some(l) => {
            if !valid_language(l) {
                return Err(invalid(
                    "language must be a language tag such as nl or en-GB.",
                ));
            }
            if !served.is_empty() && !served.iter().any(|s| s.as_str() == l) {
                return Err(ApiError::Unprocessable(
                    "language_not_served",
                    format!(
                        "This job serves {}; a source's documents must be in one of them (D68).",
                        served.join(", ")
                    ),
                ));
            }
            l.to_string()
        }
        None => match sc.stored_language {
            Some(l) => l.to_string(),
            None => served
                .first()
                .cloned()
                .unwrap_or_else(|| DEFAULT_LANGUAGE.to_string()),
        },
    };
    let schedule = i
        .schedule
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if let Some(cron) = &schedule
        && crate::knowledge::schedule::Cron::parse(cron).is_none()
    {
        return Err(ApiError::Unprocessable(
            "invalid_schedule",
            "A schedule has five cron fields, for example 0 3 * * *.".into(),
        ));
    }
    let mapping = if i.kind.structured() {
        let targets = mapping_targets(&mut *c, sc.reg, sc.compiler, sc.ctx).await?;
        validate_mapping(&targets, &i.mapping)?
    } else {
        Vec::new()
    };
    let supersedes = match &i.supersedes {
        None | Some(Value::Null) => None,
        Some(v) => Some(normalize_supersedes(v).map_err(invalid)?),
    };
    Ok(Checked {
        name,
        language,
        schedule,
        mapping,
        supersedes,
    })
}

/// 409 `source_exists`.
pub fn source_exists(name: &str) -> ApiError {
    ApiError::Conflict(
        "source_exists",
        format!("This job already has a source called “{}”.", short(name)),
    )
}

/// The rows kept (blank source fields dropped, as the UI skips them): no source field twice (case-insensitive) →
/// 422 `duplicate_field`; every row an attribute of `targets` → else 422 `unknown_attribute`. Duplicates are checked
/// over the whole mapping first, so the refusal names the real problem whatever the targets are.
pub fn validate_mapping(
    targets: &[MappingTarget],
    rows: &[MappingRow],
) -> ApiResult<Vec<MappingRow>> {
    let kept: Vec<MappingRow> = rows
        .iter()
        .filter(|m| !m.source_field.trim().is_empty())
        .map(|m| MappingRow {
            source_field: m.source_field.trim().to_string(),
            entity: m.entity.trim().to_string(),
            attribute: m.attribute.trim().to_string(),
        })
        .collect();
    let mut seen: Vec<String> = Vec::with_capacity(kept.len());
    for m in &kept {
        if m.source_field.chars().count() > 200 {
            return Err(invalid("A source field name has at most 200 characters."));
        }
        let f = m.source_field.to_lowercase();
        if seen.contains(&f) {
            return Err(ApiError::Unprocessable(
                "duplicate_field",
                format!(
                    "The source field “{}” is mapped twice.",
                    short(&m.source_field)
                ),
            ));
        }
        seen.push(f);
    }
    for m in &kept {
        if !targets
            .iter()
            .any(|t| t.entity == m.entity && t.attribute == m.attribute)
        {
            let or_q = |s: &str| {
                if s.is_empty() {
                    "?".to_string()
                } else {
                    short(s)
                }
            };
            return Err(ApiError::Unprocessable(
                "unknown_attribute",
                format!(
                    "{}.{} is not an attribute of this job's ontology (field “{}”).",
                    or_q(&m.entity),
                    or_q(&m.attribute),
                    short(&m.source_field)
                ),
            ));
        }
    }
    Ok(kept)
}

/// Credentials are allowed only where something sends them: an api or datatable source whose `config.auth.type`
/// is `bearer` or `header`.
pub fn check_credentials_allowed(kind: SourceType, config: &Value, has: bool) -> ApiResult<()> {
    if has && !(kind.structured() && auth_type(config) != "none") {
        return Err(invalid(
            "Only api and datatable sources with config.auth.type bearer or header use a key. Send \"credentials\": null to remove it.",
        ));
    }
    Ok(())
}

/// `config.auth.type`, `none` when absent.
pub fn auth_type(config: &Value) -> &str {
    config["auth"]["type"].as_str().unwrap_or("none")
}

fn short(k: &str) -> String {
    k.chars().take(64).collect()
}

/// `raw` through `providers::check_url` (sources: 422 `invalid_source` / `address_refused`).
pub fn check_source_url(raw: &str, policy: &Policy, tenant: Uuid) -> ApiResult<reqwest::Url> {
    providers::check_url(raw, policy, tenant, UrlField::Source)
}

/// The URL a source reads from: `config.url`, else (a website in `urls` mode) the first of `config.urls`.
pub fn source_url(config: &Value) -> Option<String> {
    let u = config["url"]
        .as_str()
        .or_else(|| config["urls"].as_array()?.first()?.as_str())?;
    let u = u.trim();
    (!u.is_empty()).then(|| u.to_string())
}

/// The config of a source of `kind`: known keys only, every URL through [`check_source_url`].
pub fn validate_config(
    kind: SourceType,
    cfg: &Value,
    policy: &Policy,
    tenant: Uuid,
) -> ApiResult<()> {
    let o = cfg
        .as_object()
        .ok_or_else(|| invalid("config must be a JSON object."))?;
    let allowed: &[&str] = match kind {
        SourceType::Website => &["mode", "urls", "url", "crawl", "sitemap", "options"],
        SourceType::Api => &["url", "auth", "options"],
        SourceType::File => &["options"],
        SourceType::Datatable => &["url", "format", "sheet", "header_row", "auth", "options"],
    };
    if let Some(k) = o.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(invalid(format!(
            "config.{} is not a {} setting.",
            short(k),
            kind.as_str()
        )));
    }
    if let Some(opts) = o.get("options") {
        validate_options(opts)?;
    }
    match kind {
        SourceType::Website => validate_website(o, policy, tenant)?,
        SourceType::Api | SourceType::Datatable => {
            let u = o
                .get("url")
                .and_then(Value::as_str)
                .filter(|u| !u.trim().is_empty())
                .ok_or_else(|| invalid("Enter the URL the source reads from."))?;
            check_source_url(u, policy, tenant)?;
            validate_auth(o.get("auth"))?;
            if kind == SourceType::Datatable {
                if let Some(f) = o.get("format")
                    && !matches!(f.as_str(), Some("csv" | "xlsx"))
                {
                    return Err(invalid("config.format must be csv or xlsx."));
                }
                if let Some(sheet) = o.get("sheet")
                    && !sheet.is_null()
                    && !sheet
                        .as_str()
                        .is_some_and(|t| !t.trim().is_empty() && t.len() <= 100)
                {
                    return Err(invalid(
                        "config.sheet must be a sheet name (1-100 characters).",
                    ));
                }
                if o.get("header_row").is_some_and(|v| !v.is_boolean()) {
                    return Err(invalid("config.header_row must be true or false."));
                }
            }
        }
        SourceType::File => {}
    }
    Ok(())
}

/// How a website source finds its pages (`config.mode`, default `urls`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebsiteMode {
    /// `config.urls`: 1-50 pages, fetched as listed.
    Urls,
    /// `config.url` is the start page; strata_ingest follows links (`config.crawl`).
    Crawl,
    /// `config.url` is a sitemap; strata_ingest reads the pages it lists (`config.sitemap`).
    Sitemap,
}

impl WebsiteMode {
    /// The mode of a website config; `None` for a value that is not one.
    pub fn of(cfg: &Value) -> Option<Self> {
        Self::parse(cfg.get("mode"))
    }

    fn parse(mode: Option<&Value>) -> Option<Self> {
        match mode {
            None => Some(Self::Urls),
            Some(m) => match m.as_str()? {
                "urls" => Some(Self::Urls),
                "crawl" => Some(Self::Crawl),
                "sitemap" => Some(Self::Sitemap),
                _ => None,
            },
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Urls => "urls",
            Self::Crawl => "crawl",
            Self::Sitemap => "sitemap",
        }
    }
}

/// `urls` mode keeps the 1-50 `urls` list (a single `url` is accepted as a one-page list, as the UI sends a start
/// URL that way). `crawl` and `sitemap` take ONE start URL under `url` plus that mode's own settings; the other
/// mode's keys are refused so a stored config never carries settings that are not sent.
fn validate_website(o: &Map<String, Value>, policy: &Policy, tenant: Uuid) -> ApiResult<()> {
    let mode = WebsiteMode::parse(o.get("mode"))
        .ok_or_else(|| invalid("config.mode must be urls, crawl or sitemap."))?;
    let own: &[&str] = match mode {
        WebsiteMode::Urls => &["urls", "url"],
        WebsiteMode::Crawl => &["url", "crawl"],
        WebsiteMode::Sitemap => &["url", "sitemap"],
    };
    if let Some(k) = ["urls", "url", "crawl", "sitemap"]
        .into_iter()
        .find(|k| o.contains_key(*k) && !own.contains(k))
    {
        return Err(invalid(format!(
            "config.{k} is not a setting of a website source in {} mode.",
            mode.as_str()
        )));
    }
    match mode {
        WebsiteMode::Urls => {
            let mut urls: Vec<&str> = Vec::new();
            if let Some(list) = o.get("urls") {
                let list = list
                    .as_array()
                    .filter(|a| (1..=50).contains(&a.len()))
                    .ok_or_else(|| invalid("config.urls must list 1 to 50 URLs."))?;
                for u in list {
                    urls.push(u.as_str().ok_or_else(|| {
                        invalid("every entry of config.urls must be a URL string.")
                    })?);
                }
            }
            if let Some(u) = o.get("url") {
                urls.push(
                    u.as_str()
                        .ok_or_else(|| invalid("config.url must be a URL string."))?,
                );
            }
            if urls.iter().all(|u| u.trim().is_empty()) {
                return Err(invalid("Enter the URL the source reads from."));
            }
            for u in urls {
                check_source_url(u, policy, tenant)?;
            }
        }
        WebsiteMode::Crawl | WebsiteMode::Sitemap => {
            let u = o
                .get("url")
                .and_then(Value::as_str)
                .filter(|u| !u.trim().is_empty())
                .ok_or_else(|| invalid("Enter the URL the source reads from."))?;
            check_source_url(u, policy, tenant)?;
            if mode == WebsiteMode::Crawl {
                if let Some(c) = o.get("crawl") {
                    validate_crawl(c)?;
                }
            } else if let Some(c) = o.get("sitemap") {
                validate_sitemap(c)?;
            }
        }
    }
    Ok(())
}

fn int_in(x: &Value, lo: u64, hi: u64) -> bool {
    x.as_u64().is_some_and(|n| (lo..=hi).contains(&n))
}

/// A list of 1-50 non-empty strings of at most `max_len` characters, each passing `each`.
fn str_list(x: &Value, max_len: usize, each: impl Fn(&str) -> bool) -> bool {
    x.as_array().is_some_and(|a| {
        (1..=50).contains(&a.len())
            && a.iter().all(|v| {
                v.as_str().is_some_and(|t| {
                    !t.trim().is_empty()
                        && t.chars().count() <= max_len
                        && !t.chars().any(char::is_control)
                        && each(t)
                })
            })
    })
}

/// A bare host name such as `help.example.nl`: letters, digits, `-` and `.`, no scheme, port or path.
fn host_ok(h: &str) -> bool {
    h.len() <= 253
        && h.split('.').all(|l| {
            (1..=63).contains(&l.len())
                && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                && !l.starts_with('-')
                && !l.ends_with('-')
        })
}

/// strata_ingest's crawl settings: `max_depth` 1-5, `max_pages` 1-200, `allowed_domains` host names, `link_query`.
fn validate_crawl(v: &Value) -> ApiResult<()> {
    let o = v
        .as_object()
        .ok_or_else(|| invalid("config.crawl must be an object."))?;
    for (k, x) in o {
        let (ok, rule) = match k.as_str() {
            "max_depth" => (int_in(x, 1, 5), "a whole number from 1 to 5"),
            "max_pages" => (int_in(x, 1, 200), "a whole number from 1 to 200"),
            "allowed_domains" => (
                str_list(x, 253, host_ok),
                "a list of 1-50 host names such as help.example.nl",
            ),
            "link_query" => (
                x.as_str().is_some_and(|t| {
                    !t.trim().is_empty()
                        && t.chars().count() <= 500
                        && !t.chars().any(char::is_control)
                }),
                "text of 1-500 characters",
            ),
            _ => {
                return Err(invalid(format!(
                    "config.crawl.{} is not a crawl setting.",
                    short(k)
                )));
            }
        };
        if !ok {
            return Err(invalid(format!("config.crawl.{k} must be {rule}.")));
        }
    }
    Ok(())
}

/// strata_ingest's sitemap settings: `max_urls` 1-1000, `include` / `exclude` URL substrings.
fn validate_sitemap(v: &Value) -> ApiResult<()> {
    let o = v
        .as_object()
        .ok_or_else(|| invalid("config.sitemap must be an object."))?;
    for (k, x) in o {
        let (ok, rule) = match k.as_str() {
            "max_urls" => (int_in(x, 1, 1000), "a whole number from 1 to 1000"),
            "include" | "exclude" => (
                str_list(x, 200, |_| true),
                "a list of 1-50 URL fragments of at most 200 characters",
            ),
            _ => {
                return Err(invalid(format!(
                    "config.sitemap.{} is not a sitemap setting.",
                    short(k)
                )));
            }
        };
        if !ok {
            return Err(invalid(format!("config.sitemap.{k} must be {rule}.")));
        }
    }
    Ok(())
}

fn validate_options(v: &Value) -> ApiResult<()> {
    let o = v
        .as_object()
        .ok_or_else(|| invalid("config.options must be an object."))?;
    for (k, x) in o {
        let ok = match k.as_str() {
            "ocr" => x.as_str().is_some_and(|t| {
                (1..=20).contains(&t.len())
                    && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
            }),
            "captions" => x.is_boolean(),
            "language" => {
                x.is_null()
                    || x.as_str().is_some_and(|t| {
                        (1..=35).contains(&t.len())
                            && t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    })
            }
            _ => false,
        };
        if !ok {
            return Err(invalid(format!(
                "config.options.{} is not a valid strata_ingest option.",
                short(k)
            )));
        }
    }
    Ok(())
}

const FORBIDDEN_HEADERS: [&str; 8] = [
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "upgrade",
    "te",
    "trailer",
    "cookie",
];

fn header_name_ok(h: &str) -> bool {
    (1..=64).contains(&h.len())
        && h.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
        && !FORBIDDEN_HEADERS.contains(&h.to_ascii_lowercase().as_str())
}

/// `config.auth`: `{type: none|bearer}` or `{type: header, header_name}`.
pub fn validate_auth(a: Option<&Value>) -> ApiResult<()> {
    let Some(a) = a else { return Ok(()) };
    if a.is_null() {
        return Ok(());
    }
    let o = a
        .as_object()
        .ok_or_else(|| invalid("config.auth must be an object."))?;
    if o.keys().any(|k| k != "type" && k != "header_name") {
        return Err(invalid("config.auth takes only type and header_name."));
    }
    match (o.get("type").and_then(Value::as_str), o.get("header_name")) {
        (Some("none" | "bearer"), None | Some(Value::Null)) => Ok(()),
        (Some("header"), Some(Value::String(h))) if header_name_ok(h) => Ok(()),
        (Some("header"), _) => Err(invalid(
            "config.auth.header_name must be an HTTP header name such as X-Api-Key (not Host, Cookie or a hop-by-hop header).",
        )),
        _ => Err(invalid(
            "config.auth.type must be none, bearer or header; header_name goes only with header.",
        )),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Rows

/// One source with its last run, as `GET …/sources` reads it. The credential column is never selected, only
/// whether it is set.
#[derive(sqlx::FromRow)]
struct ViewRow {
    id: Uuid,
    name: String,
    #[sqlx(rename = "type")]
    kind: String,
    config: Value,
    mapping: Value,
    target_store_id: Uuid,
    schedule: Option<String>,
    enabled: bool,
    language: String,
    supersedes: Vec<String>,
    has_credentials: bool,
    credentials_updated_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    updated_by: String,
    run_id: Option<Uuid>,
    run_status: Option<String>,
    run_started_at: Option<DateTime<Utc>>,
    run_finished_at: Option<DateTime<Utc>>,
    run_started_by: Option<String>,
    run_documents: Option<i32>,
    run_pages: Option<Value>,
    run_error: Option<Value>,
    run_ingest_job_ids: Option<Vec<String>>,
    run_mapping: Option<String>,
    run_uploads: Option<Value>,
    run_replace_report: Option<Value>,
}

impl ViewRow {
    fn json(self, job: &str) -> Value {
        let last_run = match (self.run_id, self.run_status, self.run_started_at) {
            (Some(id), Some(status), Some(started_at)) => json!({
                "id": id,
                "source_id": self.id,
                "status": status,
                "started_at": started_at,
                "finished_at": self.run_finished_at,
                "started_by": self.run_started_by,
                "documents": self.run_documents.unwrap_or(0),
                "pages": self.run_pages,
                "error": self.run_error,
                "ingest_job_ids": self.run_ingest_job_ids.unwrap_or_default(),
                "mapping": self.run_mapping,
                "uploads": self.run_uploads,
                "replace_report": self.run_replace_report,
            }),
            _ => Value::Null,
        };
        json!({
            "id": self.id,
            "job": job,
            "name": self.name,
            "type": self.kind,
            "config": self.config,
            "mapping": self.mapping,
            "target_store_id": self.target_store_id,
            "schedule_active": self.schedule.is_some() && crate::knowledge::schedule::active(),
            "schedule": self.schedule,
            "enabled": self.enabled,
            "has_credentials": self.has_credentials,
            "credentials_updated_at": self.credentials_updated_at,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
            "updated_by": self.updated_by,
            "last_run": last_run,
            "language": self.language,
            "supersedes": self.supersedes,
        })
    }
}

/// The job's sources (one when `id` is set), ordered by name, each with its latest run.
const VIEW_SQL: &str = "SELECT s.id, s.name, s.type, s.config, s.mapping, s.target_store_id, s.schedule, s.enabled, \
        s.language, s.supersedes, (s.credentials_sealed IS NOT NULL) AS has_credentials, s.credentials_updated_at, s.created_at, \
        s.updated_at, s.updated_by, \
        r.id AS run_id, r.status AS run_status, r.started_at AS run_started_at, r.finished_at AS run_finished_at, \
        r.started_by AS run_started_by, r.documents AS run_documents, r.pages AS run_pages, r.error AS run_error, \
        r.ingest_job_ids AS run_ingest_job_ids, r.mapping AS run_mapping, r.uploads AS run_uploads, \
        r.replace_report AS run_replace_report \
   FROM source s \
   LEFT JOIN LATERAL (SELECT * FROM source_run sr WHERE sr.source_id = s.id \
                       ORDER BY sr.started_at DESC, sr.id DESC LIMIT 1) r ON true \
  WHERE s.job_id = $1 AND ($2::uuid IS NULL OR s.id = $2) \
  ORDER BY s.name LIMIT $3";

/// Most sources one job lists (fixture `paged(.., {limit: 200})`).
pub const LIST_LIMIT: i64 = 200;

/// The job's sources as UI `Source` values, ordered by name.
pub async fn list(c: &mut PgConnection, ctx: &JobCtx) -> ApiResult<Vec<Value>> {
    let rows: Vec<ViewRow> = sqlx::query_as(VIEW_SQL)
        .bind(ctx.job_id)
        .bind(None::<Uuid>)
        .bind(LIST_LIMIT)
        .fetch_all(&mut *c)
        .await?;
    Ok(rows.into_iter().map(|r| r.json(&ctx.job_key)).collect())
}

/// One source as UI `Source`, or 404 `source_not_found`.
pub async fn view(c: &mut PgConnection, ctx: &JobCtx, id: Uuid) -> ApiResult<Value> {
    let row: Option<ViewRow> = sqlx::query_as(VIEW_SQL)
        .bind(ctx.job_id)
        .bind(Some(id))
        .bind(1_i64)
        .fetch_optional(&mut *c)
        .await?;
    row.map(|r| r.json(&ctx.job_key))
        .ok_or_else(source_not_found)
}

/// A stored source as writes, tests and deletes need it: the sealed credential still sealed.
#[derive(sqlx::FromRow)]
pub struct Stored {
    pub id: Uuid,
    #[sqlx(rename = "type")]
    pub kind: String,
    pub config: Value,
    pub language: String,
    credentials_sealed: Option<String>,
}

impl Stored {
    pub fn kind(&self) -> ApiResult<SourceType> {
        SourceType::parse(&self.kind)
            .ok_or_else(|| ApiError::Internal(anyhow::anyhow!("unknown source type {}", self.kind)))
    }

    pub fn has_credentials(&self) -> bool {
        self.credentials_sealed.is_some()
    }

    /// The stored key, opened (409 `secret_unreadable`, 503 `provider_secret_key_unconfigured`).
    pub fn secret(&self, kr: Option<&Keyring>, tenant: Uuid) -> ApiResult<Option<SecretText>> {
        match &self.credentials_sealed {
            None => Ok(None),
            Some(s) => secret::open(kr, tenant, SecretKind::SourceCred, self.id, s).map(Some),
        }
    }
}

/// The source `id` of the job, row-locked when `lock`, or 404 `source_not_found`.
pub async fn stored(c: &mut PgConnection, ctx: &JobCtx, id: Uuid, lock: bool) -> ApiResult<Stored> {
    let sql = if lock {
        "SELECT id, type, config, language, credentials_sealed FROM source WHERE job_id = $1 AND id = $2 FOR UPDATE"
    } else {
        "SELECT id, type, config, language, credentials_sealed FROM source WHERE job_id = $1 AND id = $2"
    };
    sqlx::query_as(sql)
        .bind(ctx.job_id)
        .bind(id)
        .fetch_optional(&mut *c)
        .await?
        .ok_or_else(source_not_found)
}

/// `credentials` sealed for source `id` of `tenant` (503 `provider_secret_key_unconfigured` without a keyring).
pub fn seal(kr: Option<&Keyring>, tenant: Uuid, id: Uuid, c: &Credentials) -> ApiResult<Sealed> {
    secret::seal(kr, tenant, SecretKind::SourceCred, id, &c.secret)
}

/// A unique violation on the job's source names is 409 `source_exists`; any other error as is.
fn on_name_conflict(e: sqlx::Error, name: &str) -> ApiError {
    let dup = e
        .as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|c| c == "23505");
    if dup {
        source_exists(name)
    } else {
        ApiError::from(e)
    }
}

/// Inserts source `id` of the job.
pub async fn insert(
    c: &mut PgConnection,
    ctx: &JobCtx,
    id: Uuid,
    i: &SourceInput,
    ck: &Checked,
    sealed: Option<&Sealed>,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO source (job_id, id, name, type, config, mapping, target_store_id, schedule, enabled, language, \
                             credentials_sealed, credentials_fpr, credentials_updated_at, updated_by, supersedes) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, \
                 CASE WHEN $11::text IS NULL THEN NULL ELSE clock_timestamp() END, $13, $14)",
    )
    .bind(ctx.job_id)
    .bind(id)
    .bind(&ck.name)
    .bind(i.kind.as_str())
    .bind(&i.config)
    .bind(Json(&ck.mapping))
    .bind(i.target_store_id)
    .bind(&ck.schedule)
    .bind(i.enabled)
    .bind(&ck.language)
    .bind(sealed.map(|s| s.sealed.as_str()))
    .bind(sealed.map(|s| s.fingerprint.as_str()))
    .bind(&ctx.t.caller.actor)
    .bind(ck.supersedes.clone().unwrap_or_default())
    .execute(&mut *c)
    .await
    .map_err(|e| on_name_conflict(e, &ck.name))?;
    Ok(())
}

/// What a save does to the stored credential.
#[derive(Debug)]
pub enum SealedChange {
    Keep,
    Clear,
    Set(Sealed),
}

impl SealedChange {
    /// The audit word (fixture `saveSource`): `kept`, `cleared`, `replaced`.
    pub fn word(&self) -> &'static str {
        match self {
            SealedChange::Keep => "kept",
            SealedChange::Clear => "cleared",
            SealedChange::Set(_) => "replaced",
        }
    }
}

/// Updates source `id` of the job (its type never changes).
pub async fn update(
    c: &mut PgConnection,
    ctx: &JobCtx,
    id: Uuid,
    i: &SourceInput,
    ck: &Checked,
    change: &SealedChange,
) -> ApiResult<()> {
    let (mode, sealed, fpr) = match change {
        SealedChange::Keep => ("keep", None, None),
        SealedChange::Clear => ("clear", None, None),
        SealedChange::Set(s) => ("set", Some(s.sealed.as_str()), Some(s.fingerprint.as_str())),
    };
    let r = sqlx::query(
        "UPDATE source SET name = $3, config = $4, mapping = $5, target_store_id = $6, schedule = $7, enabled = $8, \
                language = $9, \
                credentials_sealed = CASE $10::text WHEN 'keep' THEN credentials_sealed WHEN 'clear' THEN NULL \
                                     ELSE $11::text END, \
                credentials_fpr = CASE $10::text WHEN 'keep' THEN credentials_fpr WHEN 'clear' THEN NULL \
                                  ELSE $12::text END, \
                credentials_updated_at = CASE $10::text WHEN 'keep' THEN credentials_updated_at \
                                         WHEN 'clear' THEN NULL ELSE clock_timestamp() END, \
                supersedes = coalesce($14::text[], supersedes), \
                updated_at = clock_timestamp(), updated_by = $13 \
          WHERE job_id = $1 AND id = $2",
    )
    .bind(ctx.job_id)
    .bind(id)
    .bind(&ck.name)
    .bind(&i.config)
    .bind(Json(&ck.mapping))
    .bind(i.target_store_id)
    .bind(&ck.schedule)
    .bind(i.enabled)
    .bind(&ck.language)
    .bind(mode)
    .bind(sealed)
    .bind(fpr)
    .bind(&ctx.t.caller.actor)
    .bind(ck.supersedes.clone())
    .execute(&mut *c)
    .await
    .map_err(|e| on_name_conflict(e, &ck.name))?;
    if r.rows_affected() == 0 {
        return Err(source_not_found());
    }
    Ok(())
}

/// Whether a re-sync of source `id` is running.
pub async fn running(c: &mut PgConnection, id: Uuid) -> ApiResult<bool> {
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM source_run WHERE source_id = $1 AND status = 'running')",
    )
    .bind(id)
    .fetch_one(&mut *c)
    .await?)
}

/// The documents a source holds in strata_rag as its runs counted them: every run not yet replaced for a file
/// source (one document per file, across runs), the last completed run for a feed (fixture `deleteSource`).
pub async fn documents_held(c: &mut PgConnection, id: Uuid, kind: SourceType) -> ApiResult<i64> {
    let n: Option<i64> = if kind == SourceType::File {
        sqlx::query_scalar(
            "SELECT sum(documents)::bigint FROM source_run WHERE source_id = $1 AND NOT documents_removed",
        )
        .bind(id)
        .fetch_one(&mut *c)
        .await?
    } else {
        sqlx::query_scalar(
            "SELECT documents::bigint FROM source_run WHERE source_id = $1 AND status = 'completed' \
              ORDER BY started_at DESC, id DESC LIMIT 1",
        )
        .bind(id)
        .fetch_optional(&mut *c)
        .await?
    };
    Ok(n.unwrap_or(0))
}

/// Deletes source `id` of the job (its runs go with it) unless a run started meanwhile (409 `run_in_progress`).
pub async fn delete(c: &mut PgConnection, ctx: &JobCtx, id: Uuid) -> ApiResult<()> {
    let r = sqlx::query(
        "DELETE FROM source WHERE job_id = $1 AND id = $2 \
            AND NOT EXISTS (SELECT 1 FROM source_run WHERE source_id = $2 AND status = 'running')",
    )
    .bind(ctx.job_id)
    .bind(id)
    .execute(&mut *c)
    .await?;
    if r.rows_affected() == 0 {
        return Err(run_in_progress());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web(cfg: Value) -> ApiResult<()> {
        validate_config(
            SourceType::Website,
            &cfg,
            &Policy::default(),
            Uuid::new_v4(),
        )
    }

    /// The `invalid_source` message of a refused config.
    fn refused(cfg: Value) -> String {
        match web(cfg.clone()) {
            Err(ApiError::Unprocessable("invalid_source", m)) => m,
            other => panic!("{cfg} was not refused as invalid_source: {other:?}"),
        }
    }

    #[test]
    fn urls_mode_is_the_default() {
        assert!(web(json!({"urls": ["https://help.example.nl/a"]})).is_ok());
        assert!(web(json!({"mode": "urls", "urls": ["https://help.example.nl/a"]})).is_ok());
        assert!(web(json!({"url": "https://help.example.nl/"})).is_ok());
        assert!(refused(json!({"urls": []})).contains("1 to 50"));
        assert!(refused(json!({})).contains("Enter the URL"));
        assert!(
            refused(json!({"urls": ["https://help.example.nl/a"], "crawl": {"max_depth": 2}}))
                .contains("config.crawl")
        );
    }

    #[test]
    fn crawl_and_sitemap_take_one_start_url_and_bounded_settings() {
        assert!(web(json!({"mode": "crawl", "url": "https://help.example.nl/"})).is_ok());
        assert!(
            web(json!({"mode": "crawl", "url": "https://help.example.nl/",
                        "crawl": {"max_depth": 5, "max_pages": 200, "allowed_domains": ["help.example.nl"]}}))
            .is_ok()
        );
        assert!(refused(json!({"mode": "crawl"})).contains("Enter the URL"));
        for (crawl, key) in [
            (json!({"max_depth": 6}), "max_depth"),
            (json!({"max_pages": 0}), "max_pages"),
            (
                json!({"allowed_domains": ["https://help.example.nl"]}),
                "allowed_domains",
            ),
            (json!({"follow_external": true}), "follow_external"),
        ] {
            let m = refused(
                json!({"mode": "crawl", "url": "https://help.example.nl/", "crawl": crawl}),
            );
            assert!(m.contains(&format!("config.crawl.{key}")), "{m}");
        }
        assert!(
            web(
                json!({"mode": "sitemap", "url": "https://help.example.nl/sitemap.xml",
                        "sitemap": {"max_urls": 1000, "include": ["/help/"]}})
            )
            .is_ok()
        );
        assert!(
            refused(
                json!({"mode": "sitemap", "url": "https://help.example.nl/s.xml",
                           "sitemap": {"max_urls": 1001}})
            )
            .contains("config.sitemap.max_urls")
        );
        assert!(
            refused(json!({"mode": "spider", "url": "https://help.example.nl/"}))
                .contains("config.mode")
        );
    }

    #[test]
    fn source_urls_use_the_source_codes() {
        for (url, code) in [
            ("http://help.example.nl/", "invalid_source"),
            ("https://help.example.nl/a?key=1", "invalid_source"),
            ("not a url", "invalid_source"),
            ("https://10.0.0.1/sitemap.xml", "address_refused"),
            ("https://100.64.0.1/x", "address_refused"),
            ("https://minio.internal/x", "address_refused"),
        ] {
            let r = web(json!({"mode": "sitemap", "url": url}));
            match r {
                Err(ApiError::Unprocessable(c, _)) => assert_eq!(c, code, "{url}"),
                other => panic!("{url}: {other:?}"),
            }
        }
    }

    #[test]
    fn auth_and_datatable_settings() {
        let p = Policy::default();
        let t = Uuid::new_v4();
        let api = |auth: Value| {
            validate_config(
                SourceType::Api,
                &json!({"url": "https://api.example.com/v1/promos", "auth": auth}),
                &p,
                t,
            )
        };
        assert!(api(json!({"type": "bearer"})).is_ok());
        assert!(api(json!({"type": "header", "header_name": "X-Api-Key"})).is_ok());
        assert!(api(json!({"type": "header", "header_name": "Cookie"})).is_err());
        assert!(api(json!({"type": "bearer", "header_name": "X"})).is_err());
        let dt = |cfg: Value| validate_config(SourceType::Datatable, &cfg, &p, t);
        assert!(
            dt(json!({"url": "https://x.example.com/t.csv", "format": "csv", "header_row": false}))
                .is_ok()
        );
        assert!(dt(json!({"url": "https://x.example.com/t.csv"})).is_ok());
        assert!(dt(json!({"url": "https://x.example.com/t.ods", "format": "ods"})).is_err());
        assert!(dt(json!({"url": "https://x.example.com/t.csv", "header_row": "yes"})).is_err());
        assert!(
            validate_config(
                SourceType::File,
                &json!({"url": "https://x.example.com"}),
                &p,
                t
            )
            .is_err()
        );
    }

    #[test]
    fn cron_language_and_credentials() {
        assert!(valid_cron("0 3 * * *"));
        assert!(valid_cron("*/15 6-18 * * 1,2"));
        assert!(!valid_cron("0 3 * *"));
        assert!(!valid_cron("@daily"));
        assert!(valid_language("nl"));
        assert!(valid_language("en-GB"));
        assert!(!valid_language("EN"));
        assert!(!valid_language("english"));
        assert!(matches!(
            credentials_change(None),
            Ok(CredentialsChange::Keep)
        ));
        assert!(matches!(
            credentials_change(Some(Value::Null)),
            Ok(CredentialsChange::Clear)
        ));
        assert!(matches!(
            credentials_change(Some(json!({"secret": ""}))),
            Ok(CredentialsChange::Keep)
        ));
        assert!(matches!(
            credentials_change(Some(json!({"secret": "k-1"}))),
            Ok(CredentialsChange::Set(_))
        ));
        assert!(credentials_change(Some(json!({"secret": "a\nb"}))).is_err());
        assert!(credentials_change(Some(json!({"key": "x"}))).is_err());
        let c = Credentials {
            secret: "sk-live-1".into(),
        };
        assert_eq!(format!("{c:?}"), "Credentials([redacted])");
    }

    #[test]
    fn mapping_duplicates_before_unknown_attributes() {
        let row = |f: &str| MappingRow {
            source_field: f.into(),
            entity: "Offering".into(),
            attribute: "price".into(),
        };
        let code = |r: ApiResult<Vec<MappingRow>>| r.unwrap_err().code();
        assert_eq!(
            code(validate_mapping(&[], &[row("price"), row("Price ")])),
            "duplicate_field"
        );
        assert_eq!(
            code(validate_mapping(&[], &[row("price")])),
            "unknown_attribute"
        );
        let targets = [MappingTarget {
            entity: "Offering".into(),
            attribute: "price".into(),
            kind: "number".into(),
            pii: false,
        }];
        let kept = validate_mapping(&targets, &[row(" price "), row("  ")]).unwrap();
        assert_eq!(kept, [row("price")], "blank rows dropped, fields trimmed");
        assert_eq!(strata_source_id(Uuid::nil()), Uuid::nil().to_string());
    }

    #[test]
    fn targets_are_visible_attributes_without_conflicts() {
        use crate::job_design::types::{
            AttributeConflict, OntologyAttribute, OntologyScope, Origin,
        };
        let attr = |name: &str, ty: &str, pii: bool, conflict: Option<AttributeConflict>| {
            OntologyAttribute {
                name: name.into(),
                data_type: ty.into(),
                pii,
                synonyms: vec![],
                origin: Origin::Base,
                conflict,
            }
        };
        let entity =
            |name: &str, hidden: bool, attributes: Vec<OntologyAttribute>| OntologyEntity {
                tenant_id: Uuid::nil().to_string(),
                job: "support".into(),
                name: name.into(),
                label: name.into(),
                scope: OntologyScope::Generic,
                industry: None,
                department: None,
                synonyms: vec![],
                attributes,
                origin: Origin::Base,
                hidden,
                object_key: None,
                layer: "base".into(),
                created_by: None,
                created_at: DateTime::UNIX_EPOCH,
            };
        let got = targets_of(&[
            entity(
                "Customer",
                false,
                vec![
                    attr("email", "string", true, None),
                    attr(
                        "old",
                        "string",
                        false,
                        Some(AttributeConflict::ParentMissing),
                    ),
                ],
            ),
            entity("Secret", true, vec![attr("x", "string", false, None)]),
            entity(
                "Offering",
                false,
                vec![attr("price", "number", false, None)],
            ),
        ]);
        let t = |e: &str, a: &str, k: &str, pii| MappingTarget {
            entity: e.into(),
            attribute: a.into(),
            kind: k.into(),
            pii,
        };
        assert_eq!(
            got,
            [
                t("Customer", "email", "string", true),
                t("Offering", "price", "number", false)
            ]
        );
        assert!(targets_of(&[]).is_empty());
    }

    #[test]
    fn supersedes_is_bounded_trimmed_sorted_and_deduplicated() {
        assert_eq!(
            normalize_supersedes(&json!([" returns ", "hours", "returns"])).unwrap(),
            ["hours", "returns"]
        );
        assert_eq!(
            normalize_supersedes(&json!([])).unwrap(),
            Vec::<String>::new()
        );
        let max: Vec<String> = (0..MAX_SUPERSEDES).map(|i| format!("k{i}")).collect();
        assert_eq!(
            normalize_supersedes(&json!(max)).unwrap().len(),
            MAX_SUPERSEDES
        );
        let over: Vec<String> = (0..=MAX_SUPERSEDES).map(|i| format!("k{i}")).collect();
        assert!(
            normalize_supersedes(&json!(over))
                .unwrap_err()
                .contains("at most 32")
        );
        let long = "k".repeat(MAX_SUPERSEDES_CHARS);
        assert!(normalize_supersedes(&json!([long])).is_ok());
        // strata's charset: a key outside [a-z0-9_./-] is refused here, not by strata's 400
        for bad in [
            "k".repeat(MAX_SUPERSEDES_CHARS + 1),
            "é".to_string(),
            "  ".to_string(),
            "a\tb".to_string(),
            "Hours".to_string(),
            "opening hours".to_string(),
        ] {
            assert!(
                normalize_supersedes(&json!([bad]))
                    .unwrap_err()
                    .contains("[a-z0-9_./-]"),
                "{bad:?}"
            );
        }
        assert!(
            normalize_supersedes(&json!([1]))
                .unwrap_err()
                .contains("string")
        );
        assert!(
            normalize_supersedes(&json!("hours"))
                .unwrap_err()
                .contains("list")
        );
        // strata_ingest #15: top-level in a JSON body, `[]` included (a cleared list must clear strata's copy)
        let mut body = json!({"store_id": "s"});
        forward_supersedes(&mut body, &["hours".to_string()]);
        assert_eq!(body["supersedes"], json!(["hours"]));
        forward_supersedes(&mut body, &[]);
        assert_eq!(body["supersedes"], json!([]));
    }

    #[test]
    fn a_secret_outside_credentials_is_refused() {
        let body = json!({"name": "Promos", "type": "api", "target_store_id": Uuid::nil(),
                          "config": {"url": "https://api.example.com/x", "token": "abc"}});
        assert_eq!(parse(body).unwrap_err().code(), "secret_rejected");
        let ok = json!({"name": "Promos", "type": "api", "target_store_id": Uuid::nil(),
                        "config": {"url": "https://api.example.com/x", "auth": {"type": "bearer"}},
                        "credentials": {"secret": "sk-live-abcdefghijklmnopqrstuvwxyz0123456789"}});
        let p = parse(ok).unwrap();
        assert!(matches!(p.credentials, CredentialsChange::Set(_)));
        assert_eq!(parse(json!([1])).unwrap_err().code(), "invalid_body");
    }
}
