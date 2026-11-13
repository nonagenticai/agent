//! The known-customer web chat assertion (B7/B4 known-customer plan §1): a short-lived JWT the tenant's logged-in
//! site mints per widget open. Pure and deterministic: [`verify`] takes the clock as an argument and consumes
//! nothing (the single-use `jti` claim is the caller's job, in the session-insert transaction).
//!
//! The algorithm is pinned by the binding's `settings.identity.mode`; the token header is never trusted for it.
//! Every failure is one [`Refusal::Invalid`]; its reason is for an internal log line ONLY (the HTTP answer is always
//! `401 assertion_invalid`, no oracle). The assertion and its claims are customer data: never logged.
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jsonwebtoken::errors::ErrorKind;
use jsonwebtoken::{
    Algorithm, DecodingKey, EncodingKey, Header, Validation, decode, decode_header, encode,
};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};

/// The largest assertion the core will look at (bytes).
pub const MAX_ASSERTION_BYTES: usize = 2048;
/// The longest an assertion may live (`exp - iat`, seconds).
const MAX_LIFETIME_S: i64 = 300;
/// Clock leeway on `exp` and `iat` (seconds).
const LEEWAY_S: i64 = 30;
/// The identified-session cap when `session_hours` is absent.
const DEFAULT_SESSION_HOURS: u32 = 8;

/// How a binding verifies customer assertions. `off` is the default for every binding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdentityMode {
    #[default]
    Off,
    Hs256,
    Es256,
}

/// `settings.identity` of a web chat binding. Public data only: the HS256 secret lives in the sealed credential map.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IdentitySettings {
    pub mode: IdentityMode,
    pub issuer: String,
    /// 1..=24; 0 / absent = the default (8), see [`IdentitySettings::session_hours`].
    pub session_hours: u32,
    pub sign_in_url: Option<String>,
    /// ES256 only: up to 2 public P-256 JWKs, each with a `kid`.
    pub jwks: Vec<serde_json::Value>,
    pub prev_valid_until: Option<chrono::DateTime<chrono::Utc>>,
}

impl IdentitySettings {
    /// The identified-session cap in hours (default 8).
    pub fn session_hours(&self) -> u32 {
        if self.session_hours == 0 {
            DEFAULT_SESSION_HOURS
        } else {
            self.session_hours.min(24)
        }
    }
}

/// One HS256 key slot. `secret` is the full `ids_<b64url>` string; the HMAC key is its UTF-8 bytes.
pub struct HsKey {
    pub kid: String,
    pub secret: String,
}

/// A verified assertion. Nothing here has been consumed: the caller claims the `jti`.
pub struct Assertion {
    pub sub: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub jti: String,
    pub exp: i64,
}

// no PII in logs: the subject, email and name are redacted
impl std::fmt::Debug for Assertion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assertion")
            .field("sub", &"<redacted>")
            .field("email", &self.email.as_ref().map(|_| "<redacted>"))
            .field("name", &self.name.as_ref().map(|_| "<redacted>"))
            .field("exp", &self.exp)
            .finish_non_exhaustive()
    }
}

/// The one refusal. The reason is an internal log reason ONLY.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    Invalid(&'static str),
}

#[derive(Deserialize)]
struct Claims {
    sub: String,
    iat: i64,
    exp: i64,
    jti: String,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

fn invalid<T>(reason: &'static str) -> Result<T, Refusal> {
    Err(Refusal::Invalid(reason))
}

fn pinned_alg(mode: IdentityMode) -> Option<Algorithm> {
    match mode {
        IdentityMode::Off => None,
        IdentityMode::Hs256 => Some(Algorithm::HS256),
        IdentityMode::Es256 => Some(Algorithm::ES256),
    }
}

pub fn sub_ok(sub: &str) -> bool {
    !sub.is_empty()
        && sub.len() <= 200
        && sub
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'@' | b'-'))
}

