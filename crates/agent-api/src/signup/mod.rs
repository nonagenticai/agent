//! Early-access signup (owner 2026-10-19, D73; early-access v2 guide contracts C1–C6). Ported from
//! strata_configurator `signup.rs` (requester-typed text, the default workspace name, Turnstile) with the
//! commonpractice v2 deltas: the decider guard (`middleware/signup-decider-auth.ts`), the locale
//! (`utils/signup-locale.ts`), Telegram with names and buttons (crate::signup::telegram) and the journaled,
//! tick-retried approval (crate::signup::provisioning).
//!
//! Routes (crate::routes::signups) are mounted only when `EARLY_ACCESS_SIGNUP_ENABLED` is on.
pub mod freemail;
pub mod provisioning;
pub mod renotify;
pub mod signals;
pub mod store;
pub mod telegram;

use std::time::Duration;

use crate::auth::{SUPER_ADMIN, ServiceClaims};
use crate::config::SignupConfig;
use crate::error::ApiError;

/// The languages a signup can carry (commonpractice 0121; the `signup_approvals.locale` CHECK; contract C6).
pub const SIGNUP_LOCALES: [&str; 8] = ["ar", "de", "en", "es", "fr", "he", "nl", "pl"];
pub const DEFAULT_LOCALE: &str = "en";

/// A caller-supplied language tag → one of [`SIGNUP_LOCALES`], or `None`. Lenient on purpose (commonpractice
/// `normaliseSignupLocale`): the intake relays a browser's `<html lang>`, so `de-DE`, `DE` and `de_AT` mean
/// German, and an unknown language is "not told", never a refused signup.
pub fn normalise_locale(v: Option<&str>) -> Option<&'static str> {
    let v = v?.trim().to_ascii_lowercase();
    let primary = v.split(['-', '_']).next().unwrap_or_default();
    SIGNUP_LOCALES.iter().copied().find(|l| *l == primary)
}

fn forbidden(code: &'static str, why: &str) -> ApiError {
    ApiError::Forbidden(code, why.into())
}

/// The service-account door shared by the intake and the decider (commonpractice
/// `signup-intake-auth.ts` / `signup-decider-auth.ts`): a token carrying `super_admin` is refused (an
/// operator session never poses as a service), `azp` must be one of `allowed`, `aud` must contain that
/// `azp`, and the narrow `role` must be held. The token must also come from `issuer`, the ONE issuer this door
/// accepts its client from (`SIGNUP_*_ISSUER`): a same-named client in any other trusted realm never stands in for
/// it. 403 `code` otherwise.
pub(crate) fn require_service(
    c: &ServiceClaims,
    issuer: &str,
    super_admin_role: &str,
    allowed: &[String],
    role: &str,
    code: &'static str,
    what: &str,
) -> Result<String, ApiError> {
    if c.iss != issuer {
        return Err(forbidden(
            code,
            &format!("the token was not issued by the realm the {what} accepts its client from"),
        ));
    }
    // Refused wherever it sits (realm or any client); the door role below counts only as a REALM role.
    if c.has_anywhere(SUPER_ADMIN) || c.has_anywhere(super_admin_role) {
        return Err(forbidden(
            code,
            &format!(
                "the {what} accepts only its dedicated service-account token, never an operator session"
            ),
        ));
    }
    let Some(azp) = c.azp.as_deref().filter(|a| allowed.iter().any(|x| x == a)) else {
        return Err(forbidden(
            code,
            &format!("this client is not allowed to use the {what}"),
        ));
    };
    if !c.aud.iter().any(|a| a == azp) {
        return Err(forbidden(
            code,
            &format!("the token is not addressed to the {what}"),
        ));
    }
    if role.is_empty() || !c.has(role) {
        return Err(forbidden(
            code,
            &format!("role '{role}' is required for the {what}"),
        ));
    }
    Ok(azp.to_string())
}

/// The intake door: the website / chat intake handler's client-credentials token. Returns its `azp`.
pub fn require_intake(cfg: &SignupConfig, c: &ServiceClaims) -> Result<String, ApiError> {
    require_service(
        c,
        &cfg.intake_issuer,
        &cfg.super_admin_role,
        &cfg.intake_azp,
        &cfg.intake_role,
        "signup_intake_forbidden",
        "signup intake",
    )
}

