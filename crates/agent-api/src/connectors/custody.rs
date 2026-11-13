//! Credential custody (B7 T8, from P6 T3 + T4): a connector instance's secret fields, sealed at rest.
//!
//! - Each tenant has one data key (DEK, 32 random bytes) in `tenant_data_key` (0093), stored only wrapped by the
//!   TenantData keyring (crate::crypto::keyring) with AAD `tenant ‖ kek_id`, reached through the definer functions
//!   `agent_tenant_dek()` / `agent_tenant_dek_put()` of the caller's tenant (`app.current_tenant`). The first seal
//!   mints it; replicas minting at once converge on whichever key landed first.
//! - A credential is ONE JSON object of the template's secret fields, sealed AES-256-GCM under the DEK with AAD
//!   `tenant ‖ instance_id ‖ credential_version` (big-endian i32): a row copied onto another instance, another
//!   tenant, or replayed at another version does not open. Every seal bumps `credential_version`.
//! - `fingerprint` = the first 8 hex of HMAC-SHA256(DEK, canonical JSON of the secret fields, object keys sorted):
//!   stable for the same secret, different on change, meaningless outside the tenant (the UI shows it).
//! - Unwrapped DEKs are cached in-process for at most [`DEK_TTL`] and wiped on drop; [`destroy_tenant`] evicts.
//! - Without `TENANT_DATA_KEYS` every call is 503 `tenant_data_key_unconfigured`. Nothing here logs a secret, a
//!   key or a ciphertext.
//!
//! Callers pass a connection scoped to `tenant` (crate::tenancy::begin_scoped), except [`destroy_tenant`], which
//! switches to `agent_platform` for its one call when the connection is not already that role.
use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Map, Value};
use sqlx::PgConnection;
use uuid::Uuid;
use zeroize::Zeroize;

use crate::crypto::keyring::{KEY_LEN, Keyring};
use crate::error::{ApiError, ApiResult};
use crate::platform_db::{self, PLATFORM_ROLE};

/// How long an unwrapped DEK stays in the process cache.
pub const DEK_TTL: Duration = Duration::from_secs(300);
/// The version of every DEK minted here (a re-key bumps it; not built yet).
const FIRST_DEK_VERSION: i32 = 1;

/// What [`seal_credential`] wrote. `Debug` shows lengths, never bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedCredential {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub credential_version: i32,
    pub dek_version: i32,
    pub fingerprint: String,
}

impl std::fmt::Debug for SealedCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedCredential")
            .field("ciphertext_len", &self.ciphertext.len())
            .field("credential_version", &self.credential_version)
            .field("dek_version", &self.dek_version)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// An unwrapped tenant DEK; its bytes are overwritten when the last reference drops.
struct Dek {
    bytes: [u8; KEY_LEN],
    version: i32,
}

impl Drop for Dek {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

type DekCache = Mutex<HashMap<Uuid, (Arc<Dek>, Instant)>>;

fn cache() -> &'static DekCache {
    static CACHE: OnceLock<DekCache> = OnceLock::new();
    CACHE.get_or_init(DekCache::default)
}

fn cache_get(tenant: Uuid) -> Option<Arc<Dek>> {
    let mut c = cache().lock().unwrap_or_else(PoisonError::into_inner);
    let fresh = c
        .get(&tenant)
        .filter(|(_, at)| at.elapsed() < DEK_TTL)
        .map(|(dek, _)| Arc::clone(dek));
    if fresh.is_none() {
        c.remove(&tenant);
    }
    fresh
}

fn cache_put(tenant: Uuid, dek: Arc<Dek>) {
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(tenant, (dek, Instant::now()));
}

fn cache_evict(tenant: Uuid) {
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .remove(&tenant);
}

fn rng() -> &'static SystemRandom {
    static RNG: OnceLock<SystemRandom> = OnceLock::new();
    RNG.get_or_init(SystemRandom::new)
}

fn unconfigured() -> ApiError {
    ApiError::Unavailable(
        "tenant_data_key_unconfigured",
        "credential custody is not configured on this deployment (TENANT_DATA_KEYS)".into(),
        None,
    )
}

