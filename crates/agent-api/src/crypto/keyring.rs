//! The `v2:` keyring (B7 T1 shape, after P3 T5): AES-256-GCM under one of several named keys, one key set per
//! [`Purpose`], so a blob sealed for one purpose never opens under another.
//!
//! - Configuration: `<PURPOSE>_KEYS` = a JSON object `{kid: base64(32 bytes)}` (standard alphabet) and
//!   `<PURPOSE>_KEY_CURRENT` = the kid new seals use. A kid matches `[A-Za-z0-9_-]{1,32}`. Old kids stay in the set
//!   so values sealed under them still open after `current` moves (rotation = add a kid, move current, rewrap).
//! - Stored form: `v2:<kid>:` + base64url (no padding) of `nonce (12) ‖ ciphertext ‖ tag (16)`.
//! - Associated data: `aad_scope ‖ kid`. The caller's scope (e.g. a tenant id) binds the value to its row; the kid
//!   is bound too, so relabelling a blob with another kid that happens to hold the same bytes does not open.
//! - Keys never appear in `Debug`, in errors, or in logs.
//!
//! [`Purpose::TenantData`] (`TENANT_DATA_KEYS`, `TENANT_DATA_KEY_CURRENT`) is OPTIONAL at boot: absent, credential
//! custody answers 503 `tenant_data_key_unconfigured`; present but invalid, the boot fails naming the variable.
//! [`Purpose::ProviderSecret`] (`PROVIDER_SECRET_KEYS`, `PROVIDER_SECRET_KEY_CURRENT`, B3) is optional the same way:
//! absent, every provider/source secret read or write answers 503 `provider_secret_key_unconfigured`.
//! [`Purpose::LicenceToken`] (`LICENSE_TOKEN_ENC_KEYS`, `LICENSE_TOKEN_ENC_KEY_CURRENT`, B7 T1, R12: the third purpose)
//! seals the tenant and member licence tokens; it is REQUIRED, with a transitional fallback to the legacy single key
//! `LICENSE_TOKEN_ENC_KEY` (see crate::crypto::licence).
use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};
use ring::rand::{SecureRandom, SystemRandom};

/// The version prefix of a keyring value.
pub const PREFIX: &str = "v2:";
/// Bytes of one key.
pub const KEY_LEN: usize = 32;
const TAG_LEN: usize = 16;
const KID_MAX: usize = 32;

/// What a key set protects. Each purpose has its own environment variables and its own keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Purpose {
    /// The wrapping keys (KEKs) of the per-tenant data keys (crate::connectors::custody).
    TenantData,
    /// Provider and source secrets (B3, crate::providers::secret): LLM provider keys, slot keys, source credentials.
    ProviderSecret,
    /// Tenant and member licence tokens (B7 T1, crate::crypto::licence; AAD = the tenant id).
    LicenceToken,
}

impl Purpose {
    /// The variable holding the `{kid: base64}` object.
    pub fn keys_var(self) -> &'static str {
        match self {
            Purpose::TenantData => "TENANT_DATA_KEYS",
            Purpose::ProviderSecret => "PROVIDER_SECRET_KEYS",
            Purpose::LicenceToken => "LICENSE_TOKEN_ENC_KEYS",
        }
    }

    /// The variable naming the current kid.
    pub fn current_var(self) -> &'static str {
        match self {
            Purpose::TenantData => "TENANT_DATA_KEY_CURRENT",
            Purpose::ProviderSecret => "PROVIDER_SECRET_KEY_CURRENT",
            Purpose::LicenceToken => "LICENSE_TOKEN_ENC_KEY_CURRENT",
        }
    }
}

/// `[A-Za-z0-9_-]{1,32}`.
pub fn valid_kid(kid: &str) -> bool {
    !kid.is_empty()
        && kid.len() <= KID_MAX
        && kid
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// One purpose's keys, decoded. `Debug` shows the kids, never the key bytes.
#[derive(Clone)]
pub struct KeySet {
    pub current: String,
    pub keys: BTreeMap<String, [u8; KEY_LEN]>,
}

impl std::fmt::Debug for KeySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeySet")
            .field("current", &self.current)
            .field("kids", &self.keys.keys().collect::<Vec<_>>())
            .field("keys", &"[redacted]")
            .finish()
    }
}

