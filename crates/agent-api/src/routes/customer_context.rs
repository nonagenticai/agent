//! `GET /api/jobs/{job}/conversations/{id}/customer-context` (B4 known customer, for the staff workspace): what the
//! agent knows about the conversation's identified customer, in the shape of the web's customer panel:
//!
//! ```text
//! {identity{source, level, verified_at}, customer{id, name, email}|null,
//!  orders[{order_id, status, placed_at, items[{name, qty}]}],
//!  payments[{payment_id, order_id, amount, currency, status, refunded_amount, at}],
//!  deliveries[{delivery_id, order_id, carrier, status, eta, delivered_at, tracking_url?}],
//!  cases[{case_id, state, title, opened_at}], focus[{kind, id, why}]}
//! ```
//!
//! - **Who.** `support_agent`, `support_manager` or `operator` (read only; an operator sees what a worker sees); any
//!   other member is 403 `forbidden` (`missing_role: support_agent`). The conversation is looked up in the caller's
//!   tenant and job: another tenant's conversation, like an unknown one, is 404 `conversation_not_found`.
//! - **Source.** `identity.source` is the conversation's `identity_source` state (`assertion` | `staff_pick`;
//!   crate::conversations::store::Conv::known_customer): null means nobody is identified, and then `customer` is null
//!   and every list is empty. `orders`, `payments` and `deliveries` are the snapshot the case-start context phase
//!   loaded (crate::runtime::customer_context; empty until a case has loaded it); `cases` are read now.
//! - **Focus.** The records this conversation's calls named (`operation_call.step.refs`: ids only), newest first, one
//!   entry per record, with a plain-words reason. The context phase's own reads are not focus.
use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use serde_json::{Value, json};

use crate::auth::Role;
use crate::conversations::{iso, store};
use crate::error::{ApiPath, ApiResult};
use crate::runtime::customer_context as cc;
use crate::state::AppState;
use crate::tenancy::{JobMember, missing_role};

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/jobs/{job}/conversations/{id}/customer-context",
        get(customer_context),
    )
}

/// The roles that read a customer's context.
pub const READERS: [Role; 3] = [Role::SupportAgent, Role::SupportManager, Role::Operator];

/// The most focus entries returned.
const FOCUS_MAX: usize = 10;

async fn customer_context(
    JobMember(j): JobMember,
    State(st): State<AppState>,
    ApiPath((_, id)): ApiPath<(String, String)>,
) -> ApiResult<Json<Value>> {
    if !READERS.iter().any(|r| j.t.effective.has(*r)) {
        return Err(missing_role(Role::SupportAgent));
    }
    let mut tx = j.begin(&st.db).await?;
    let conv = store::conversation_or_404(&mut tx, &id).await?;
    let known = conv.known_customer().map(str::to_owned);
    let Some(party) = known else {
        tx.commit().await?;
        return Ok(Json(json!({
            "identity": {"source": null, "level": 0, "verified_at": null},
            "customer": null, "orders": [], "payments": [], "deliveries": [], "cases": [], "focus": [],
        })));
    };
    // a known customer's cases are all born at this level (derived from the conversation's state)
    let level = i64::from(store::KNOWN_CUSTOMER_IDENTITY);
    let identified_at: Option<chrono::DateTime<chrono::Utc>> = sqlx::query_scalar(
        "SELECT min(identified_at) FROM web_chat_session WHERE conversation_id = $1",
    )
    .bind(&id)
    .fetch_one(&mut *tx)
    .await?;
    let verified_at = match identified_at {
        Some(t) => Some(t),
        None => {
            sqlx::query_scalar("SELECT started_at FROM conversation WHERE conversation_id = $1")
                .bind(&id)
                .fetch_optional(&mut *tx)
                .await?
        }
    };
    let display: Option<Option<String>> = sqlx::query_scalar(
        "SELECT p.display_name FROM conversation v JOIN party p ON p.id = v.party_id \
          WHERE v.conversation_id = $1",
    )
    .bind(&id)
    .fetch_optional(&mut *tx)
    .await?;
    let name = crate::support::names::display_name(
        display.flatten().as_deref(),
        conv.customer_name.as_deref(),
        &conv.channel,
        &conv.id,
    );
    let cases = cc::party_cases(&mut tx, &party, &id).await?;
    let calls: Vec<(String, String, Option<Value>)> = sqlx::query_as(
        "SELECT oc.operation, oc.state, oc.step->'refs' FROM operation_call oc \
           JOIN agent_case ac ON ac.tenant_id = oc.tenant_id AND ac.job_id = oc.job_id AND ac.case_id = oc.case_id \
          WHERE ac.conversation_id = $1 AND COALESCE(oc.step->>'phase', '') <> 'context' \
            AND oc.step->'refs' IS NOT NULL AND oc.step->'refs' <> '{}'::jsonb \
          ORDER BY oc.started_at DESC, oc.call_id DESC LIMIT 50",
    )
    .bind(&id)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;

    let snapshot = match st.redis.as_ref() {
        Some(bus) => cc::read_snapshot(bus, j.t.tenant, &j.job_key, &id)
            .await
            .ok()
            .flatten()
            .map(|(ctx, _, _)| ctx)
            // a staff test chat types an email; the snapshot holds the system's party that email resolved to
            .filter(|ctx| {
                ctx.party == party
                    || ctx.resolved_from.as_deref() == Some(party.to_lowercase().as_str())
            }),
        None => None,
    }
    .unwrap_or_default();

    let mut body = json!({
        "identity": {
            "source": conv.identity_source, "level": level, "verified_at": verified_at.map(iso),
        },
        "customer": {
            "id": if snapshot.party.is_empty() { party.as_str() } else { snapshot.party.as_str() },
            "name": name, "email": conv.customer_email,
        },
        "orders": snapshot.orders,
        "payments": snapshot.payments,
        "deliveries": snapshot.deliveries,
        "cases": cases,
        "focus": focus(&calls),
    });
    // a staff test chat whose typed email found no single customer: what the admin is told (the context is empty).
    // Present only then, so every other answer keeps its fields.
    if let Some(note) = cc::lookup_note(&snapshot.degraded) {
        body["degraded"] = json!(snapshot.degraded);
        body["note"] = json!(note);
    }
    Ok(Json(body))
}

/// One entry per record the conversation's calls named, newest call first, at most [`FOCUS_MAX`].
fn focus(calls: &[(String, String, Option<Value>)]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for (operation, state, refs) in calls {
        let Some(refs) = refs.as_ref().and_then(Value::as_object) else {
            continue;
        };
        let read = operation.ends_with(".read") || operation.ends_with(".list");
        let why = match (read, state.as_str()) {
            (true, _) => "looked up for this chat".to_owned(),
            (false, "ok") => format!("changed in this chat ({operation})"),
            (false, _) => format!("a change was tried in this chat ({operation}, {state})"),
        };
        for (field, kind) in [
            ("order_id", "order"),
            ("payment_id", "payment"),
            ("delivery_id", "delivery"),
        ] {
            let Some(rid) = refs.get(field).and_then(Value::as_str) else {
                continue;
            };
            if out.iter().any(|f| f["kind"] == kind && f["id"] == rid) {
                continue;
            }
            out.push(json!({"kind": kind, "id": rid, "why": why}));
        }
        if out.len() >= FOCUS_MAX {
            break;
        }
    }
    out.truncate(FOCUS_MAX);
    out
}