/// The Telegram decision door (C3): chat-bridge's `chat-bridge-signup-decider` token. Returns its `azp`.
/// Deliberately not the intake client: the website SA can queue signups and nothing else.
pub fn require_decider(cfg: &SignupConfig, c: &ServiceClaims) -> Result<String, ApiError> {
    require_service(
        c,
        &cfg.decider_issuer,
        &cfg.super_admin_role,
        &cfg.decider_azp,
        &cfg.decider_role,
        "signup_decider_forbidden",
        "signup decision door",
    )
}

/// Unicode format characters (general category Cf): bidi embeddings, overrides and isolates, LRM/RLM,
/// zero-width characters, the BOM, the soft hyphen and the tag characters. Invisible by design, so
/// requester-typed text drops them.
fn is_format(c: char) -> bool {
    matches!(c as u32,
        0x00AD | 0x0600..=0x0605 | 0x061C | 0x06DD | 0x070F | 0x0890..=0x0891 | 0x08E2 | 0x180E
        | 0x200B..=0x200F | 0x202A..=0x202E | 0x2060..=0x2064 | 0x2066..=0x206F | 0xFEFF
        | 0xFFF9..=0xFFFB | 0x110BD | 0x110CD | 0x13430..=0x1343F | 0x1BCA0..=0x1BCA3
        | 0x1D173..=0x1D17A | 0xE0001 | 0xE0020..=0xE007F)
}

/// U+2028 LINE SEPARATOR and U+2029 PARAGRAPH SEPARATOR.
fn is_separator(c: char) -> bool {
    matches!(c, '\u{2028}' | '\u{2029}')
}

/// One printable line (commonpractice `singleLine`): control characters and line/paragraph separators
/// become spaces, format characters go, whitespace collapses to one space, and the ends are trimmed.
pub fn single_line(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut space = false;
    for c in v.chars() {
        if is_format(c) {
            continue;
        }
        if c.is_control() || is_separator(c) || c.is_whitespace() {
            space = true;
            continue;
        }
        if space && !out.is_empty() {
            out.push(' ');
        }
        space = false;
        out.push(c);
    }
    out
}

/// Printable text keeping its line breaks (commonpractice `multiLine`): CRLF, CR and the line/paragraph
/// separators become `\n`, a tab a space, every other control and format character goes, and the ends
/// are trimmed.
pub fn multi_line(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    let mut chars = v.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' => out.push('\n'),
            '\t' => out.push(' '),
            c if is_separator(c) => out.push('\n'),
            c if c.is_control() || is_format(c) => {}
            c => out.push(c),
        }
    }
    out.trim().to_string()
}

fn invalid(msg: String) -> ApiError {
    ApiError::BadRequest("invalid_body", msg)
}

fn too_long(field: &str, max: usize) -> ApiError {
    invalid(format!("{field} must be at most {max} characters"))
}

/// A requester-typed one-line field: [`single_line`], then 1 to `max` characters (the raw input is
/// bounded at four times that first). Empty after stripping is refused.
pub fn single_line_field(field: &str, v: &str, max: usize) -> Result<String, ApiError> {
    if v.chars().count() > max * 4 {
        return Err(too_long(field, max));
    }
    let t = single_line(v);
    if t.is_empty() {
        return Err(invalid(format!("{field} must not be empty")));
    }
    if t.chars().count() > max {
        return Err(too_long(field, max));
    }
    Ok(t)
}

/// A requester-typed multi-line field: [`multi_line`], at most `max` characters; empty after stripping is
/// `None` (as if not given).
pub fn multi_line_field(field: &str, v: &str, max: usize) -> Result<Option<String>, ApiError> {
    if v.chars().count() > max * 4 {
        return Err(too_long(field, max));
    }
    let t = multi_line(v);
    if t.chars().count() > max {
        return Err(too_long(field, max));
    }
    Ok(Some(t).filter(|t| !t.is_empty()))
}

const MAX_EMAIL: usize = 254;

/// `v` as an email address: trimmed, no control characters or whitespace, at most 254 characters, one `@`
/// with a non-empty local part and a dotted domain. Keycloak checks the rest.
pub fn email_address(field: &str, v: &str) -> Result<String, ApiError> {
    let e = v.trim();
    let ok = !e.is_empty()
        && e.chars().count() <= MAX_EMAIL
        && !e.chars().any(|c| c.is_control() || c.is_whitespace())
        && e.matches('@').count() == 1
        && e.split_once('@').is_some_and(|(local, domain)| {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
        });
    if !ok {
        return Err(invalid(format!("{field} must be an email address")));
    }
    Ok(e.to_string())
}