fn destroyed() -> ApiError {
    ApiError::Conflict(
        "tenant_data_key_destroyed",
        "this workspace's data key was destroyed; its credentials can no longer be read".into(),
    )
}

fn dek_unreadable() -> ApiError {
    ApiError::Unavailable(
        "tenant_data_key_unreadable",
        "this workspace's data key does not open under the configured TENANT_DATA_KEYS".into(),
        None,
    )
}

fn credential_unreadable() -> ApiError {
    ApiError::Conflict(
        "credential_unreadable",
        "the stored credential does not open; set it again".into(),
    )
}

fn seal_failed() -> ApiError {
    ApiError::Internal(anyhow::anyhow!("could not seal the credential"))
}

fn configured(keyring: Option<&Keyring>) -> ApiResult<&Keyring> {
    keyring.ok_or_else(unconfigured)
}

/// `tenant ‖ instance_id ‖ credential_version` (tenant fixed-length first, version fixed-length last).
fn credential_aad(tenant: Uuid, instance_id: &str, credential_version: i32) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + instance_id.len() + 4);
    out.extend_from_slice(tenant.as_bytes());
    out.extend_from_slice(instance_id.as_bytes());
    out.extend_from_slice(&credential_version.to_be_bytes());
    out
}

fn aead_key(dek: &Dek) -> LessSafeKey {
    let unbound = UnboundKey::new(&AES_256_GCM, &dek.bytes)
        .expect("a 32-byte key is always a valid AES-256-GCM key");
    LessSafeKey::new(unbound)
}

/// Canonical JSON: object keys sorted (recursively), no insignificant whitespace.
fn canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(m) => canonical_object(m, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

fn canonical_object(m: &Map<String, Value>, out: &mut String) {
    let mut entries: Vec<(&String, &Value)> = m.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    out.push('{');
    for (i, (k, v)) in entries.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&Value::String(k.clone()).to_string());
        out.push(':');
        canonical(v, out);
    }
    out.push('}');
}