/// Verify a customer assertion for `binding_id` at `now` (unix seconds). Pure: consumes nothing.
pub fn verify(
    s: &IdentitySettings,
    binding_id: uuid::Uuid,
    hs_keys: &[HsKey],
    token: &str,
    now: i64,
) -> Result<Assertion, Refusal> {
    let Some(alg) = pinned_alg(s.mode) else {
        return invalid("identity_off");
    };
    if token.is_empty() || token.len() > MAX_ASSERTION_BYTES {
        return invalid("token_size");
    }
    // The header is read only to pick the key slot and to check it names the pinned algorithm.
    let Ok(header) = decode_header(token) else {
        return invalid("header_malformed");
    };
    if header.alg != alg {
        return invalid("alg_mismatch");
    }
    let Some(kid) = header.kid.as_deref().filter(|k| !k.is_empty()) else {
        return invalid("kid_missing");
    };
    let key = match alg {
        Algorithm::HS256 => {
            let Some(k) = hs_keys.iter().find(|k| k.kid == kid) else {
                return invalid("kid_unknown");
            };
            DecodingKey::from_secret(k.secret.as_bytes())
        }
        _ => {
            let Some(jwk) = s
                .jwks
                .iter()
                .find(|j| j.get("kid").and_then(|v| v.as_str()) == Some(kid))
            else {
                return invalid("kid_unknown");
            };
            let (Some(x), Some(y)) = (
                jwk.get("x").and_then(|v| v.as_str()),
                jwk.get("y").and_then(|v| v.as_str()),
            ) else {
                return invalid("jwk_malformed");
            };
            match DecodingKey::from_ec_components(x, y) {
                Ok(k) => k,
                Err(_) => return invalid("jwk_malformed"),
            }
        }
    };
    let mut v = Validation::new(alg);
    v.set_audience(&[binding_id.to_string()]);
    v.set_issuer(&[s.issuer.as_str()]);
    v.set_required_spec_claims(&["exp", "iss", "aud"]);
    v.leeway = LEEWAY_S as u64;
    // `exp` is checked below against the caller's `now` (jsonwebtoken would read the wall clock itself, which
    // makes the check untestable and the function impure).
    v.validate_exp = false;
    let claims = match decode::<Claims>(token, &key, &v) {
        Ok(d) => d.claims,
        Err(e) => {
            return invalid(match e.kind() {
                ErrorKind::InvalidSignature => "bad_signature",
                ErrorKind::InvalidIssuer => "bad_issuer",
                ErrorKind::InvalidAudience => "bad_audience",
                ErrorKind::MissingRequiredClaim(_) => "claim_missing",
                ErrorKind::InvalidAlgorithm => "alg_mismatch",
                _ => "token_malformed",
            });
        }
    };
    if claims.exp.saturating_add(LEEWAY_S) < now {
        return invalid("expired");
    }
    if claims.iat > now.saturating_add(LEEWAY_S) {
        return invalid("iat_in_future");
    }
    let lifetime = claims.exp.saturating_sub(claims.iat);
    if lifetime <= 0 || lifetime > MAX_LIFETIME_S {
        return invalid("lifetime");
    }
    if !(16..=64).contains(&claims.jti.len()) || !claims.jti.bytes().all(|b| b.is_ascii_graphic()) {
        return invalid("jti_shape");
    }
    if !sub_ok(&claims.sub) {
        return invalid("sub_shape");
    }
    let email = claims.email.filter(|e| !e.is_empty());
    if email.as_ref().is_some_and(|e| e.chars().count() > 320) {
        return invalid("email_shape");
    }
    // `name` only feeds the greeting: an absurd one is dropped, not a reason to refuse a verified customer.
    let name = claims
        .name
        .filter(|n| !n.trim().is_empty() && n.chars().count() <= 200);
    Ok(Assertion {
        sub: claims.sub,
        email,
        name,
        jti: claims.jti,
        exp: claims.exp,
    })
}

/// Validate `settings.identity` on a binding settings PUT. The message is the 422 text.
pub fn validate_settings(s: &IdentitySettings, allowed_origins: &[String]) -> Result<(), String> {
    // the sign-in URL is returned to every visitor, so it is checked in EVERY mode (off included)
    if let Some(url) = s.sign_in_url.as_deref() {
        validate_sign_in_url(url, allowed_origins)?;
    }
    if s.mode == IdentityMode::Off {
        return Ok(());
    }
    if s.issuer.trim().is_empty() {
        return Err("identity.issuer is required".into());
    }
    if s.session_hours > 24 {
        return Err("identity.session_hours must be 1..24".into());
    }
    match s.mode {
        IdentityMode::Hs256 => {
            if !s.jwks.is_empty() {
                return Err("identity.jwks is for es256 only".into());
            }
        }
        _ => validate_jwks(&s.jwks)?,
    }
    Ok(())
}

