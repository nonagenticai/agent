//! Attachments are an admin setting (owner 2026-10-28): whether customers and staff may send photos and PDFs is
//! `settings.attachments {customer, staff}` on the job's customer channel binding, set in the console, not a switch in
//! the code. Only a web chat or a Telegram binding can carry files (their connectors do); any other kind refuses
//! `true` when the settings are saved. An absent setting means on for those two kinds and off for the rest.
//!
//! - [`check`] validates the setting (`bindings::check_settings`).
//! - [`Switches::of`] reads it; [`for_conversation`] finds the binding a conversation arrived on (staff side, in the
//!   tenant's transaction); [`for_edge_binding`] reads it by binding id for the edge's uploads.
//! - `GET /api/jobs/{job}/attachment-quota`: the tenant's storage quota and what it holds now, read-only for the
//!   tenant admin (the quota itself is the platform's: `PATCH /api/tenants/{id}`).
//!
//! A customer upload with customer files off is 422 `attachment_refused` with `reason: attachments_off` (the widget
//! and the Telegram connector turn that into plain words). Because nothing can be uploaded, nothing is left to link
//! to an inbound message; a file uploaded just before the admin switched it off is simply never linked.
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use super::bindings::require_channel_admin;
use super::store;
use crate::attachments::store::{tenant_quota, used_bytes};
use crate::error::{ApiError, ApiResult};
use crate::state::AppState;
use crate::tenancy::JobMember;

/// The binding kinds whose connector carries a file.
pub const KINDS: [&str; 2] = ["web_chat", "telegram"];

/// Whether a binding of `kind` can carry files at all.
pub fn kind_takes_files(kind: &str) -> bool {
    KINDS.contains(&kind)
}

/// Who may send files on a conversation's channel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct Switches {
    /// The customer attaches a photo or PDF to a message.
    pub customer: bool,
    /// Staff attach a photo or PDF to a reply.
    pub staff: bool,
}

impl Switches {
    /// What a kind does when the admin has not set anything.
    pub fn default_for(kind: &str) -> Self {
        let on = kind_takes_files(kind);
        Self {
            customer: on,
            staff: on,
        }
    }

    /// The switches of a binding of `kind` with `settings`: the stored value, never more than the kind can do.
    pub fn of(kind: &str, settings: &Value) -> Self {
        let take = |who: &str| {
            settings
                .get("attachments")
                .and_then(|a| a.get(who))
                .and_then(Value::as_bool)
                .unwrap_or(true)
        };
        let can = kind_takes_files(kind);
        Self {
            customer: can && take("customer"),
            staff: can && take("staff"),
        }
    }
}

