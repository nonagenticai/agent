//! The tenant holiday calendar (B4 T16a, F-14; route table `tenantCalendar`, `saveTenantCalendar`):
//!
//! - `GET /api/tenant/calendar` — any member: `{live, draft, drafted}` (crate::handover::calendar::view).
//! - `PUT /api/tenant/calendar` `{timezone, holidays}` — `support_manager`: validates (422 `invalid_calendar`:
//!   unknown timezone, bad date, duplicate date, more than 366 rows), then writes the tenant draft through
//!   `design::track_tenant` (kind `tenant_calendar`, D54/D69: recorded in every job's draft, never live on save).
use axum::Router;
use axum::extract::State;
use axum::response::Json;
use axum::routing::get;
use serde_json::Value;

use crate::design::{self, DraftRef, DraftWrite, Label};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::handover::{calendar, hours};
use crate::state::AppState;
use crate::tenancy::{self, Member, Requires, role};

/// The design area and page of a calendar change (the tenant admin home's Calendar card).
const AREA: &str = "calendar";
const PAGE: &str = "T1";

pub fn routes() -> Router<AppState> {
    Router::new().route(
        "/api/tenant/calendar",
        get(tenant_calendar).put(save_tenant_calendar),
    )
}

async fn tenant_calendar(Member(t): Member, State(st): State<AppState>) -> ApiResult<Json<Value>> {
    let mut tx = t.begin(&st.db).await?;
    let v = calendar::view(&mut tx, &st.design, t.tenant).await?;
    tx.commit().await?;
    Ok(Json(v))
}

fn invalid(msg: impl Into<String>) -> ApiError {
    ApiError::Unprocessable("invalid_calendar", msg.into())
}

async fn save_tenant_calendar(
    Requires(t, _): Requires<role::SupportManager>,
    State(st): State<AppState>,
    ApiJson(b): ApiJson<Value>,
) -> ApiResult<Json<Value>> {
    let cal = calendar::validate(&b).map_err(invalid)?;
    let mut tx = t.begin(&st.db).await?;
    if !hours::known_timezone(&mut tx, &cal.timezone).await? {
        return Err(invalid("unknown timezone"));
    }
    // lock order: the tenants row first (the audit row's FK takes FOR KEY SHARE on it), then the jobs
    tenancy::lock_tenant_row(&mut tx, t.tenant).await?;
    let w = DraftWrite {
        area: AREA,
        page: PAGE,
        kind: calendar::KIND,
        r#ref: DraftRef::new(),
        label: Label::Now(format!(
            "tenant calendar: {}",
            calendar::calendar_text(Some(&cal))
        )),
    };
    let (tenant, by) = (t.tenant, t.caller.sub.clone());
    design::track_tenant::<(), _>(&mut tx, &st.design, &t, w, move |c| {
        Box::pin(async move { calendar::write(c, tenant, &cal, &by).await })
    })
    .await?;
    let v = calendar::view(&mut tx, &st.design, t.tenant).await?;
    tx.commit().await?;
    Ok(Json(v))
}