/// The settings' sign-in URL, trimmed, only when it passes the same check as a settings PUT against the binding's
/// `allowed_origins` (a stored value that no longer validates is never handed to a visitor).
pub fn checked_sign_in_url(s: &IdentitySettings, allowed_origins: &[String]) -> Option<String> {
    let url = s.sign_in_url.as_deref()?.trim();
    (!url.is_empty() && validate_sign_in_url(url, allowed_origins).is_ok()).then(|| url.to_string())
}

fn validate_sign_in_url(url: &str, allowed_origins: &[String]) -> Result<(), String> {
    const BAD: &str =
        "identity.sign_in_url must be an https URL on an allowed origin, or a path starting with /";
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err(BAD.into());
    }
    if url.starts_with('/') {
        // A same-origin path; `//host` is a protocol-relative URL, not a path.
        return if url.starts_with("//") {
            Err(BAD.into())
        } else {
            Ok(())
        };
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(BAD.into());
    };
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    if authority.is_empty() || authority.contains('@') {
        return Err(BAD.into());
    }
    let origin = format!("https://{authority}");
    if allowed_origins
        .iter()
        .any(|o| o.trim().trim_end_matches('/').eq_ignore_ascii_case(&origin))
    {
        Ok(())
    } else {
        Err("identity.sign_in_url origin is not an allowed origin of this binding".into())
    }
}

fn validate_jwks(jwks: &[serde_json::Value]) -> Result<(), String> {
    if jwks.is_empty() || jwks.len() > 2 {
        return Err("identity.jwks must hold 1 or 2 public P-256 keys for es256".into());
    }
    let mut kids: Vec<&str> = Vec::new();
    for j in jwks {
        let text = |k: &str| j.get(k).and_then(|v| v.as_str()).filter(|v| !v.is_empty());
        if j.get("d").is_some() {
            return Err(
                "identity.jwks must hold public keys only (a private key member was supplied)"
                    .into(),
            );
        }
        if text("kty") != Some("EC") || text("crv") != Some("P-256") {
            return Err("identity.jwks keys must be kty EC, crv P-256".into());
        }
        let (Some(kid), Some(x), Some(y)) = (text("kid"), text("x"), text("y")) else {
            return Err("identity.jwks keys need kid, x and y".into());
        };
        // A P-256 coordinate is 32 bytes = 43 base64url characters.
        let coord_ok = |c: &str| {
            URL_SAFE_NO_PAD
                .decode(c)
                .map(|b| b.len() == 32)
                .unwrap_or(false)
        };
        if !coord_ok(x) || !coord_ok(y) {
            return Err("identity.jwks key coordinates must be 32-byte base64url".into());
        }
        if kids.contains(&kid) {
            return Err("identity.jwks kids must be unique".into());
        }
        kids.push(kid);
    }
    Ok(())
}

/// A fresh HS256 secret: `ids_` + base64url(32 random bytes). Shown once, then sealed.
pub fn generate_secret() -> String {
    let mut b = [0u8; 32];
    SystemRandom::new()
        .fill(&mut b)
        .expect("the system random source is available");
    format!("ids_{}", URL_SAFE_NO_PAD.encode(b))
}

#[derive(Serialize)]
struct MintClaims<'a> {
    iss: &'a str,
    aud: String,
    sub: &'a str,
    iat: i64,
    exp: i64,
    jti: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}

