//! `GET /api/roles` (roleCatalog, D88 plan §R.4): the tenant role vocabulary in §R.1 order,
//! `[{key, label, description}]`. Any member (an observer included) or a super_admin in a tenant.
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::auth::Role;
use crate::state::AppState;
use crate::tenancy::Member;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/roles", get(catalog))
}

/// The catalog body (also what the web fixture mirrors).
pub fn catalog_json() -> Value {
    Value::Array(
        Role::ALL
            .into_iter()
            .map(|r| json!({"key": r.key(), "label": r.label(), "description": r.description()}))
            .collect(),
    )
}

async fn catalog(Member(_): Member) -> Json<Value> {
    Json(catalog_json())
}