/// First 8 hex of HMAC-SHA256(`dek`, canonical JSON of `secret_fields`).
fn fingerprint_of(dek: &Dek, secret_fields: &Map<String, Value>) -> String {
    let mut text = String::new();
    canonical_object(secret_fields, &mut text);
    let key = hmac::Key::new(hmac::HMAC_SHA256, &dek.bytes);
    let tag = hmac::sign(&key, text.as_bytes());
    let mut text_bytes = text.into_bytes();
    text_bytes.zeroize();
    let mut hex = String::with_capacity(8);
    for b in &tag.as_ref()[..4] {
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// The caller tenant's key row: `(wrapped_dek, dek_version, destroyed)`.
async fn read_dek(conn: &mut PgConnection) -> ApiResult<Option<(Option<Vec<u8>>, i32, bool)>> {
    Ok(sqlx::query_as::<_, (Option<Vec<u8>>, i32, bool)>(
        "SELECT wrapped_dek, dek_version, destroyed_at IS NOT NULL FROM agent_tenant_dek()",
    )
    .fetch_optional(&mut *conn)
    .await?)
}

/// The tenant's DEK, unwrapped (cached). `create`: mint and store one when the tenant has none yet; otherwise a
/// tenant without a key is `None`.
async fn tenant_dek(
    conn: &mut PgConnection,
    keyring: &Keyring,
    tenant: Uuid,
    create: bool,
) -> ApiResult<Option<Arc<Dek>>> {
    if let Some(dek) = cache_get(tenant) {
        return Ok(Some(dek));
    }
    let mut row = read_dek(conn).await?;
    if row.is_none() && create {
        let mut fresh = Dek {
            bytes: [0u8; KEY_LEN],
            version: FIRST_DEK_VERSION,
        };
        rng().fill(&mut fresh.bytes).map_err(|_| seal_failed())?;
        let wrapped = keyring
            .seal(tenant.as_bytes(), &fresh.bytes)
            .map_err(|_| seal_failed())?;
        sqlx::query("SELECT agent_tenant_dek_put($1, $2, $3)")
            .bind(wrapped.into_bytes())
            .bind(keyring.current_kid().to_string())
            .bind(fresh.version)
            .execute(&mut *conn)
            .await?;
        // Re-read: a concurrent first seal may have stored its key first, and that one wins.
        row = read_dek(conn).await?;
    }
    let Some((wrapped, version, is_destroyed)) = row else {
        return Ok(None);
    };
    if is_destroyed {
        return Err(destroyed());
    }
    let wrapped = wrapped.ok_or_else(destroyed)?;
    let sealed = std::str::from_utf8(&wrapped).map_err(|_| dek_unreadable())?;
    let mut raw = keyring
        .open(tenant.as_bytes(), sealed)
        .map_err(|_| dek_unreadable())?;
    let bytes: Result<[u8; KEY_LEN], _> = raw.as_slice().try_into();
    raw.zeroize();
    let bytes = bytes.map_err(|_| dek_unreadable())?;
    let dek = Arc::new(Dek { bytes, version });
    cache_put(tenant, Arc::clone(&dek));
    Ok(Some(dek))
}

/// Seal `secret_fields` as the credential of `instance_id` (upsert; `credential_version` bumps on every seal) and
/// return what was stored. 404 `connector_instance_not_found` when the instance is not the tenant's.
pub async fn seal_credential(
    conn: &mut PgConnection,
    keyring: Option<&Keyring>,
    tenant: Uuid,
    instance_id: &str,
    secret_fields: &Map<String, Value>,
    updated_by: &str,
) -> ApiResult<SealedCredential> {
    let keyring = configured(keyring)?;
    let exists: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM connector_instance WHERE tenant_id = $1 AND id = $2 FOR UPDATE",
    )
    .bind(tenant)
    .bind(instance_id)
    .fetch_optional(&mut *conn)
    .await?;
    if exists.is_none() {
        return Err(ApiError::Missing(
            "connector_instance_not_found",
            "no such connector instance".into(),
        ));
    }
    let previous: Option<i32> = sqlx::query_scalar(
        "SELECT credential_version FROM connector_credential WHERE tenant_id = $1 AND instance_id = $2",
    )
    .bind(tenant)
    .bind(instance_id)
    .fetch_optional(&mut *conn)
    .await?;
    let credential_version = previous.map_or(1, |v| v + 1);
    let dek = tenant_dek(conn, keyring, tenant, true)
        .await?
        .ok_or_else(seal_failed)?;
    let mut nonce = [0u8; NONCE_LEN];
    rng().fill(&mut nonce).map_err(|_| seal_failed())?;
    let mut in_out = serde_json::to_vec(secret_fields).map_err(|_| seal_failed())?;
    aead_key(&dek)
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(credential_aad(tenant, instance_id, credential_version)),
            &mut in_out,
        )
        .map_err(|_| seal_failed())?;
    let sealed = SealedCredential {
        ciphertext: in_out,
        nonce: nonce.to_vec(),
        credential_version,
        dek_version: dek.version,
        fingerprint: fingerprint_of(&dek, secret_fields),
    };
    sqlx::query(
        "INSERT INTO connector_credential
                (tenant_id, instance_id, ciphertext, nonce, credential_version, dek_version, fingerprint, updated_by)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT (tenant_id, instance_id) DO UPDATE
            SET ciphertext = EXCLUDED.ciphertext, nonce = EXCLUDED.nonce,
                credential_version = EXCLUDED.credential_version, dek_version = EXCLUDED.dek_version,
                fingerprint = EXCLUDED.fingerprint, updated_by = EXCLUDED.updated_by, updated_at = now()",
    )
    .bind(tenant)
    .bind(instance_id)
    .bind(sealed.ciphertext.clone())
    .bind(sealed.nonce.clone())
    .bind(sealed.credential_version)
    .bind(sealed.dek_version)
    .bind(sealed.fingerprint.clone())
    .bind(updated_by)
    .execute(&mut *conn)
    .await?;
    Ok(sealed)
}

