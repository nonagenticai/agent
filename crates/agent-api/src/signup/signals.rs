//! Signup intake signals (B1b T3, owner ruling F-03): what intake noticed about each request in the approval
//! queue, computed at read time (no column, no migration) as `SignupSignal` (web `types/platform.ts`):
//!
//! - `turnstile`: the `details.turnstile` B1b T1 writes on the request's `signup.intake` platform audit row —
//!   `verified` ok, `not_configured` info, `absent` risk; a row from before T1 says "not recorded" (info); a
//!   super_admin invite has no intake (info).
//! - `domain`: a disposable-mail domain is risk, a free-mail provider info ("personal address"), else ok.
//! - `duplicate`: another request from the same address (any status) is risk; another PENDING request from the same
//!   (non-free-mail) domain is info.
//! - `history`: the address was rejected before (risk, `signup.reject`); it is already a member of a workspace
//!   (risk, `user.add`).
//! - `detail`: no description given (info).
//!
//! Labels and details are fixed templates plus, at most, the address's domain: never anything the requester typed.
//! One query per kind for the whole page (no N+1), in the caller's `begin_platform` transaction; the membership
//! read is the one cross-tenant users read (`agent_signup_homes`, granted to `agent_app`), so it runs between
//! `leave` and `enter`.
use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::ApiResult;
use crate::platform_db::{enter, leave};
use crate::signup::freemail;
use crate::signup::store::Approval;

/// `SignupSignal`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Signal {
    pub key: &'static str,
    pub level: &'static str,
    pub label: &'static str,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_action: Option<&'static str>,
}

fn signal(
    key: &'static str,
    level: &'static str,
    label: &'static str,
    detail: impl Into<String>,
    audit_action: Option<&'static str>,
) -> Signal {
    Signal {
        key,
        level,
        label,
        detail: detail.into(),
        audit_action,
    }
}

/// The signals of a page, and how many queries computing them took (constant per page: one per kind).
pub struct PageSignals {
    pub by_id: HashMap<Uuid, Vec<Signal>>,
    pub queries: usize,
}

impl PageSignals {
    /// The signals of row `id` as JSON (`[]` when none).
    pub fn json(&self, id: Uuid) -> Value {
        serde_json::to_value(self.by_id.get(&id).cloned().unwrap_or_default())
            .unwrap_or(Value::Array(Vec::new()))
    }
}

/// The `turnstile` signal from the intake row's recorded outcome.
pub fn turnstile(source: &str, recorded: Option<&str>) -> Signal {
    const ACTION: Option<&str> = Some("signup.intake");
    if source == "invite" {
        return signal(
            "turnstile",
            "info",
            "No intake check",
            "A super_admin invite does not pass through intake.",
            Some("signup.invite"),
        );
    }
    match recorded {
        Some("verified") => signal(
            "turnstile",
            "ok",
            "Bot check passed",
            "Turnstile verified this request at intake.",
            ACTION,
        ),
        Some("not_configured") => signal(
            "turnstile",
            "info",
            "Bot check off",
            "Turnstile was not configured when this request arrived; nothing was checked.",
            ACTION,
        ),
        Some("absent") => signal(
            "turnstile",
            "risk",
            "No bot check token",
            "Turnstile is on but the intake relayed no token for this request.",
            ACTION,
        ),
        _ => signal(
            "turnstile",
            "info",
            "Bot check not recorded",
            "This request arrived before intake recorded its bot check.",
            ACTION,
        ),
    }
}

/// The `domain` signal of an address.
pub fn domain(email: &str) -> Signal {
    match freemail::domain(email) {
        Some(d) if freemail::is_disposable(&d) => signal(
            "domain",
            "risk",
            "Disposable address",
            format!("{d} is a disposable-mail service."),
            None,
        ),
        Some(d) if freemail::is_freemail(&d) => signal(
            "domain",
            "info",
            "Personal address",
            format!("{d} is a free-mail provider, not a company domain."),
            None,
        ),
        Some(d) => signal("domain", "ok", "Company domain", d, None),
        None => signal(
            "domain",
            "risk",
            "No domain",
            "The address has no domain.",
            None,
        ),
    }
}

/// The `duplicate` signal: `same_address` other requests from the address (any status), `same_domain_pending` other
/// pending requests from its domain.
pub fn duplicate(email: &str, same_address: i64, same_domain_pending: i64) -> Signal {
    const ACTION: Option<&str> = Some("signup.intake");
    if same_address > 0 {
        return signal(
            "duplicate",
            "risk",
            "Repeat request",
            format!("{same_address} other request(s) from this address."),
            ACTION,
        );
    }
    match freemail::domain(email) {
        Some(d) if same_domain_pending > 0 && !freemail::is_freemail(&d) => signal(
            "duplicate",
            "info",
            "Same company domain",
            format!("{same_domain_pending} other pending request(s) from {d}."),
            ACTION,
        ),
        _ => signal(
            "duplicate",
            "ok",
            "First request",
            "No other request from this address.",
            None,
        ),
    }
}

/// The `history` signals: rejected before, already a member; one `ok` when neither.
pub fn history(rejected_before: bool, member: bool) -> Vec<Signal> {
    let mut out = Vec::new();
    if rejected_before {
        out.push(signal(
            "history",
            "risk",
            "Rejected before",
            "An earlier request from this address was rejected.",
            Some("signup.reject"),
        ));
    }
    if member {
        out.push(signal(
            "history",
            "risk",
            "Already a member",
            "This address already belongs to a workspace.",
            Some("user.add"),
        ));
    }
    if out.is_empty() {
        out.push(signal(
            "history",
            "ok",
            "No history",
            "No earlier rejection and no workspace membership.",
            None,
        ));
    }
    out
}

