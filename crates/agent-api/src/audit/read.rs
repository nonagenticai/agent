//! Reading the audit logs (B1 T15/T16): the `AuditEvent` rows of the tenant log as the UI shows them
//! (`web/src/lib/api/types/tenant.ts`), with the read-time fields: `actor_name` (the member's name: display name,
//! else email, as `crate::members::names`), `platform_actor`, `resource_label` (the name of a user object, the same
//! way) and, for failed rows, `outcome`.
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

/// The select list of an [`AuditRow`] over `audit_event e` (joins `users a` for the actor and `users r` for a user
/// resource; both under the transaction's tenant scope).
pub const ROW_SELECT: &str = "SELECT e.id, e.at, e.actor_id, e.actor_type, e.platform_actor, e.action, \
            e.resource_type, e.resource_id, e.job, e.status, e.details, \
            coalesce(nullif(btrim(a.display_name), ''), nullif(btrim(a.email), '')) AS actor_name, \
            CASE WHEN e.resource_type = 'user' \
                 THEN coalesce(nullif(btrim(r.display_name), ''), nullif(btrim(r.email), '')) END AS resource_label \
       FROM audit_event e \
       LEFT JOIN users a ON a.sub = e.actor_id \
       LEFT JOIN users r ON r.sub = e.resource_id";

/// One tenant-log row with its read-time names.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AuditRow {
    pub id: i64,
    pub at: DateTime<Utc>,
    pub actor_id: String,
    pub actor_type: String,
    pub platform_actor: bool,
    pub action: String,
    pub resource_type: String,
    pub resource_id: String,
    pub job: Option<String>,
    pub status: String,
    pub details: Value,
    pub actor_name: Option<String>,
    pub resource_label: Option<String>,
}

impl AuditRow {
    /// A failure-like row (`failure`, `error`); `denied` is policy working, not a failure to fix.
    pub fn failed(&self) -> bool {
        matches!(self.status.as_str(), "failure" | "error")
    }

    /// An agent operation call.
    pub fn is_call(&self) -> bool {
        self.action == "execute"
    }
}

/// `AuditEvent` (`id` as a string); `outcome` when given.
pub fn event_json(r: &AuditRow, outcome: Option<Value>) -> Value {
    let mut v = json!({
        "id": r.id.to_string(),
        "at": r.at,
        "actor_id": r.actor_id,
        "actor_type": r.actor_type,
        "action": r.action,
        "resource_type": r.resource_type,
        "resource_id": r.resource_id,
        "job": r.job,
        "status": r.status,
        "details": r.details,
        "actor_name": r.actor_name,
        "platform_actor": r.platform_actor,
        "resource_label": r.resource_label,
    });
    if let Some(o) = outcome {
        v["outcome"] = o;
    }
    v
}

/// The T4 event families (port of `web/src/lib/tenant/auditT4.ts` `FAMILIES`: key and action prefixes; `user` matches
/// `user` and `user.*`). `calls` is `execute`; `other` is everything else.
pub const TENANT_FAMILIES: &[(&str, &[&str])] = &[
    ("access", &["user", "tenant"]),
    ("releases", &["release", "refinement", "job", "chat"]),
    (
        "connections",
        &["connection", "provider", "provider_slot", "channel"],
    ),
    (
        "knowledge",
        &["knowledge_store", "knowledge_document", "ingest", "source"],
    ),
    ("platform", &["provision", "licence", "license"]),
];

/// The P19 platform families (port of `web/src/lib/platform/auditP19.ts` `FAMILIES`).
pub const PLATFORM_FAMILIES: &[(&str, &[&str])] = &[
    ("signup", &["signup"]),
    (
        "creation",
        &[
            "tenant.import",
            "tenant.create",
            "invite.unresolved",
            "license.issue_failed",
        ],
    ),
    ("keys", &["license.reseal"]),
    (
        "catalogue",
        &[
            "dimension",
            "layer",
            "template",
            "business_model",
            "industry",
            "department",
        ],
    ),
];

/// `action` is `prefix` or starts with `prefix.`.
pub fn matches_prefix(action: &str, prefix: &str) -> bool {
    action == prefix
        || action
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('.'))
}

/// The family of a tenant-log action.
pub fn tenant_family(action: &str) -> &'static str {
    if action == "execute" {
        return "calls";
    }
    TENANT_FAMILIES
        .iter()
        .find(|(_, ps)| ps.iter().any(|p| matches_prefix(action, p)))
        .map_or("other", |(k, _)| *k)
}

/// The family of a platform-log action.
pub fn platform_family(action: &str) -> &'static str {
    PLATFORM_FAMILIES
        .iter()
        .find(|(_, ps)| ps.iter().any(|p| matches_prefix(action, p)))
        .map_or("other", |(k, _)| *k)
}

/// `%q%` for ILIKE, with `\`, `%` and `_` escaped.
pub fn ilike(q: &str) -> String {
    let mut out = String::with_capacity(q.len() + 2);
    out.push('%');
    for c in q.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// The open-outcome object.
pub fn open_outcome() -> Value {
    json!({"state": "open", "by_id": null, "by_action": null, "by_at": null})
}

/// The resolved-outcome object.
pub fn resolved_outcome(by_id: i64, by_action: &str, by_at: DateTime<Utc>) -> Value {
    json!({"state": "resolved", "by_id": by_id.to_string(), "by_action": by_action, "by_at": by_at})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_and_prefixes() {
        assert_eq!(tenant_family("user.invite"), "access");
        assert_eq!(tenant_family("tenant.switch"), "access");
        assert_eq!(tenant_family("provider_slot.save"), "connections");
        assert_eq!(tenant_family("provision.failed"), "platform");
        assert_eq!(tenant_family("license.revoke"), "platform");
        assert_eq!(tenant_family("execute"), "calls");
        assert_eq!(tenant_family("userx.add"), "other");
        assert_eq!(platform_family("signup.approve"), "signup");
        assert_eq!(platform_family("tenant.create"), "creation");
        assert_eq!(platform_family("tenant.offboard"), "other");
        assert_eq!(ilike("50%_a"), "%50\\%\\_a%");
    }
}