/// A workspace name (approval `tenantName`): trimmed, no control characters, 2 to 120 characters.
pub fn tenant_name(v: &str) -> Result<String, ApiError> {
    let t = v.trim();
    if t.chars().any(char::is_control) {
        return Err(invalid(
            "tenantName must not contain control characters".into(),
        ));
    }
    let n = t.chars().count();
    if !(2..=120).contains(&n) {
        return Err(invalid("tenantName must be 2 to 120 characters".into()));
    }
    Ok(t.to_string())
}

/// Public mailbox providers, which never name a customer (commonpractice `PUBLIC_EMAIL_DOMAINS`).
pub const PUBLIC_EMAIL_DOMAINS: &[&str] = &[
    "gmail.com",
    "googlemail.com",
    "outlook.com",
    "hotmail.com",
    "hotmail.co.uk",
    "live.com",
    "msn.com",
    "yahoo.com",
    "yahoo.co.uk",
    "ymail.com",
    "icloud.com",
    "me.com",
    "mac.com",
    "aol.com",
    "proton.me",
    "protonmail.com",
    "pm.me",
    "gmx.com",
    "gmx.de",
    "gmx.net",
    "web.de",
    "mail.com",
    "zoho.com",
    "yandex.com",
    "yandex.ru",
    "hey.com",
    "fastmail.com",
    "tutanota.com",
    "tuta.io",
    "qq.com",
    "163.com",
    "ziggo.nl",
    "kpnmail.nl",
    "hetnet.nl",
    "planet.nl",
    "telenet.be",
    "orange.fr",
    "free.fr",
    "laposte.net",
    "t-online.de",
];

/// Second-level labels under a country TLD (`acme.co.uk`).
const SECOND_LEVEL_LABELS: [&str; 9] =
    ["co", "com", "org", "net", "ac", "gov", "edu", "ltd", "plc"];

/// `mail.acme.co.uk` → `acme` (commonpractice `organisationLabel`, no public-suffix list).
pub fn organisation_label(domain: &str) -> String {
    let lower = domain.trim().to_lowercase();
    let mut labels: Vec<&str> = lower.split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() <= 1 {
        return labels.first().map(|l| l.to_string()).unwrap_or_default();
    }
    labels.pop();
    if labels.len() > 1
        && labels
            .last()
            .is_some_and(|l| SECOND_LEVEL_LABELS.contains(l))
    {
        labels.pop();
    }
    labels.last().map(|l| l.to_string()).unwrap_or_default()
}

/// A usable workspace name: trimmed, at most 120 characters, at least 2, with a letter or digit.
fn usable(v: Option<&str>) -> Option<String> {
    let t: String = v?.trim().chars().take(120).collect();
    let t = t.trim();
    (t.chars().count() >= 2 && t.chars().any(|c| c.is_ascii_alphanumeric())).then(|| t.to_string())
}

/// The workspace name an approval uses when none is given (commonpractice `defaultTenantNameFrom`): the
/// company; else the organisation of the email domain, unless it is a public mailbox provider; else the
/// requester's name; else the email's local part.
pub fn default_tenant_name(company: Option<&str>, email: &str, name: Option<&str>) -> String {
    if let Some(c) = usable(company) {
        return c;
    }
    let at = email.rfind('@');
    if let Some(i) = at {
        let domain = email[i + 1..].trim().to_lowercase();
        if !domain.is_empty() && !PUBLIC_EMAIL_DOMAINS.contains(&domain.as_str()) {
            let label = organisation_label(&domain);
            if let Some(l) = usable(Some(label.as_str())) {
                return l;
            }
        }
    }
    if let Some(n) = usable(name) {
        return n;
    }
    usable(Some(&email[..at.unwrap_or(0)])).unwrap_or_else(|| "workspace".into())
}

/// The signup settings (the Keycloak and licences clients are `AppState.identity`, crate::identity).
pub struct Signup {
    pub cfg: SignupConfig,
    pub http: reqwest::Client,
}

impl Signup {
    pub fn new(cfg: SignupConfig, http: reqwest::Client) -> Self {
        Self { cfg, http }
    }