/// The `detail` signal: was a description given? (Only its emptiness is read, never its text.)
pub fn detail(message: Option<&str>) -> Signal {
    if message.is_none_or(|m| m.trim().is_empty()) {
        signal(
            "detail",
            "info",
            "No description",
            "No description of the use case was given.",
            None,
        )
    } else {
        signal(
            "detail",
            "ok",
            "Description given",
            "The requester described their use case.",
            None,
        )
    }
}

/// The signals of `rows` (one page of the queue). `conn` is `agent_platform` (`begin_platform`) and is left so.
pub async fn for_rows(conn: &mut PgConnection, rows: &[Approval]) -> ApiResult<PageSignals> {
    let mut queries = 0usize;
    let mut by_id = HashMap::new();
    if rows.is_empty() {
        return Ok(PageSignals { by_id, queries });
    }
    let ids: Vec<Uuid> = rows.iter().map(|a| a.id).collect();
    let emails: Vec<String> = rows.iter().map(|a| a.email.to_lowercase()).collect();

    // 1. the turnstile outcome on each request's intake row
    let recorded: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT details->>'signup', details->>'turnstile' FROM platform_audit \
          WHERE action = 'signup.intake' AND details->>'signup' = ANY ($1::text[])",
    )
    .bind(ids.iter().map(Uuid::to_string).collect::<Vec<_>>())
    .fetch_all(&mut *conn)
    .await?;
    queries += 1;
    let recorded: HashMap<String, Option<String>> = recorded.into_iter().collect();

    // 2. other requests: same address (any status / rejected), same domain pending
    type Counts = (Uuid, i64, bool, i64);
    let counts: Vec<Counts> = sqlx::query_as(
        "SELECT a.id, \
                count(o.id) FILTER (WHERE lower(o.email) = lower(a.email)), \
                coalesce(bool_or(lower(o.email) = lower(a.email) AND o.status = 'rejected'), false), \
                count(o.id) FILTER (WHERE lower(o.email) <> lower(a.email) AND o.status = 'pending') \
           FROM signup_approvals a \
           LEFT JOIN signup_approvals o ON o.id <> a.id \
                 AND (lower(o.email) = lower(a.email) \
                      OR split_part(lower(o.email), '@', 2) = split_part(lower(a.email), '@', 2)) \
          WHERE a.id = ANY ($1) \
          GROUP BY a.id",
    )
    .bind(&ids)
    .fetch_all(&mut *conn)
    .await?;
    queries += 1;
    let counts: HashMap<Uuid, (i64, bool, i64)> = counts
        .into_iter()
        .map(|(id, same, rejected, domain)| (id, (same, rejected, domain)))
        .collect();

    // 3. addresses already a member somewhere: the one cross-tenant users read, as agent_app
    leave(&mut *conn).await?;
    let members: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT e FROM unnest($1::text[]) AS x (e) \
          WHERE EXISTS (SELECT 1 FROM agent_signup_homes(e, NULL))",
    )
    .bind(&emails)
    .fetch_all(&mut *conn)
    .await?;
    queries += 1;
    enter(&mut *conn).await?;

    for a in rows {
        let (same, rejected, domain_pending) = counts.get(&a.id).copied().unwrap_or_default();
        let mut s = vec![
            turnstile(
                &a.source,
                recorded.get(&a.id.to_string()).and_then(|o| o.as_deref()),
            ),
            domain(&a.email),
            duplicate(&a.email, same, domain_pending),
        ];
        // An approved requester is a member because of this very request: not a risk.
        let member = a.status != "approved" && members.contains(&a.email.to_lowercase());
        s.extend(history(rejected, member));
        s.push(detail(a.message.as_deref()));
        by_id.insert(a.id, s);
    }
    Ok(PageSignals { by_id, queries })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turnstile_levels() {
        assert_eq!(turnstile("web", Some("verified")).level, "ok");
        assert_eq!(turnstile("web", Some("not_configured")).level, "info");
        assert_eq!(turnstile("web", Some("absent")).level, "risk");
        let old = turnstile("web", None);
        assert_eq!((old.level, old.label), ("info", "Bot check not recorded"));
        assert_eq!(turnstile("web", None).audit_action, Some("signup.intake"));
        assert_eq!(turnstile("invite", None).level, "info");
    }

    #[test]
    fn disposable_domain_is_risk_freemail_is_info() {
        assert_eq!(domain("a@mailinator.com").level, "risk");
        let f = domain("a@GMail.com");
        assert_eq!((f.level, f.label), ("info", "Personal address"));
        assert!(f.detail.contains("gmail.com") && !f.detail.contains('@'));
        assert_eq!(domain("a@globex.com").level, "ok");
    }

    #[test]
    fn duplicate_and_history() {
        assert_eq!(duplicate("a@globex.com", 1, 0).level, "risk");
        assert_eq!(duplicate("a@globex.com", 0, 2).level, "info");
        assert_eq!(
            duplicate("a@gmail.com", 0, 9).level,
            "ok",
            "a free-mail domain is no company"
        );
        let h = history(true, true);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].audit_action, Some("signup.reject"));
        assert_eq!(h[1].audit_action, Some("user.add"));
        assert_eq!(history(false, false)[0].level, "ok");
        assert_eq!(detail(Some("  ")).level, "info");
        assert_eq!(detail(None).level, "info");
        assert_eq!(detail(Some("We run clinics")).level, "ok");
        assert!(!detail(Some("We run clinics")).detail.contains("clinics"));
    }
}