/// Mint an HS256 assertion the way a tenant's site would (the admin test session and the tests).
#[allow(clippy::too_many_arguments)]
pub fn mint_hs256(
    key: &HsKey,
    issuer: &str,
    binding_id: uuid::Uuid,
    sub: &str,
    email: Option<&str>,
    name: Option<&str>,
    now: i64,
    ttl_s: i64,
    jti: &str,
) -> String {
    let mut h = Header::new(Algorithm::HS256);
    h.kid = Some(key.kid.clone());
    let claims = MintClaims {
        iss: issuer,
        aud: binding_id.to_string(),
        sub,
        iat: now,
        exp: now + ttl_s,
        jti,
        email,
        name,
    };
    encode(
        &h,
        &claims,
        &EncodingKey::from_secret(key.secret.as_bytes()),
    )
    .expect("an HS256 assertion with string and integer claims always serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
    use serde_json::{Value, json};

    const NOW: i64 = 1_800_000_000;
    const ISS: &str = "https://shop.example";
    const JTI: &str = "0123456789abcdef";

    fn binding() -> uuid::Uuid {
        uuid::Uuid::from_u128(0xb1)
    }

    fn hs_key() -> HsKey {
        HsKey {
            kid: "k1".into(),
            secret: generate_secret(),
        }
    }

    fn hs_settings() -> IdentitySettings {
        IdentitySettings {
            mode: IdentityMode::Hs256,
            issuer: ISS.into(),
            ..Default::default()
        }
    }

    fn claims(binding: uuid::Uuid, iat: i64, exp: i64) -> Value {
        json!({"iss": ISS, "aud": binding.to_string(), "sub": "cust-1", "iat": iat, "exp": exp, "jti": JTI})
    }

    fn hs_raw(kid: &str, secret: &str, c: &Value) -> String {
        let mut h = Header::new(Algorithm::HS256);
        h.kid = Some(kid.into());
        encode(&h, c, &EncodingKey::from_secret(secret.as_bytes())).unwrap()
    }

    fn ok_hs_token(k: &HsKey) -> String {
        mint_hs256(
            k,
            ISS,
            binding(),
            "cust-1",
            Some("a@b.example"),
            Some("Ann"),
            NOW,
            300,
            JTI,
        )
    }

    fn assert_refused(r: Result<Assertion, Refusal>, why: &str) {
        match r {
            Err(Refusal::Invalid(_)) => {}
            Ok(a) => panic!("{why}: accepted {a:?}"),
        }
    }

    struct Es {
        pair: EcdsaKeyPair,
        jwk: Value,
    }

    fn es_key(kid: &str) -> Es {
        let rng = SystemRandom::new();
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
        let pair = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng)
            .unwrap();
        let pubkey = pair.public_key().as_ref().to_vec();
        assert_eq!(pubkey.len(), 65);
        assert_eq!(pubkey[0], 4);
        let jwk = json!({
            "kty": "EC", "crv": "P-256", "kid": kid,
            "x": URL_SAFE_NO_PAD.encode(&pubkey[1..33]),
            "y": URL_SAFE_NO_PAD.encode(&pubkey[33..65]),
        });
        Es { pair, jwk }
    }

    fn es_settings(keys: &[&Es]) -> IdentitySettings {
        IdentitySettings {
            mode: IdentityMode::Es256,
            issuer: ISS.into(),
            jwks: keys.iter().map(|k| k.jwk.clone()).collect(),
            ..Default::default()
        }
    }

    fn es_token(k: &Es, kid: &str, c: &Value) -> String {
        let h =
            URL_SAFE_NO_PAD.encode(json!({"alg": "ES256", "typ": "JWT", "kid": kid}).to_string());
        let p = URL_SAFE_NO_PAD.encode(c.to_string());
        let msg = format!("{h}.{p}");
        let sig = k.pair.sign(&SystemRandom::new(), msg.as_bytes()).unwrap();
        format!("{msg}.{}", URL_SAFE_NO_PAD.encode(sig.as_ref()))
    }

    #[test]
    fn assertion_verifies_and_refuses_uniformly() {
        let k = hs_key();
        let keys = [HsKey {
            kid: k.kid.clone(),
            secret: k.secret.clone(),
        }];
        let s = hs_settings();
        let a =
            verify(&s, binding(), &keys, &ok_hs_token(&k), NOW).expect("valid assertion verifies");
        assert_eq!(a.sub, "cust-1");
        assert_eq!(a.email.as_deref(), Some("a@b.example"));
        assert_eq!(a.name.as_deref(), Some("Ann"));
        assert_eq!(a.jti, JTI);
        assert_eq!(a.exp, NOW + 300);

        // bad signature: signed with another secret under the right kid
        let other = generate_secret();
        assert_refused(
            verify(
                &s,
                binding(),
                &keys,
                &hs_raw("k1", &other, &claims(binding(), NOW, NOW + 300)),
                NOW,
            ),
            "bad signature",
        );
        // tampered payload keeps the old signature
        let good = ok_hs_token(&k);
        let parts: Vec<&str> = good.split('.').collect();
        let forged = URL_SAFE_NO_PAD.encode(
            claims(binding(), NOW, NOW + 300)
                .to_string()
                .replace("cust-1", "cust-2"),
        );
        assert_refused(
            verify(
                &s,
                binding(),
                &keys,
                &format!("{}.{forged}.{}", parts[0], parts[2]),
                NOW,
            ),
            "tampered payload",
        );
        // header alg none
        let none_h =
            URL_SAFE_NO_PAD.encode(json!({"alg": "none", "typ": "JWT", "kid": "k1"}).to_string());
        let none_p = URL_SAFE_NO_PAD.encode(claims(binding(), NOW, NOW + 300).to_string());
        assert_refused(
            verify(&s, binding(), &keys, &format!("{none_h}.{none_p}."), NOW),
            "alg none",
        );
        // HS token on an ES binding, ES token on an HS binding
        let es = es_key("e1");
        assert_refused(
            verify(&es_settings(&[&es]), binding(), &keys, &good, NOW),
            "HS token on ES binding",
        );
        let es_tok = es_token(&es, "e1", &claims(binding(), NOW, NOW + 300));
        assert_refused(
            verify(&s, binding(), &keys, &es_tok, NOW),
            "ES token on HS binding",
        );
        // wrong iss
        let mut c = claims(binding(), NOW, NOW + 300);
        c["iss"] = json!("https://evil.example");
        assert_refused(
            verify(&s, binding(), &keys, &hs_raw("k1", &k.secret, &c), NOW),
            "wrong iss",
        );
        // wrong aud (a token for another binding)
        let other_binding = uuid::Uuid::from_u128(0xb2);
        assert_refused(
            verify(
                &s,
                binding(),
                &keys,
                &hs_raw("k1", &k.secret, &claims(other_binding, NOW, NOW + 300)),
                NOW,
            ),
            "wrong aud",
        );
        // aud / iss absent
        let mut c = claims(binding(), NOW, NOW + 300);
        c.as_object_mut().unwrap().remove("aud");
        assert_refused(
            verify(&s, binding(), &keys, &hs_raw("k1", &k.secret, &c), NOW),
            "no aud",
        );
        let mut c = claims(binding(), NOW, NOW + 300);
        c.as_object_mut().unwrap().remove("iss");
        assert_refused(
            verify(&s, binding(), &keys, &hs_raw("k1", &k.secret, &c), NOW),
            "no iss",
        );
        // exp past beyond leeway
        assert_refused(
            verify(
                &s,
                binding(),
                &keys,
                &hs_raw("k1", &k.secret, &claims(binding(), NOW - 400, NOW - 100)),
                NOW,
            ),
            "expired",
        );
        // exp - iat = 301
        assert_refused(
            verify(
                &s,
                binding(),
                &keys,
                &hs_raw("k1", &k.secret, &claims(binding(), NOW, NOW + 301)),
                NOW,
            ),
            "lifetime 301",
        );
        // iat 31 s in the future
        assert_refused(
            verify(
                &s,
                binding(),
                &keys,
                &hs_raw("k1", &k.secret, &claims(binding(), NOW + 31, NOW + 200)),
                NOW,
            ),
            "iat future",
        );
        // missing iat / exp
        for field in ["iat", "exp"] {
            let mut c = claims(binding(), NOW, NOW + 300);
            c.as_object_mut().unwrap().remove(field);
            assert_refused(
                verify(&s, binding(), &keys, &hs_raw("k1", &k.secret, &c), NOW),
                field,
            );
        }
        // missing jti, 15-char jti
        let mut c = claims(binding(), NOW, NOW + 300);
        c.as_object_mut().unwrap().remove("jti");
        assert_refused(
            verify(&s, binding(), &keys, &hs_raw("k1", &k.secret, &c), NOW),
            "no jti",
        );
        let mut c = claims(binding(), NOW, NOW + 300);
        c["jti"] = json!("0123456789abcde");
        assert_refused(
            verify(&s, binding(), &keys, &hs_raw("k1", &k.secret, &c), NOW),
            "15-char jti",
        );
        let mut c = claims(binding(), NOW, NOW + 300);
        c["jti"] = json!("x".repeat(65));
        assert_refused(
            verify(&s, binding(), &keys, &hs_raw("k1", &k.secret, &c), NOW),
            "65-char jti",
        );
        // sub: charset, empty, too long, missing
        for bad in ["cust 1", "cust/1", "", "é", "a".repeat(201).as_str()] {
            let mut c = claims(binding(), NOW, NOW + 300);
            c["sub"] = json!(bad);
            assert_refused(
                verify(&s, binding(), &keys, &hs_raw("k1", &k.secret, &c), NOW),
                "sub",
            );
        }
        let mut c = claims(binding(), NOW, NOW + 300);
        c.as_object_mut().unwrap().remove("sub");
        assert_refused(
            verify(&s, binding(), &keys, &hs_raw("k1", &k.secret, &c), NOW),
            "no sub",
        );
        // email over 320
        let mut c = claims(binding(), NOW, NOW + 300);
        c["email"] = json!(format!("{}@b.example", "a".repeat(320)));
        assert_refused(
            verify(&s, binding(), &keys, &hs_raw("k1", &k.secret, &c), NOW),
            "long email",
        );
        // unknown kid, missing kid
        assert_refused(
            verify(
                &s,
                binding(),
                &keys,
                &hs_raw("k9", &k.secret, &claims(binding(), NOW, NOW + 300)),
                NOW,
            ),
            "unknown kid",
        );
        let nokid = encode(
            &Header::new(Algorithm::HS256),
            &claims(binding(), NOW, NOW + 300),
            &EncodingKey::from_secret(k.secret.as_bytes()),
        )
        .unwrap();
        assert_refused(verify(&s, binding(), &keys, &nokid, NOW), "no kid");
        // oversize token (even if it would otherwise verify)
        let mut c = claims(binding(), NOW, NOW + 300);
        c["name"] = json!("n".repeat(MAX_ASSERTION_BYTES));
        let big = hs_raw("k1", &k.secret, &c);
        assert!(big.len() > MAX_ASSERTION_BYTES);
        assert_eq!(
            verify(&s, binding(), &keys, &big, NOW).unwrap_err(),
            Refusal::Invalid("token_size")
        );
        // garbage and empty
        assert_refused(verify(&s, binding(), &keys, "", NOW), "empty");
        assert_refused(verify(&s, binding(), &keys, "not.a.jwt", NOW), "garbage");
        // mode off refuses everything
        assert_refused(
            verify(&IdentitySettings::default(), binding(), &keys, &good, NOW),
            "mode off",
        );
        // no key slot at all
        assert_refused(verify(&s, binding(), &[], &good, NOW), "no keys");
    }

    #[test]
    fn leeway_edges() {
        let k = hs_key();
        let keys = [HsKey {
            kid: k.kid.clone(),
            secret: k.secret.clone(),
        }];
        let s = hs_settings();
        let check = |iat: i64, exp: i64| {
            verify(
                &s,
                binding(),
                &keys,
                &hs_raw("k1", &k.secret, &claims(binding(), iat, exp)),
                NOW,
            )
        };
        // exp exactly 30 s past: still inside the leeway; 31 s past: refused
        assert!(check(NOW - 330, NOW - 30).is_ok());
        assert!(check(NOW - 331, NOW - 31).is_err());
        // iat exactly 30 s ahead: ok; 31 s ahead: refused
        assert!(check(NOW + 30, NOW + 330).is_ok());
        assert!(check(NOW + 31, NOW + 331).is_err());
        // lifetime 300 ok, 301 refused, 0 / negative refused
        assert!(check(NOW, NOW + 300).is_ok());
        assert!(check(NOW, NOW + 301).is_err());
        assert!(check(NOW, NOW).is_err());
        assert!(check(NOW, NOW - 5).is_err());
        // jti 16 and 64 ok
        for n in [16usize, 64] {
            let mut c = claims(binding(), NOW, NOW + 300);
            c["jti"] = json!("j".repeat(n));
            assert!(
                verify(&s, binding(), &keys, &hs_raw("k1", &k.secret, &c), NOW).is_ok(),
                "jti {n}"
            );
        }
        // sub 200 ok; email 320 ok
        let mut c = claims(binding(), NOW, NOW + 300);
        c["sub"] = json!("s".repeat(200));
        c["email"] = json!("e".repeat(320));
        assert!(verify(&s, binding(), &keys, &hs_raw("k1", &k.secret, &c), NOW).is_ok());
    }

    #[test]
    fn es256_verifies() {
        let a = es_key("e1");
        let b = es_key("e2");
        let s = es_settings(&[&a, &b]);
        let mut c = claims(binding(), NOW, NOW + 300);
        c["email"] = json!("a@b.example");
        let got = verify(&s, binding(), &[], &es_token(&a, "e1", &c), NOW).expect("ES256 verifies");
        assert_eq!(got.sub, "cust-1");
        assert_eq!(got.email.as_deref(), Some("a@b.example"));
        // the second slot verifies too
        assert!(verify(&s, binding(), &[], &es_token(&b, "e2", &c), NOW).is_ok());
        // right kid, wrong key
        assert!(verify(&s, binding(), &[], &es_token(&b, "e1", &c), NOW).is_err());
        // unknown kid
        assert!(verify(&s, binding(), &[], &es_token(&a, "e9", &c), NOW).is_err());
        // wrong binding
        let other = uuid::Uuid::from_u128(0xb2);
        assert!(verify(&s, other, &[], &es_token(&a, "e1", &c), NOW).is_err());
        // a rotated-out key (only b configured) no longer verifies a
        assert!(
            verify(
                &es_settings(&[&b]),
                binding(),
                &[],
                &es_token(&a, "e1", &c),
                NOW
            )
            .is_err()
        );
    }

    #[test]
    fn alg_is_pinned_per_binding() {
        let k = hs_key();
        let keys = [HsKey {
            kid: k.kid.clone(),
            secret: k.secret.clone(),
        }];
        let es = es_key("k1");
        let tok = ok_hs_token(&k);
        // the same token: fine on an HS256 binding, dead on an ES256 binding, dead when off
        assert!(verify(&hs_settings(), binding(), &keys, &tok, NOW).is_ok());
        assert_eq!(
            verify(&es_settings(&[&es]), binding(), &keys, &tok, NOW).unwrap_err(),
            Refusal::Invalid("alg_mismatch")
        );
        assert!(verify(&IdentitySettings::default(), binding(), &keys, &tok, NOW).is_err());
        // classic confusion: an HS256 token keyed with the ES public key bytes, on the ES binding
        let pubkey_secret = es.jwk["x"].as_str().unwrap().to_string();
        let confused = hs_raw("k1", &pubkey_secret, &claims(binding(), NOW, NOW + 300));
        assert!(verify(&es_settings(&[&es]), binding(), &keys, &confused, NOW).is_err());
        // an ES token on the HS binding
        let es_tok = es_token(&es, "k1", &claims(binding(), NOW, NOW + 300));
        assert_eq!(
            verify(&hs_settings(), binding(), &keys, &es_tok, NOW).unwrap_err(),
            Refusal::Invalid("alg_mismatch")
        );
    }

    #[test]
    fn validate_settings_rules() {
        let origins = vec![
            "https://shop.example".to_string(),
            "https://app.example:8443".to_string(),
        ];
        let es = es_key("e1");

        // off is ok whatever else is set, except that a sign-in URL is checked in every mode
        let off = IdentitySettings {
            session_hours: 99,
            ..Default::default()
        };
        assert!(validate_settings(&off, &origins).is_ok());
        let off_bad = IdentitySettings {
            sign_in_url: Some("http://nope".into()),
            ..Default::default()
        };
        assert!(validate_settings(&off_bad, &origins).is_err());
        let off_ok = IdentitySettings {
            sign_in_url: Some("/login".into()),
            ..Default::default()
        };
        assert!(validate_settings(&off_ok, &origins).is_ok());
        assert_eq!(
            checked_sign_in_url(&off_ok, &origins).as_deref(),
            Some("/login")
        );
        assert_eq!(checked_sign_in_url(&off_bad, &origins), None);

        // the Debug of an assertion carries no PII
        let a = Assertion {
            sub: "cust-secret-1".into(),
            email: Some("a@b.example".into()),
            name: Some("Ada Lovelace".into()),
            jti: "j".into(),
            exp: 1,
        };
        let dbg = format!("{a:?}");
        assert!(
            !dbg.contains("cust-secret-1") && !dbg.contains("a@b.example") && !dbg.contains("Ada")
        );

        // hs256
        assert!(validate_settings(&hs_settings(), &origins).is_ok());
        let mut s = hs_settings();
        s.issuer = "  ".into();
        assert!(validate_settings(&s, &origins).is_err());
        for h in [0u32, 1, 8, 24] {
            let mut s = hs_settings();
            s.session_hours = h;
            assert!(validate_settings(&s, &origins).is_ok(), "hours {h}");
        }
        let mut s = hs_settings();
        s.session_hours = 25;
        assert!(validate_settings(&s, &origins).is_err());
        let mut s = hs_settings();
        s.jwks = vec![es.jwk.clone()];
        assert!(
            validate_settings(&s, &origins).is_err(),
            "hs256 must have empty jwks"
        );

        // sign_in_url
        for ok in [
            "https://shop.example/login",
            "https://shop.example",
            "https://app.example:8443/in?next=%2Fchat",
            "https://SHOP.example/login",
            "/login",
            "/account/sign-in?x=1",
        ] {
            let mut s = hs_settings();
            s.sign_in_url = Some(ok.into());
            assert!(validate_settings(&s, &origins).is_ok(), "{ok}");
        }
        for bad in [
            "http://shop.example/login",
            "https://evil.example/login",
            "https://shop.example.evil.example/",
            "https://shop.example@evil.example/",
            "https://evil.example#@shop.example",
            "//evil.example/login",
            "login",
            "javascript:alert(1)",
            "https://",
            "/a b",
            "/\\evil.example",
            "",
        ] {
            let mut s = hs_settings();
            s.sign_in_url = Some(bad.into());
            assert!(validate_settings(&s, &origins).is_err(), "{bad}");
        }

        // es256
        assert!(validate_settings(&es_settings(&[&es]), &origins).is_ok());
        let es2 = es_key("e2");
        assert!(validate_settings(&es_settings(&[&es, &es2]), &origins).is_ok());
        let es3 = es_key("e3");
        assert!(
            validate_settings(&es_settings(&[&es, &es2, &es3]), &origins).is_err(),
            "3 keys"
        );
        assert!(
            validate_settings(&es_settings(&[]), &origins).is_err(),
            "no keys"
        );
        let mut dup = es_settings(&[&es]);
        dup.jwks.push(es.jwk.clone());
        assert!(validate_settings(&dup, &origins).is_err(), "duplicate kid");
        let with = |f: &dyn Fn(&mut Value)| {
            let mut s = es_settings(&[&es]);
            f(&mut s.jwks[0]);
            validate_settings(&s, &origins)
        };
        assert!(
            with(&|j| j["d"] = json!("AAAA")).is_err(),
            "private key refused"
        );
        assert!(with(&|j| j["kty"] = json!("RSA")).is_err());
        assert!(with(&|j| j["crv"] = json!("P-384")).is_err());
        assert!(with(&|j| drop(j.as_object_mut().unwrap().remove("kid"))).is_err());
        assert!(with(&|j| drop(j.as_object_mut().unwrap().remove("x"))).is_err());
        assert!(with(&|j| drop(j.as_object_mut().unwrap().remove("y"))).is_err());
        assert!(with(&|j| j["x"] = json!("short")).is_err());
        let mut s = es_settings(&[&es]);
        s.issuer = String::new();
        assert!(validate_settings(&s, &origins).is_err());
    }

    #[test]
    fn generate_secret_shape() {
        let a = generate_secret();
        let b = generate_secret();
        assert_ne!(a, b);
        for s in [&a, &b] {
            let body = s.strip_prefix("ids_").expect("ids_ prefix");
            assert_eq!(body.len(), 43, "32 bytes in unpadded base64url");
            assert!(
                body.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
            );
            assert_eq!(URL_SAFE_NO_PAD.decode(body).unwrap().len(), 32);
        }
        // session_hours default
        assert_eq!(IdentitySettings::default().session_hours(), 8);
        let s = IdentitySettings {
            session_hours: 3,
            ..IdentitySettings::default()
        };
        assert_eq!(s.session_hours(), 3);
        // serde: lowercase mode, tolerant defaults
        let parsed: IdentitySettings =
            serde_json::from_value(json!({"mode": "es256", "issuer": "x"})).unwrap();
        assert_eq!(parsed.mode, IdentityMode::Es256);
        assert!(
            serde_json::from_value::<IdentitySettings>(json!({}))
                .unwrap()
                .jwks
                .is_empty()
        );
    }
}
