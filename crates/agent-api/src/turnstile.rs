//! Cloudflare Turnstile `siteverify`, shared by the early-access intake (crate::signup) and web chat session creation
//! (crate::conversations::webchat_sessions, B14 §2). One POST, form-encoded, with a timeout; the caller maps the
//! outcome to its own answer. Neither the secret nor the token is ever logged.
use std::time::Duration;

use serde::Deserialize;

use crate::service_token::form;

/// The public siteverify endpoint (`TURNSTILE_VERIFY_URL` overrides it).
pub const DEFAULT_VERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

/// Why a token was not verified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// siteverify unreachable, timed out, non-2xx or an unreadable body: callers fail closed (503).
    Unavailable,
    /// siteverify answered and did not accept the token (or it was minted for another action): 403.
    Rejected,
}

/// One siteverify call.
pub struct Check<'a> {
    pub url: &'a str,
    pub timeout: Duration,
    pub secret: &'a str,
    pub token: &'a str,
    /// The client's IP (`remoteip`), when known.
    pub remote_ip: Option<&'a str>,
    /// The `action` the widget must have rendered with; `None` accepts any.
    pub action: Option<&'a str>,
    /// The page host the widget must have been solved on (siteverify `hostname`, compared case-insensitively);
    /// `None` accepts any. A site key shared across tenants (platform mode) is only as narrow as this check.
    pub hostname: Option<&'a str>,
}

#[derive(Deserialize)]
struct SiteVerify {
    success: Option<bool>,
    action: Option<String>,
    hostname: Option<String>,
}

/// Verify `c.token`: `Ok(())` only when siteverify says `success: true` (and, with [`Check::action`], the token's
/// action matches, and with [`Check::hostname`], the token's hostname matches).
pub async fn verify(http: &reqwest::Client, c: &Check<'_>) -> Result<(), Failure> {
    let mut fields = vec![("secret", c.secret), ("response", c.token)];
    if let Some(ip) = c.remote_ip.filter(|ip| !ip.is_empty()) {
        fields.push(("remoteip", ip));
    }
    let resp = http
        .post(c.url)
        .header(
            reqwest::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        )
        .body(form(&fields))
        .timeout(c.timeout)
        .send()
        .await
        .map_err(|_| {
            tracing::warn!("turnstile_verify_unavailable");
            Failure::Unavailable
        })?;
    if !resp.status().is_success() {
        tracing::warn!(
            status = resp.status().as_u16(),
            "turnstile_verify_unavailable"
        );
        return Err(Failure::Unavailable);
    }
    let body: SiteVerify = resp.json().await.map_err(|_| Failure::Unavailable)?;
    if body.success != Some(true) {
        tracing::info!("turnstile_verify_rejected");
        return Err(Failure::Rejected);
    }
    if let Some(want) = c.action
        && body.action.as_deref() != Some(want)
    {
        tracing::info!("turnstile_verify_wrong_action");
        return Err(Failure::Rejected);
    }
    if let Some(want) = c.hostname
        && !body
            .hostname
            .as_deref()
            .is_some_and(|h| h.eq_ignore_ascii_case(want))
    {
        tracing::info!("turnstile_verify_wrong_hostname");
        return Err(Failure::Rejected);
    }
    Ok(())
}