    /// One line at boot: which optional parts are on.
    pub fn log_status(&self) {
        tracing::info!(
            turnstile = self.cfg.turnstile_secret.is_some(),
            telegram = self.cfg.telegram.is_some(),
            telegram_missing = ?self.cfg.telegram_missing,
            "early_access_signup_enabled"
        );
    }

    /// Verify a relayed Turnstile token (commonpractice `enforceTurnstile` with `requireToken: false`: the
    /// intake is a server, not a browser). No secret: skipped ([`TurnstileOutcome::NotConfigured`]); no token:
    /// skipped ([`TurnstileOutcome::Absent`]). siteverify unreachable or non-2xx → 503 `turnstile_unavailable`; a
    /// token it does not accept → 403 `turnstile_failed`; one it accepts → [`TurnstileOutcome::Verified`].
    pub async fn verify_turnstile(
        &self,
        token: Option<&str>,
    ) -> Result<TurnstileOutcome, ApiError> {
        let Some(secret) = self.cfg.turnstile_secret.as_deref() else {
            return Ok(TurnstileOutcome::NotConfigured);
        };
        let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
            return Ok(TurnstileOutcome::Absent);
        };
        let check = crate::turnstile::Check {
            url: self.cfg.turnstile_verify_url.as_str(),
            timeout: Duration::from_millis(self.cfg.turnstile_timeout_ms),
            secret,
            token,
            remote_ip: None,
            action: None,
            hostname: None,
        };
        match crate::turnstile::verify(&self.http, &check).await {
            Ok(()) => {}
            Err(crate::turnstile::Failure::Unavailable) => {
                return Err(ApiError::Unavailable(
                    "turnstile_unavailable",
                    "sign-up verification is temporarily unavailable; try again shortly".into(),
                    None,
                ));
            }
            Err(crate::turnstile::Failure::Rejected) => {
                return Err(ApiError::Forbidden(
                    "turnstile_failed",
                    "the verification challenge was not accepted".into(),
                ));
            }
        }
        Ok(TurnstileOutcome::Verified)
    }
}

/// What the intake's Turnstile check found (B1b T1, F-03), recorded as `details.turnstile` of the `signup.intake`
/// platform audit row and read back as the `turnstile` intake signal (crate::signup::signals).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TurnstileOutcome {
    /// A relayed token siteverify accepted.
    Verified,
    /// No `TURNSTILE_SECRET`: nothing was checked.
    NotConfigured,
    /// A secret is set but the intake relayed no token.
    Absent,
}