/// The secret fields stored for `instance_id`, or `None` when no credential is set. A credential that does not
/// open (moved, tampered, other DEK version) is 409 `credential_unreadable`; a destroyed tenant key is 409
/// `tenant_data_key_destroyed`.
pub async fn open_credential(
    conn: &mut PgConnection,
    keyring: Option<&Keyring>,
    tenant: Uuid,
    instance_id: &str,
) -> ApiResult<Option<Map<String, Value>>> {
    let keyring = configured(keyring)?;
    let row: Option<(Vec<u8>, Vec<u8>, i32, i32)> = sqlx::query_as(
        "SELECT ciphertext, nonce, credential_version, dek_version
           FROM connector_credential WHERE tenant_id = $1 AND instance_id = $2",
    )
    .bind(tenant)
    .bind(instance_id)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((mut ciphertext, nonce, credential_version, dek_version)) = row else {
        return Ok(None);
    };
    let dek = tenant_dek(conn, keyring, tenant, false)
        .await?
        .ok_or_else(credential_unreadable)?;
    if dek.version != dek_version {
        return Err(credential_unreadable());
    }
    let nonce: [u8; NONCE_LEN] = nonce
        .as_slice()
        .try_into()
        .map_err(|_| credential_unreadable())?;
    let plain = aead_key(&dek)
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(credential_aad(tenant, instance_id, credential_version)),
            &mut ciphertext,
        )
        .map_err(|_| credential_unreadable())?;
    let fields = serde_json::from_slice::<Map<String, Value>>(plain);
    ciphertext.zeroize();
    fields.map(Some).map_err(|_| credential_unreadable())
}

/// A sealed JSON object under the tenant DEK (a channel binding's credential, B12). `Debug` shows no bytes.
#[derive(Clone, PartialEq, Eq)]
pub struct SealedBlob {
    pub ciphertext: Vec<u8>,
    pub nonce: Vec<u8>,
    pub dek_version: i32,
    pub fingerprint: String,
}

impl std::fmt::Debug for SealedBlob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SealedBlob")
            .field("ciphertext_len", &self.ciphertext.len())
            .field("dek_version", &self.dek_version)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// The AAD of a channel binding's credential: `tenant ‖ "binding_cred" ‖ binding_id`.
pub fn binding_aad(tenant: Uuid, binding_id: Uuid) -> Vec<u8> {
    let mut out = Vec::with_capacity(16 + 12 + 16);
    out.extend_from_slice(tenant.as_bytes());
    out.extend_from_slice(b"binding_cred");
    out.extend_from_slice(binding_id.as_bytes());
    out
}

/// Seal `fields` under `tenant`'s DEK (minted on first use) with `aad`; the caller stores the blob. `conn` is scoped to
/// `tenant`.
pub async fn seal_fields(
    conn: &mut PgConnection,
    keyring: Option<&Keyring>,
    tenant: Uuid,
    aad: &[u8],
    fields: &Map<String, Value>,
) -> ApiResult<SealedBlob> {
    let keyring = configured(keyring)?;
    let dek = tenant_dek(conn, keyring, tenant, true)
        .await?
        .ok_or_else(seal_failed)?;
    let mut nonce = [0u8; NONCE_LEN];
    rng().fill(&mut nonce).map_err(|_| seal_failed())?;
    let mut in_out = serde_json::to_vec(fields).map_err(|_| seal_failed())?;
    aead_key(&dek)
        .seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut in_out,
        )
        .map_err(|_| seal_failed())?;
    Ok(SealedBlob {
        ciphertext: in_out,
        nonce: nonce.to_vec(),
        dek_version: dek.version,
        fingerprint: fingerprint_of(&dek, fields),
    })
}

