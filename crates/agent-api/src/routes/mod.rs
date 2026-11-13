use axum::Router;

use crate::state::AppState;

pub mod agent_status;
pub mod agents;
pub mod analytics;
pub mod audit;
pub mod bus_flow;
pub mod cases;
pub mod chat;
pub mod chat_profile;
pub mod customer_context;
pub mod discovery;
pub mod generator;
pub mod generator_drive;
pub mod grounding;
pub mod handover;
pub mod health;
pub mod inbox;
pub mod ingest;
pub mod invites;
pub mod job_design;
pub mod jobs;
pub mod journey;
pub mod knowledge;
pub mod knowledge_eval;
pub mod knowledge_kinds;
pub mod licensing;
pub mod llm_providers;
pub mod mcp_tasks;
pub mod me;
pub mod model_changes;
pub mod operation_calls;
pub mod overview;
pub mod platform;
pub mod platform_exclusions;
pub mod platform_knowledge;
pub mod provider_slots;
pub mod release;
pub mod roles;
pub mod search;
pub mod signups;
pub mod sources;
pub mod streams;
pub mod templates;
pub mod tenant_calendar;
pub mod tenant_channels;
pub mod tenant_home;
pub mod tenant_profile;
pub mod tenants;
pub mod test_cases;
pub mod usage;
pub mod users;
pub mod validation;

/// Every route. The `/api/signup-approvals` routes exist only while early-access signup is on; while it is
/// dark they are the unknown-route 404, so the app can deploy before the intake is live.
pub fn router(st: AppState) -> Router {
    let mut r = Router::new()
        .merge(health::routes())
        .merge(usage::routes())
        .merge(me::routes())
        .merge(roles::routes())
        .merge(tenants::routes())
        .merge(licensing::routes())
        .merge(jobs::routes())
        .merge(templates::routes())
        .merge(users::routes())
        .merge(invites::routes())
        .merge(platform::routes())
        .merge(tenant_home::routes())
        .merge(tenant_profile::routes())
        .merge(audit::routes())
        .merge(release::routes())
        .merge(job_design::routes())
        .merge(journey::routes())
        .merge(llm_providers::routes())
        .merge(mcp_tasks::routes())
        .merge(provider_slots::routes())
        .merge(knowledge::routes())
        .merge(model_changes::routes())
        .merge(ingest::routes())
        .merge(knowledge_eval::routes())
        .merge(knowledge_kinds::routes())
        .merge(platform_exclusions::routes())
        .merge(platform_knowledge::routes())
        .merge(grounding::routes())
        .merge(sources::routes())
        .merge(crate::platform::routes::routes())
        .merge(crate::provision::routes::routes())
        .merge(crate::connectors::routes::routes())
        .merge(crate::interaction::routes::routes())
        .merge(crate::interaction::mcp::routes())
        .merge(crate::interaction::apikeys::routes())
        .merge(crate::voice::routes::routes())
        .merge(crate::conversations::routes::routes())
        .merge(crate::conversations::internal::routes())
        .merge(crate::support::routes::routes())
        .merge(chat::routes())
        .merge(chat_profile::routes())
        .merge(customer_context::routes())
        .merge(operation_calls::routes())
        .merge(crate::party::routes::routes())
        .merge(crate::iam::routes::routes())
        .merge(cases::routes())
        .merge(agents::routes())
        .merge(streams::routes())
        .merge(bus_flow::routes())
        .merge(overview::routes())
        .merge(agent_status::routes())
        .merge(inbox::routes())
        .merge(search::routes())
        .merge(analytics::routes())
        .merge(handover::routes())
        .merge(tenant_calendar::routes())
        .merge(tenant_channels::routes())
        .merge(test_cases::routes())
        .merge(discovery::routes())
        .merge(generator::routes())
        .merge(generator_drive::routes())
        .merge(validation::routes());
    if st.signup.is_some() {
        r = r.merge(signups::routes());
    }
    r.fallback(crate::error::fallback)
        .method_not_allowed_fallback(crate::error::method_not_allowed)
        .layer(axum::middleware::from_fn(crate::observability::http_layer))
        .with_state(st)
}