impl TurnstileOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            TurnstileOutcome::Verified => "verified",
            TurnstileOutcome::NotConfigured => "not_configured",
            TurnstileOutcome::Absent => "absent",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORTAL: &str = "https://kc.test/realms/portal";
    const SECURE_APPS: &str = "https://kc.test/realms/secure-apps";

    fn claims(azp: Option<&str>, aud: &[&str], roles: &[&str]) -> ServiceClaims {
        ServiceClaims {
            iss: PORTAL.into(),
            sub: "sa".into(),
            azp: azp.map(String::from),
            aud: aud.iter().map(|s| s.to_string()).collect(),
            roles: roles.iter().map(|s| s.to_string()).collect(),
            client_roles: vec![],
        }
    }

    #[test]
    fn service_doors_check_azp_aud_role_and_refuse_super_admin() {
        let allowed = vec!["agent-signup-intake".to_string()];
        let role = "agent-signup-intake";
        let ok = claims(
            Some("agent-signup-intake"),
            &["agent-signup-intake"],
            &[role],
        );
        assert_eq!(
            require_service(&ok, PORTAL, "super_admin", &allowed, role, "x", "door").unwrap(),
            "agent-signup-intake"
        );
        for bad in [
            claims(
                Some("agent-signup-intake"),
                &["agent-signup-intake"],
                &[role, "super_admin"],
            ),
            claims(Some("cp-signup-intake"), &["cp-signup-intake"], &[role]),
            claims(None, &["agent-signup-intake"], &[role]),
            claims(Some("agent-signup-intake"), &["agent"], &[role]),
            claims(
                Some("agent-signup-intake"),
                &["agent-signup-intake"],
                &["other"],
            ),
            // the door role held only as a CLIENT role
            ServiceClaims {
                client_roles: vec![role.to_string()],
                ..claims(Some("agent-signup-intake"), &["agent-signup-intake"], &[])
            },
            // super_admin as a client role is still an operator
            ServiceClaims {
                client_roles: vec!["super_admin".to_string()],
                ..ok.clone()
            },
        ] {
            let e = require_service(
                &bad,
                PORTAL,
                "super_admin",
                &allowed,
                role,
                "signup_intake_forbidden",
                "door",
            )
            .unwrap_err();
            assert_eq!(e.code(), "signup_intake_forbidden");
        }
    }

    /// Realm per app A1: a door pinned to secure-apps accepts a secure-apps service token and refuses the same client
    /// from portal (and vice versa); the default pin is the portal issuer.
    #[test]
    fn service_doors_accept_only_their_pinned_issuer() {
        let allowed = vec!["agent-signup-intake".to_string()];
        let role = "agent-signup-intake";
        let from = |iss: &str| ServiceClaims {
            iss: iss.into(),
            ..claims(
                Some("agent-signup-intake"),
                &["agent-signup-intake"],
                &[role],
            )
        };
        let door = |c: &ServiceClaims, pin: &str| {
            require_service(
                c,
                pin,
                "super_admin",
                &allowed,
                role,
                "signup_intake_forbidden",
                "door",
            )
        };
        assert!(door(&from(SECURE_APPS), SECURE_APPS).is_ok());
        let e = door(&from(PORTAL), SECURE_APPS).unwrap_err();
        assert_eq!(e.code(), "signup_intake_forbidden");
        assert!(door(&from(PORTAL), PORTAL).is_ok());
        let e = door(&from(SECURE_APPS), PORTAL).unwrap_err();
        assert_eq!(e.code(), "signup_intake_forbidden");
    }

    #[test]
    fn locales_are_lenient_and_closed() {
        assert_eq!(normalise_locale(Some("de-DE")), Some("de"));
        assert_eq!(normalise_locale(Some(" NL ")), Some("nl"));
        assert_eq!(normalise_locale(Some("pt_BR")), None);
        assert_eq!(normalise_locale(Some("")), None);
        assert_eq!(normalise_locale(None), None);
    }

    #[test]
    fn single_line_strips_controls_and_format_characters() {
        assert_eq!(single_line("  Ann\nEvil\r\n Corp  "), "Ann Evil Corp");
        assert_eq!(single_line("A\u{202E}nn\u{200B}"), "Ann");
        assert_eq!(single_line("a\u{2028}b\u{00A0}\u{3000}c\td"), "a b c d");
        assert!(single_line_field("name", "\u{200B} \n", 160).is_err());
        assert!(single_line_field("name", &"x".repeat(161), 160).is_err());
    }

    #[test]
    fn multi_line_keeps_line_breaks_only() {
        assert_eq!(
            multi_line(" Hi\r\nthere\rand\u{2029}more\tok\u{0007}\u{202E} "),
            "Hi\nthere\nand\nmore ok"
        );
        assert_eq!(
            multi_line_field("message", " \u{200B} ", 2000).unwrap(),
            None
        );
        assert!(multi_line_field("message", &"x".repeat(2001), 2000).is_err());
    }

    #[test]
    fn emails_and_names_validate() {
        assert_eq!(email_address("email", " a@b.co ").unwrap(), "a@b.co");
        for bad in [
            "", "a", "a@b", "@b.co", "a@@b.co", "a b@c.co", "a@.co", "a@co.",
        ] {
            assert!(email_address("email", bad).is_err(), "{bad:?}");
        }
        assert!(tenant_name("x").is_err());
        assert!(tenant_name("a\u{7}b").is_err());
        assert_eq!(tenant_name(" ab ").unwrap(), "ab");
    }

    #[test]
    fn default_names_follow_commonpractice() {
        assert_eq!(
            default_tenant_name(Some("Globex"), "ann@globex.com", Some("Ann")),
            "Globex"
        );
        assert_eq!(
            default_tenant_name(None, "ann@mail.acme.co.uk", Some("Ann")),
            "acme"
        );
        assert_eq!(
            default_tenant_name(Some(" "), "ann.early@gmail.com", Some("Ann Early")),
            "Ann Early"
        );
        assert_eq!(
            default_tenant_name(None, "ann.early@gmail.com", Some("A")),
            "ann.early"
        );
        assert_eq!(default_tenant_name(None, "a@gmail.com", None), "workspace");
    }
}