impl KeySet {
    /// The key set of `purpose` from the raw values of its two variables (`None` = unset; blank counts as unset).
    /// Both unset → `Ok(None)`. One without the other, a value that is not a JSON object of strings, an empty
    /// object, a bad kid, a key that is not base64 of exactly 32 bytes, or a current kid missing from the set is an
    /// error naming the variable, never echoing a key.
    pub fn parse(
        purpose: Purpose,
        keys_json: Option<&str>,
        current: Option<&str>,
    ) -> Result<Option<Self>, String> {
        let keys_var = purpose.keys_var();
        let current_var = purpose.current_var();
        let keys_json = keys_json.map(str::trim).filter(|s| !s.is_empty());
        let current = current.map(str::trim).filter(|s| !s.is_empty());
        let (raw, current) = match (keys_json, current) {
            (None, None) => return Ok(None),
            (Some(_), None) => return Err(format!("{keys_var} is set but {current_var} is not")),
            (None, Some(_)) => return Err(format!("{current_var} is set but {keys_var} is not")),
            (Some(raw), Some(current)) => (raw, current),
        };
        let encoded: BTreeMap<String, String> = serde_json::from_str(raw)
            .map_err(|_| format!("{keys_var} must be a JSON object of kid to base64 key"))?;
        if encoded.is_empty() {
            return Err(format!("{keys_var} holds no key"));
        }
        let mut keys = BTreeMap::new();
        for (kid, b64) in encoded {
            if !valid_kid(&kid) {
                return Err(format!(
                    "{keys_var}: every kid must match [A-Za-z0-9_-]{{1,32}}"
                ));
            }
            let bytes = STANDARD
                .decode(b64.trim())
                .map_err(|_| format!("{keys_var}: the key of kid {kid} is not valid base64"))?;
            let key: [u8; KEY_LEN] = bytes.as_slice().try_into().map_err(|_| {
                format!(
                    "{keys_var}: the key of kid {kid} must decode to {KEY_LEN} bytes, not {}",
                    bytes.len()
                )
            })?;
            keys.insert(kid, key);
        }
        if !keys.contains_key(current) {
            return Err(format!(
                "{current_var} names a kid that is not in {keys_var}"
            ));
        }
        Ok(Some(Self {
            current: current.to_string(),
            keys,
        }))
    }
}

/// A keyring value could not be sealed or opened. Carries no key, scope or plaintext bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum KeyringError {
    #[error("the value is not a v2 keyring value")]
    Malformed,
    #[error("the value names a key id this keyring does not hold")]
    UnknownKid,
    #[error("the value does not decrypt (wrong key or scope, or a corrupt or moved value)")]
    Undecryptable,
    #[error("could not seal the value")]
    Seal,
}

/// Seals and opens `v2:` values under one purpose's key set.
pub struct Keyring {
    purpose: Purpose,
    current: String,
    keys: BTreeMap<String, LessSafeKey>,
    rng: SystemRandom,
}

impl std::fmt::Debug for Keyring {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keyring")
            .field("purpose", &self.purpose)
            .field("current", &self.current)
            .field("kids", &self.keys.keys().collect::<Vec<_>>())
            .field("keys", &"[redacted]")
            .finish()
    }
}

/// `scope ‖ kid`.
fn aad(scope: &[u8], kid: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(scope.len() + kid.len());
    out.extend_from_slice(scope);
    out.extend_from_slice(kid.as_bytes());
    out
}

impl Keyring {
    pub fn new(purpose: Purpose, set: &KeySet) -> Self {
        let keys = set
            .keys
            .iter()
            .map(|(kid, key)| {
                let unbound = UnboundKey::new(&AES_256_GCM, key)
                    .expect("a 32-byte key is always a valid AES-256-GCM key");
                (kid.clone(), LessSafeKey::new(unbound))
            })
            .collect();
        Self {
            purpose,
            current: set.current.clone(),
            keys,
            rng: SystemRandom::new(),
        }
    }

    pub fn purpose(&self) -> Purpose {
        self.purpose
    }

    /// The kid new values are sealed under.
    pub fn current_kid(&self) -> &str {
        &self.current
    }

    /// The kid a stored value names, if it is a well-formed `v2:<kid>:` value.
    pub fn kid_of(sealed: &str) -> Option<&str> {
        let (kid, _) = sealed.strip_prefix(PREFIX)?.split_once(':')?;
        valid_kid(kid).then_some(kid)
    }