/// Open a blob sealed by [`seal_fields`] with the same `aad`: 409 `credential_unreadable` when it does not open.
pub async fn open_fields(
    conn: &mut PgConnection,
    keyring: Option<&Keyring>,
    tenant: Uuid,
    aad: &[u8],
    ciphertext: &[u8],
    nonce: &[u8],
    dek_version: i32,
) -> ApiResult<Map<String, Value>> {
    let keyring = configured(keyring)?;
    let dek = tenant_dek(conn, keyring, tenant, false)
        .await?
        .ok_or_else(credential_unreadable)?;
    if dek.version != dek_version {
        return Err(credential_unreadable());
    }
    let nonce: [u8; NONCE_LEN] = nonce.try_into().map_err(|_| credential_unreadable())?;
    let mut buf = ciphertext.to_vec();
    let plain = aead_key(&dek)
        .open_in_place(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad),
            &mut buf,
        )
        .map_err(|_| credential_unreadable())?;
    let fields = serde_json::from_slice::<Map<String, Value>>(plain);
    buf.zeroize();
    fields.map_err(|_| credential_unreadable())
}

/// Crypto-shred `tenant`'s credentials (offboard): its DEK is wiped (`agent_tenant_dek_destroy`, as
/// `agent_platform`) and evicted from this process's cache. Idempotent.
pub async fn destroy_tenant(conn: &mut PgConnection, tenant: Uuid) -> ApiResult<()> {
    let role: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&mut *conn)
        .await?;
    let switch = role != PLATFORM_ROLE;
    if switch {
        platform_db::enter(conn).await?;
    }
    sqlx::query("SELECT agent_tenant_dek_destroy($1)")
        .bind(tenant)
        .execute(&mut *conn)
        .await?;
    if switch {
        platform_db::leave(conn).await?;
    }
    cache_evict(tenant);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dek(b: u8) -> Dek {
        Dek {
            bytes: [b; KEY_LEN],
            version: 1,
        }
    }

    fn fields(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => panic!("an object"),
        }
    }

    #[test]
    fn canonical_json_sorts_keys_recursively() {
        let mut out = String::new();
        canonical(
            &json!({"b": 1, "a": {"z": [true, null, "x\"y"], "c": 2.5}}),
            &mut out,
        );
        assert_eq!(out, r#"{"a":{"c":2.5,"z":[true,null,"x\"y"]},"b":1}"#);
    }

    #[test]
    fn fingerprint_is_8_hex_stable_and_key_dependent() {
        let a = fields(json!({"api_key": "s3cret", "user": "u"}));
        let b = fields(json!({"user": "u", "api_key": "s3cret"}));
        let fa = fingerprint_of(&dek(1), &a);
        assert_eq!(fa.len(), 8);
        assert!(
            fa.bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(fa, fingerprint_of(&dek(1), &b), "key order does not matter");
        assert_ne!(fa, fingerprint_of(&dek(2), &a), "another tenant's DEK");
        let changed = fields(json!({"api_key": "s3cret2", "user": "u"}));
        assert_ne!(fa, fingerprint_of(&dek(1), &changed));
    }

    #[test]
    fn the_aad_binds_tenant_instance_and_version() {
        let t = Uuid::nil();
        let base = credential_aad(t, "shop", 1);
        assert_ne!(base, credential_aad(t, "shop", 2));
        assert_ne!(base, credential_aad(t, "shop2", 1));
        assert_ne!(base, credential_aad(Uuid::from_u128(1), "shop", 1));
    }

    #[test]
    fn sealed_credential_debug_has_no_bytes() {
        let s = SealedCredential {
            ciphertext: vec![0xAB; 40],
            nonce: vec![0xCD; 12],
            credential_version: 3,
            dek_version: 1,
            fingerprint: "0011aabb".into(),
        };
        let shown = format!("{s:?}");
        assert!(shown.contains("ciphertext_len: 40"), "{shown}");
        assert!(!shown.contains("171") && !shown.contains("205"), "{shown}");
    }

    #[test]
    fn the_cache_expires_and_evicts() {
        let t = Uuid::new_v4();
        assert!(cache_get(t).is_none());
        cache_put(t, Arc::new(dek(9)));
        assert_eq!(cache_get(t).map(|d| d.bytes[0]), Some(9));
        cache_evict(t);
        assert!(cache_get(t).is_none());
    }
}