/// `settings.attachments` as saved: `{customer?: bool, staff?: bool}`; `true` only on a web chat or Telegram binding
/// (422 `invalid_settings`).
pub fn check(kind: &str, settings: &Value) -> ApiResult<()> {
    let Some(a) = settings.get("attachments") else {
        return Ok(());
    };
    let bad = || {
        ApiError::Unprocessable(
            "invalid_settings",
            "attachments is {customer, staff}, each true or false".into(),
        )
    };
    for (who, on) in a.as_object().ok_or_else(bad)? {
        if who != "customer" && who != "staff" {
            return Err(bad());
        }
        match on.as_bool() {
            None => return Err(bad()),
            Some(true) if !kind_takes_files(kind) => {
                return Err(ApiError::Unprocessable(
                    "invalid_settings",
                    "Photos and PDFs can only be turned on for a web chat or a Telegram binding."
                        .into(),
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// The switches of the channel a conversation's customer is on: the binding it arrived on, else the channel's
/// default (a conversation without a binding, such as the staff test chat).
pub async fn for_conversation(
    conn: &mut PgConnection,
    conversation_id: &str,
    channel: &str,
) -> ApiResult<Switches> {
    let Some((_, Some(binding))) = store::customer_ref(&mut *conn, conversation_id).await? else {
        return Ok(Switches::default_for(channel));
    };
    let row: Option<(String, Value)> =
        sqlx::query_as("SELECT kind, settings FROM connector_bindings WHERE id = $1")
            .bind(binding)
            .fetch_optional(conn)
            .await?;
    let of_binding = row.map_or_else(
        || Switches::default_for(channel),
        |(kind, settings)| Switches::of(&kind, &settings),
    );
    // the conversation's own channel must be able to carry files too
    let can = kind_takes_files(channel);
    Ok(Switches {
        customer: of_binding.customer && can,
        staff: of_binding.staff && can,
    })
}

/// The switches of an enabled, verified binding by id (the edge's calls carry no tenant context); `None` when there
/// is no such binding.
pub async fn for_edge_binding(db: &PgPool, binding: Uuid) -> ApiResult<Option<Switches>> {
    let row: Option<(String, Value)> =
        sqlx::query_as("SELECT kind, settings FROM agent_connector_bindings() WHERE id = $1")
            .bind(binding)
            .fetch_optional(db)
            .await?;
    Ok(row.map(|(kind, settings)| Switches::of(&kind, &settings)))
}

/// 422 `attachment_refused` `{reason: attachments_off}`: the admin turned customer files off for this channel.
pub fn customer_off() -> ApiError {
    ApiError::detailed(
        StatusCode::UNPROCESSABLE_ENTITY,
        "attachment_refused",
        "Photos and PDFs are turned off for this channel.",
    )
    .with("reason", "attachments_off")
}

/// 422 `attachments_not_supported`: staff files are off (or the channel cannot carry any).
pub fn staff_off() -> ApiError {
    ApiError::Unprocessable(
        "attachments_not_supported",
        "Photos and PDFs are turned off for staff on this channel.".into(),
    )
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/jobs/{job}/attachment-quota", get(quota))
}

/// `GET /api/jobs/{job}/attachment-quota` → `{quota_bytes, default_quota_bytes, custom, used_bytes}`;
/// `tenant_admin` or `integrations_manager` (as the job's channel bindings).
async fn quota(JobMember(j): JobMember, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    require_channel_admin(&j)?;
    let default = st.cfg.attachment_quota_bytes;
    let mut tx = j.begin(&st.db).await?;
    let quota = tenant_quota(&mut tx, default).await?;
    let used = used_bytes(&mut tx).await?;
    tx.commit().await?;
    Ok(Json(json!({
        "quota_bytes": quota,
        "default_quota_bytes": default,
        "custom": quota != default,
        "used_bytes": used,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_means_on_for_web_chat_and_telegram_only() {
        let on = Switches {
            customer: true,
            staff: true,
        };
        let off = Switches {
            customer: false,
            staff: false,
        };
        for kind in ["web_chat", "telegram"] {
            assert_eq!(Switches::of(kind, &json!({})), on, "{kind}");
            assert_eq!(Switches::default_for(kind), on, "{kind}");
        }
        assert_eq!(Switches::of("helpdesk", &json!({})), off);
        // a stored true never makes a kind without a connector carry files
        let stored = json!({"attachments": {"customer": true, "staff": true}});
        assert_eq!(Switches::of("helpdesk", &stored), off);
    }

    #[test]
    fn each_side_is_read_on_its_own() {
        let s = json!({"attachments": {"customer": false}});
        assert_eq!(
            Switches::of("telegram", &s),
            Switches {
                customer: false,
                staff: true
            }
        );
        let s = json!({"attachments": {"staff": false}});
        assert_eq!(
            Switches::of("web_chat", &s),
            Switches {
                customer: true,
                staff: false
            }
        );
    }

    #[test]
    fn only_web_chat_and_telegram_may_turn_files_on() {
        let on = json!({"attachments": {"customer": true, "staff": false}});
        assert!(check("web_chat", &on).is_ok());
        assert!(check("telegram", &on).is_ok());
        assert!(check("web_chat", &json!({})).is_ok());
        let e = check("helpdesk", &on).unwrap_err();
        assert_eq!(e.code(), "invalid_settings");
        // switching files off is fine on any kind
        assert!(
            check(
                "helpdesk",
                &json!({"attachments": {"customer": false, "staff": false}})
            )
            .is_ok()
        );
        for bad in [
            json!({"attachments": true}),
            json!({"attachments": {"customer": "yes"}}),
            json!({"attachments": {"everyone": false}}),
            json!({"attachments": []}),
        ] {
            assert_eq!(
                check("web_chat", &bad).unwrap_err().code(),
                "invalid_settings",
                "{bad}"
            );
        }
    }
}