    /// `plaintext` sealed under the current key, bound to `aad_scope`, with a fresh random nonce.
    pub fn seal(&self, aad_scope: &[u8], plaintext: &[u8]) -> Result<String, KeyringError> {
        let key = self.keys.get(&self.current).ok_or(KeyringError::Seal)?;
        let mut nonce = [0u8; NONCE_LEN];
        self.rng.fill(&mut nonce).map_err(|_| KeyringError::Seal)?;
        let mut in_out = plaintext.to_vec();
        key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::from(aad(aad_scope, &self.current)),
            &mut in_out,
        )
        .map_err(|_| KeyringError::Seal)?;
        let mut blob = Vec::with_capacity(NONCE_LEN + in_out.len());
        blob.extend_from_slice(&nonce);
        blob.extend_from_slice(&in_out);
        Ok(format!(
            "{PREFIX}{}:{}",
            self.current,
            URL_SAFE_NO_PAD.encode(blob)
        ))
    }

    /// The plaintext of `sealed` for `aad_scope`, under whichever kid it names.
    pub fn open(&self, aad_scope: &[u8], sealed: &str) -> Result<Vec<u8>, KeyringError> {
        let rest = sealed.strip_prefix(PREFIX).ok_or(KeyringError::Malformed)?;
        let (kid, b64) = rest.split_once(':').ok_or(KeyringError::Malformed)?;
        if !valid_kid(kid) {
            return Err(KeyringError::Malformed);
        }
        let key = self.keys.get(kid).ok_or(KeyringError::UnknownKid)?;
        let mut blob = URL_SAFE_NO_PAD
            .decode(b64)
            .map_err(|_| KeyringError::Malformed)?;
        if blob.len() < NONCE_LEN + TAG_LEN {
            return Err(KeyringError::Malformed);
        }
        let mut nonce = [0u8; NONCE_LEN];
        nonce.copy_from_slice(&blob[..NONCE_LEN]);
        let plain = key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad(aad_scope, kid)),
                &mut blob[NONCE_LEN..],
            )
            .map_err(|_| KeyringError::Undecryptable)?;
        Ok(plain.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key set with `(kid, fill byte)` keys (built at runtime: no key literal is committed).
    fn set(current: &str, keys: &[(&str, u8)]) -> KeySet {
        KeySet {
            current: current.to_string(),
            keys: keys
                .iter()
                .map(|(kid, b)| (kid.to_string(), [*b; KEY_LEN]))
                .collect(),
        }
    }

    fn ring(current: &str, keys: &[(&str, u8)]) -> Keyring {
        Keyring::new(Purpose::TenantData, &set(current, keys))
    }

    const SCOPE: &[u8] = b"tenant-a";

    #[test]
    fn round_trip_with_a_fresh_nonce_each_time() {
        let k = ring("k1", &[("k1", 7)]);
        let a = k.seal(SCOPE, b"secret-bytes").unwrap();
        let b = k.seal(SCOPE, b"secret-bytes").unwrap();
        assert!(a.starts_with("v2:k1:"), "{a}");
        assert!(!a.contains("secret-bytes"));
        assert_ne!(a, b, "a fresh nonce per seal");
        assert_eq!(k.open(SCOPE, &a).unwrap(), b"secret-bytes");
        assert_eq!(k.open(SCOPE, &b).unwrap(), b"secret-bytes");
        let raw = URL_SAFE_NO_PAD.decode(&a["v2:k1:".len()..]).unwrap();
        assert_eq!(raw.len(), NONCE_LEN + "secret-bytes".len() + TAG_LEN);
        assert_eq!(Keyring::kid_of(&a), Some("k1"));
        assert_eq!(k.current_kid(), "k1");
        assert_eq!(k.purpose(), Purpose::TenantData);
    }

    #[test]
    fn opens_by_old_kid_after_current_moves() {
        let before = ring("k1", &[("k1", 1), ("k2", 2)]);
        let sealed = before.seal(SCOPE, b"x").unwrap();
        let after = ring("k2", &[("k1", 1), ("k2", 2)]);
        assert_eq!(after.open(SCOPE, &sealed).unwrap(), b"x");
        let fresh = after.seal(SCOPE, b"x").unwrap();
        assert_eq!(Keyring::kid_of(&fresh), Some("k2"));
        assert_eq!(before.open(SCOPE, &fresh).unwrap(), b"x");
    }

    #[test]
    fn unknown_kid_is_its_own_error() {
        let sealed = ring("k1", &[("k1", 1)]).seal(SCOPE, b"x").unwrap();
        assert_eq!(
            ring("k9", &[("k9", 1)]).open(SCOPE, &sealed),
            Err(KeyringError::UnknownKid)
        );
    }

    #[test]
    fn the_kid_is_bound_in_the_aad() {
        // k1 and k2 hold the same bytes: relabelling k1 → k2 must still not open.
        let k = ring("k1", &[("k1", 5), ("k2", 5)]);
        let sealed = k.seal(SCOPE, b"x").unwrap();
        let relabelled = sealed.replacen("v2:k1:", "v2:k2:", 1);
        assert_eq!(k.open(SCOPE, &relabelled), Err(KeyringError::Undecryptable));
    }

    #[test]
    fn a_value_moved_to_another_scope_does_not_open() {
        let k = ring("k1", &[("k1", 3)]);
        let sealed = k.seal(SCOPE, b"x").unwrap();
        assert_eq!(
            k.open(b"tenant-b", &sealed),
            Err(KeyringError::Undecryptable)
        );
    }

    #[test]
    fn the_wrong_key_bytes_do_not_open() {
        let sealed = ring("k1", &[("k1", 3)]).seal(SCOPE, b"x").unwrap();
        assert_eq!(
            ring("k1", &[("k1", 4)]).open(SCOPE, &sealed),
            Err(KeyringError::Undecryptable)
        );
    }

    #[test]
    fn malformed_and_v1_values_are_malformed() {
        let k = ring("k1", &[("k1", 3)]);
        let v1 = format!("v1:{}", STANDARD.encode([0u8; 40]));
        for bad in [
            "plaintext",
            v1.as_str(),
            "v2:",
            "v2:k1",
            "v2:bad kid:AAAA",
            "v2:k1:not base64!",
            "v2:k1:AAAA",
        ] {
            assert_eq!(k.open(SCOPE, bad), Err(KeyringError::Malformed), "{bad}");
        }
        assert_eq!(Keyring::kid_of("v1:abc"), None);
        assert_eq!(Keyring::kid_of("v2:bad kid:abc"), None);
    }

    #[test]
    fn debug_never_prints_key_bytes() {
        let s = set("k1", &[("k1", 0xAB)]);
        let k = Keyring::new(Purpose::TenantData, &s);
        for shown in [format!("{s:?}"), format!("{k:?}")] {
            assert!(shown.contains("[redacted]"), "{shown}");
            assert!(shown.contains("k1"), "{shown}");
            assert!(!shown.contains("171"), "{shown}"); // 0xAB
            assert!(!shown.contains("0xab") && !shown.contains("AB"), "{shown}");
        }
    }

    fn b64(byte: u8, len: usize) -> String {
        STANDARD.encode(vec![byte; len])
    }

    #[test]
    fn config_absent_is_none_and_partial_is_an_error() {
        let p = Purpose::TenantData;
        assert!(KeySet::parse(p, None, None).unwrap().is_none());
        assert!(KeySet::parse(p, Some("  "), Some("")).unwrap().is_none());
        let keys = format!(r#"{{"k1":"{}"}}"#, b64(1, 32));
        let e = KeySet::parse(p, Some(&keys), None).unwrap_err();
        assert!(e.contains("TENANT_DATA_KEY_CURRENT"), "{e}");
        let e = KeySet::parse(p, None, Some("k1")).unwrap_err();
        assert!(e.contains("TENANT_DATA_KEYS"), "{e}");
    }

    #[test]
    fn config_parses_a_valid_set() {
        let keys = format!(r#"{{"k1":"{}","k-2_b":" {} "}}"#, b64(1, 32), b64(2, 32));
        let s = KeySet::parse(Purpose::TenantData, Some(&keys), Some(" k-2_b "))
            .unwrap()
            .unwrap();
        assert_eq!(s.current, "k-2_b");
        assert_eq!(s.keys.len(), 2);
        assert_eq!(s.keys["k1"], [1u8; 32]);
    }

    #[test]
    fn config_refuses_bad_values_without_echoing_them() {
        let p = Purpose::TenantData;
        let good = b64(9, 32);
        let short = b64(9, 16);
        let long_kid = "k".repeat(33);
        let cases = [
            ("not json".to_string(), "k1", "JSON object"),
            ("[]".to_string(), "k1", "JSON object"),
            (r#"{"k1": 5}"#.to_string(), "k1", "JSON object"),
            ("{}".to_string(), "k1", "holds no key"),
            (format!(r#"{{"k1":"{short}"}}"#), "k1", "32 bytes, not 16"),
            (r#"{"k1":"%%%"}"#.to_string(), "k1", "not valid base64"),
            (
                format!(r#"{{"bad kid":"{good}"}}"#),
                "bad kid",
                "[A-Za-z0-9_-]{1,32}",
            ),
            (
                format!(r#"{{"{long_kid}":"{good}"}}"#),
                "k1",
                "[A-Za-z0-9_-]{1,32}",
            ),
            (
                format!(r#"{{"k1":"{good}"}}"#),
                "k2",
                "names a kid that is not in",
            ),
        ];
        for (keys, current, want) in cases {
            let e = KeySet::parse(p, Some(&keys), Some(current)).unwrap_err();
            assert!(e.contains(want), "{keys}: {e}");
            assert!(e.contains("TENANT_DATA_KEY"), "{e}");
            assert!(!e.contains(&good) && !e.contains(&short), "{e}");
        }
    }

    #[test]
    fn provider_secret_purpose_has_its_own_variables() {
        let p = Purpose::ProviderSecret;
        assert_eq!(p.keys_var(), "PROVIDER_SECRET_KEYS");
        assert_eq!(p.current_var(), "PROVIDER_SECRET_KEY_CURRENT");
        assert!(KeySet::parse(p, None, None).unwrap().is_none());
        let good = b64(4, 32);
        let keys = format!(r#"{{"p1":"{good}"}}"#);
        let s = KeySet::parse(p, Some(&keys), Some("p1")).unwrap().unwrap();
        let k = Keyring::new(p, &s);
        assert_eq!(k.purpose(), Purpose::ProviderSecret);
        let sealed = k.seal(SCOPE, b"sk-provider").unwrap();
        assert_eq!(k.open(SCOPE, &sealed).unwrap(), b"sk-provider");
        // another purpose's ring with other key bytes under the same kid never opens it
        let other = Keyring::new(Purpose::TenantData, &set("p1", &[("p1", 5)]));
        assert_eq!(other.open(SCOPE, &sealed), Err(KeyringError::Undecryptable));
    }

    #[test]
    fn provider_secret_bad_values_name_its_variables_never_the_key() {
        let p = Purpose::ProviderSecret;
        let good = b64(9, 32);
        let short = b64(9, 16);
        let e = KeySet::parse(p, Some(&format!(r#"{{"p1":"{good}"}}"#)), None).unwrap_err();
        assert!(e.contains("PROVIDER_SECRET_KEY_CURRENT"), "{e}");
        assert!(!e.contains(&good), "{e}");
        let e = KeySet::parse(p, Some(&format!(r#"{{"p1":"{short}"}}"#)), Some("p1")).unwrap_err();
        assert!(e.contains("PROVIDER_SECRET_KEYS"), "{e}");
        assert!(!e.contains(&short), "{e}");
        assert!(!e.contains("TENANT_DATA"), "{e}");
    }

    #[test]
    fn licence_token_purpose_has_its_own_variables_and_keys() {
        let p = Purpose::LicenceToken;
        assert_eq!(p.keys_var(), "LICENSE_TOKEN_ENC_KEYS");
        assert_eq!(p.current_var(), "LICENSE_TOKEN_ENC_KEY_CURRENT");
        assert!(KeySet::parse(p, None, None).unwrap().is_none());
        let e = KeySet::parse(p, Some(&format!(r#"{{"k1":"{}"}}"#, b64(1, 32))), None).unwrap_err();
        assert!(e.contains("LICENSE_TOKEN_ENC_KEY_CURRENT"), "{e}");
    }

    #[test]
    fn licence_and_tenant_data_key_sets_are_disjoint() {
        // the same kid name, different bytes: a licence blob does not open under TenantData
        let lic = Keyring::new(Purpose::LicenceToken, &set("k1", &[("k1", 1)]));
        let data = Keyring::new(Purpose::TenantData, &set("k1", &[("k1", 2)]));
        let sealed = lic.seal(SCOPE, b"lic-token").unwrap();
        assert_eq!(lic.open(SCOPE, &sealed).unwrap(), b"lic-token");
        assert_eq!(data.open(SCOPE, &sealed), Err(KeyringError::Undecryptable));
    }
}
