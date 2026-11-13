//! B1 T21 (D88, plan §R.1–R.3, R.5): fine-grained tenant roles. The 0039 backfill and CHECKs, the first member of a
//! tenant holding every role, role sets on invite / add / set, the last-admin guard on HELD roles, and the router walk
//! (every tenant write names exactly one §R.1 role or is super_admin-only; nothing grants or revokes super_admin).
//! Real Postgres 18 (one container for this binary); Keycloak / licences / Telegram are the in-process fake.
use crate::common;

use agent_api::auth::{Role, RoleSet};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::fakes::{self, Upstream, lock};
use common::router_walk::{self, Guard, RouteDef};
use common::{
    ALL_ROLES, TestEnv, get, insert_tenant, insert_user, json, person, send, service, sqlstate,
    super_admin,
};
use serde_json::{Map, Value, json};
use sqlx::PgPool;
use uuid::Uuid;

/// §R.1 for the B1 methods: (verb, path, role). Every other non-GET route is super_admin-only or listed below.
const R1_B1: &[(&str, &str, &str)] = &[
    ("POST", "/api/jobs/{job}/upgrade-request", "tenant_admin"),
    // D123/D124: the tenant_admin designs the tenant's jobs and company profile (tenant job design ruling)
    ("POST", "/api/jobs", "tenant_admin"),
    ("PATCH", "/api/jobs/{job}", "tenant_admin"),
    ("PUT", "/api/tenant/profile", "tenant_admin"),
    // A3 (caller identity plan): the tenant's own identity procedures; A2 per-op input policy and job attachments
    ("POST", "/api/iam-procedures", "tenant_admin"),
    ("PUT", "/api/iam-procedures/{id}", "tenant_admin"),
    ("DELETE", "/api/iam-procedures/{id}", "tenant_admin"),
    (
        "PUT",
        "/api/jobs/{job}/tools/{op}/policy",
        "integrations_manager",
    ),
    (
        "DELETE",
        "/api/jobs/{job}/tools/{op}/policy",
        "integrations_manager",
    ),
    (
        "PUT",
        "/api/jobs/{job}/iam-procedures/{id}",
        "integrations_manager",
    ),
    (
        "DELETE",
        "/api/jobs/{job}/iam-procedures/{id}",
        "integrations_manager",
    ),
    (
        "POST",
        "/api/tenant/provisioning/{id}/retry",
        "tenant_admin",
    ),
    (
        "POST",
        "/api/tenant/provisioning/{id}/cancel",
        "tenant_admin",
    ),
    ("POST", "/api/tenant/invites", "tenant_admin"),
    (
        "POST",
        "/api/tenant/invites/{invite_id}/resend",
        "tenant_admin",
    ),
    (
        "POST",
        "/api/tenant/invites/{invite_id}/revoke",
        "tenant_admin",
    ),
    ("PUT", "/api/tenant/users/{sub}", "tenant_admin"),
    ("PATCH", "/api/tenant/users/{sub}", "tenant_admin"),
    ("POST", "/api/tenant/users/{sub}/disable", "tenant_admin"),
    ("POST", "/api/tenant/users/{sub}/enable", "tenant_admin"),
    // B2 (§4): the release area
    ("PUT", "/api/jobs/{job}/target", "release_manager"),
    ("POST", "/api/jobs/{job}/releases", "release_manager"),
    (
        "POST",
        "/api/jobs/{job}/releases/{id}/apply",
        "release_manager",
    ),
    // D130: publish / unpublish a customer channel (tenant_admin holds release_manager, D127)
    (
        "POST",
        "/api/jobs/{job}/channel-bindings/{id}/publish",
        "release_manager",
    ),
    (
        "POST",
        "/api/jobs/{job}/channel-bindings/{id}/unpublish",
        "release_manager",
    ),
    ("PUT", "/api/jobs/{job}/settings", "job_designer"),
    ("DELETE", "/api/jobs/{job}/settings/{key}", "job_designer"),
    ("PUT", "/api/jobs/{job}/languages", "job_designer"),
    ("DELETE", "/api/jobs/{job}/draft", "job_designer"),
    ("DELETE", "/api/jobs/{job}/draft/{id}", "job_designer"),
    // 0246: a job's own operations from the tenant's templates
    ("PUT", "/api/jobs/{job}/own-operations/{op}", "job_designer"),
    (
        "DELETE",
        "/api/jobs/{job}/own-operations/{op}",
        "job_designer",
    ),
    ("POST", "/api/jobs/{job}/draft/restore", "job_designer"),
    ("POST", "/api/jobs/{job}/refinements/{op}", "job_designer"),
    // B8 T7/T7a (J4): the job's journey refinements
    (
        "POST",
        "/api/jobs/{job}/refinements/journey/hide",
        "job_designer",
    ),
    (
        "POST",
        "/api/jobs/{job}/refinements/journey/restore",
        "job_designer",
    ),
    (
        "POST",
        "/api/jobs/{job}/refinements/journey/extensions",
        "job_designer",
    ),
    (
        "DELETE",
        "/api/jobs/{job}/refinements/journey/extensions/{key}",
        "job_designer",
    ),
    (
        "PUT",
        "/api/jobs/{job}/refinements/journey/labels/{key}",
        "job_designer",
    ),
    (
        "PUT",
        "/api/jobs/{job}/refinements/journey/cards/{key}",
        "job_designer",
    ),
    (
        "DELETE",
        "/api/jobs/{job}/refinements/journey/cards/{key}",
        "job_designer",
    ),
    (
        "PUT",
        "/api/jobs/{job}/context-spaces/{key}",
        "job_designer",
    ),
    // KH T4 (D108): a tenant's own text for an instruction slot
    ("PUT", "/api/jobs/{job}/instructions/{slot}", "job_designer"),
    ("POST", "/api/jobs/{job}/rules", "job_designer"),
    (
        "PUT",
        "/api/jobs/{job}/rules/{id}/definition",
        "job_designer",
    ),
    ("DELETE", "/api/jobs/{job}/rules/{id}", "job_designer"),
    ("PUT", "/api/jobs/{job}/rules/{id}", "job_designer"),
    ("PUT", "/api/jobs/{job}/rules/{id}/passage", "job_designer"),
    // B7 T9/T10 (connectors): every tenant write is integrations_manager (D88)
    ("POST", "/api/connections/systems", "integrations_manager"),
    (
        "PATCH",
        "/api/connections/systems/{id}",
        "integrations_manager",
    ),
    (
        "PUT",
        "/api/connections/systems/{id}/credential",
        "integrations_manager",
    ),
    // B7 T12: mapping overrides (tenant draft)
    (
        "PUT",
        "/api/connections/systems/{id}/mappings/{op}/{field}",
        "integrations_manager",
    ),
    (
        "DELETE",
        "/api/connections/systems/{id}/mappings/{op}/{field}",
        "integrations_manager",
    ),
    (
        "POST",
        "/api/connections/systems/{id}/repin",
        "integrations_manager",
    ),
    (
        "POST",
        "/api/connections/systems/{id}/contract-tests",
        "integrations_manager",
    ),
    (
        "POST",
        "/api/connections/systems/{id}/samples",
        "integrations_manager",
    ),
    // B7 T11: tenant-private connector templates
    ("POST", "/api/connections/templates", "integrations_manager"),
    (
        "PUT",
        "/api/connections/templates/{key}/{version}",
        "integrations_manager",
    ),
    (
        "POST",
        "/api/connections/templates/{key}/{version}/validate",
        "integrations_manager",
    ),
    (
        "POST",
        "/api/connections/templates/{key}/{version}/contract-tests",
        "integrations_manager",
    ),
    (
        "POST",
        "/api/connections/templates/{key}/{version}/status",
        "integrations_manager",
    ),
    (
        "POST",
        "/api/connections/templates/{key}/{version}/drafts",
        "integrations_manager",
    ),
    (
        "POST",
        "/api/connections/templates/{key}/{version}/mcp-import",
        "integrations_manager",
    ),
    // B7 T13: job operation routes (draft)
    (
        "PUT",
        "/api/jobs/{job}/operations/{op}/routes/{env}",
        "integrations_manager",
    ),
    (
        "DELETE",
        "/api/jobs/{job}/operations/{op}/routes/{env}",
        "integrations_manager",
    ),
    // B3 T3 (§5.1): LLM providers and their uses
    ("POST", "/api/providers", "integrations_manager"),
    ("PUT", "/api/providers/{id}", "integrations_manager"),
    ("DELETE", "/api/providers/{id}", "integrations_manager"),
    ("POST", "/api/providers/test", "integrations_manager"),
    ("POST", "/api/providers/{id}/test", "integrations_manager"),
    ("PUT", "/api/providers/uses/{use}", "integrations_manager"),
    // B3 T4 (§5.1): provider slots (D69 drafts)
    ("PUT", "/api/providers/slots/{slot}", "integrations_manager"),
    (
        "POST",
        "/api/providers/slots/{slot}/adopt-remote",
        "integrations_manager",
    ),
    // B3 T4b (F-08): "Use default"; T4c (F-09): test a draft (stores nothing, but TA per §5.1)
    (
        "POST",
        "/api/providers/slots/{slot}/reset",
        "integrations_manager",
    ),
    (
        "POST",
        "/api/providers/slots/{slot}/test",
        "integrations_manager",
    ),
    // B3 review I4: retry an unconfirmed (failed, dead or withheld) slot push
    (
        "POST",
        "/api/providers/slots/{slot}/push",
        "integrations_manager",
    ),
    // B3 T5 (§5.2): the knowledge-store tree and documents
    ("POST", "/api/knowledge/stores", "knowledge_manager"),
    ("DELETE", "/api/knowledge/stores/{id}", "knowledge_manager"),
    ("POST", "/api/knowledge/stores/ensure", "knowledge_manager"),
    (
        "DELETE",
        "/api/knowledge/documents/{id}",
        "knowledge_manager",
    ),
    // B3 T4a (§5.8): model changes per job (D83/D93)
    (
        "POST",
        "/api/knowledge/model-changes/{id}/jobs/{job}/reembed",
        "knowledge_manager",
    ),
    (
        "POST",
        "/api/knowledge/model-changes/{id}/jobs/{job}/accept",
        "knowledge_manager",
    ),
    // B3 T7 (§5.4): a job's knowledge sources (the test and the column preview store nothing but are TA)
    ("POST", "/api/jobs/{job}/sources", "knowledge_manager"),
    ("PUT", "/api/jobs/{job}/sources/{id}", "knowledge_manager"),
    (
        "DELETE",
        "/api/jobs/{job}/sources/{id}",
        "knowledge_manager",
    ),
    ("POST", "/api/jobs/{job}/sources/test", "knowledge_manager"),
    (
        "POST",
        "/api/jobs/{job}/sources/{id}/test",
        "knowledge_manager",
    ),
    (
        "POST",
        "/api/jobs/{job}/sources/preview-columns",
        "knowledge_manager",
    ),
    // B3 T8 (§5.4): a source re-sync (op; strata_ingest jobs + a source_run row)
    (
        "POST",
        "/api/jobs/{job}/sources/{id}/resync",
        "knowledge_manager",
    ),
    // B3 T10 (§5.5): knowledge-kind bindings (draft)
    (
        "PUT",
        "/api/jobs/{job}/knowledge-kinds/{stage}/{kind}",
        "job_designer",
    ),
    // B3 T9 (§5.6): golden questions + eval runs
    ("POST", "/api/knowledge/eval/questions", "knowledge_manager"),
    (
        "PATCH",
        "/api/knowledge/eval/questions/{id}",
        "knowledge_manager",
    ),
    (
        "DELETE",
        "/api/knowledge/eval/questions/{id}",
        "knowledge_manager",
    ),
    (
        "POST",
        "/api/knowledge/eval/questions/import",
        "knowledge_manager",
    ),
    ("POST", "/api/knowledge/eval/runs", "knowledge_manager"),
    // PK T7 (PK-10): tenant-wide platform exclusions
    (
        "PUT",
        "/api/knowledge/platform-exclusions",
        "knowledge_manager",
    ),
    (
        "DELETE",
        "/api/knowledge/platform-exclusions",
        "knowledge_manager",
    ),
    // B3 T6 (§5.2, §5.3): direct upload and ingest jobs
    (
        "POST",
        "/api/knowledge/stores/{id}/upload",
        "knowledge_manager",
    ),
    (
        "POST",
        "/api/knowledge/ingest-jobs/{id}/retry",
        "knowledge_manager",
    ),
    (
        "POST",
        "/api/knowledge/ingest-jobs/{id}/cancel",
        "knowledge_manager",
    ),
    (
        "DELETE",
        "/api/knowledge/ingest-jobs/{id}",
        "knowledge_manager",
    ),
    // B12 (B4 T12/T16/T17, B11 phase B); SW T4: Resolve replaces the free state route
    (
        "POST",
        "/api/jobs/{job}/handover/{id}/resolve",
        "support_agent",
    ),
    (
        "POST",
        "/api/jobs/{job}/cases/{id}/handover",
        "support_agent",
    ),
    // SW T2: Take / Assign… / Hand back (claim, take over, take next retired)
    (
        "POST",
        "/api/jobs/{job}/conversations/{id}/take",
        "support_agent",
    ),
    (
        "POST",
        "/api/jobs/{job}/conversations/{id}/assign",
        "support_manager",
    ),
    (
        "POST",
        "/api/jobs/{job}/conversations/{id}/hand-back",
        "support_agent",
    ),
    (
        "POST",
        "/api/jobs/{job}/channel-bindings",
        "integrations_manager",
    ),
    (
        "PUT",
        "/api/jobs/{job}/channel-bindings/{id}",
        "integrations_manager",
    ),
    (
        "DELETE",
        "/api/jobs/{job}/channel-bindings/{id}",
        "integrations_manager",
    ),
    // B13 §4: party identity
    ("POST", "/api/jobs/{job}/cases/{id}/party", "support_agent"),
    // B13 §4: per-job API keys
    ("POST", "/api/jobs/{job}/api-keys", "integrations_manager"),
    (
        "DELETE",
        "/api/jobs/{job}/api-keys/{id}",
        "integrations_manager",
    ),
    // B13 §4: rotateWebChatKey
    (
        "POST",
        "/api/jobs/{job}/channel-bindings/{id}/rotate-key",
        "integrations_manager",
    ),
    // B7 known-customer web chat: generate / rotate the binding's identity secret (shown once)
    (
        "POST",
        "/api/jobs/{job}/channel-bindings/{id}/identity-secret",
        "integrations_manager",
    ),
    // B7 T15: sendChannelTestMessage
    (
        "POST",
        "/api/jobs/{job}/channel-bindings/{id}/test-message",
        "integrations_manager",
    ),
    // admin test-as-customer on a TEST web chat
    (
        "POST",
        "/api/jobs/{job}/channel-bindings/{id}/test-session",
        "integrations_manager",
    ),
    // B4 T13: checkUnknownOutcome
    (
        "POST",
        "/api/jobs/{job}/operation-calls/{call_id}/check",
        "operator",
    ),
    // MCP tasks (crate::mcp_client): a person answers a task's question or cancels it
    (
        "POST",
        "/api/jobs/{job}/mcp-tasks/{task_id}/answers",
        "operator",
    ),
    (
        "POST",
        "/api/jobs/{job}/mcp-tasks/{task_id}/cancel",
        "operator",
    ),
    // B4 T14: claimStuck, replayDeadLetter, discardDeadLetter
    (
        "POST",
        "/api/jobs/{job}/streams/{key}/groups/{group}/claim",
        "operator",
    ),
    (
        "POST",
        "/api/jobs/{job}/dead-letters/{id}/replay",
        "operator",
    ),
    ("DELETE", "/api/jobs/{job}/dead-letters/{id}", "operator"),
    // B4 T16: saveHandoverPolicy, handoverFeedback, draftHandoverProposal (handoverFeedback: any staff role that works
    // a conversation since SW T4, see ANY_OF; an observer is refused with support_agent)
    ("PUT", "/api/jobs/{job}/handover/policy", "support_manager"),
    (
        "POST",
        "/api/jobs/{job}/handover/{id}/feedback",
        "support_agent",
    ),
    (
        "POST",
        "/api/jobs/{job}/handover/{id}/feedback/draft",
        "job_designer",
    ),
    // SW routing: round-robin setting, tenant_admin or support_manager (see ANY_OF; an observer is refused with
    // support_manager)
    ("PUT", "/api/support/routing", "support_manager"),
    // D125: the job's staff team (see ANY_OF; an observer is refused with support_manager)
    ("PUT", "/api/jobs/{job}/staff", "support_manager"),
    // B4 T16a: saveTenantCalendar
    ("PUT", "/api/tenant/calendar", "support_manager"),
    // B4 T18: saveTestCase (the demo writes are super_admin-only and listed nowhere)
    ("POST", "/api/jobs/{job}/test-cases", "job_designer"),
    // B5 T6 (J11): the ontology writes
    ("POST", "/api/jobs/{job}/ontology/entities", "job_designer"),
    (
        "POST",
        "/api/jobs/{job}/ontology/entities/{name}/synonyms",
        "job_designer",
    ),
    (
        "POST",
        "/api/jobs/{job}/ontology/entities/{name}/attributes",
        "job_designer",
    ),
    (
        "PUT",
        "/api/jobs/{job}/ontology/entities/{name}/hidden",
        "job_designer",
    ),
    ("POST", "/api/jobs/{job}/ontology/relations", "job_designer"),
    (
        "DELETE",
        "/api/jobs/{job}/ontology/relations",
        "job_designer",
    ),
    // B5 T7 (J12): createProcedureVersion, approveProcedure, setProcedureStatus (BY_BODY: → PUBLISHED is
    // release_manager's; an observer is refused job_designer)
    ("POST", "/api/jobs/{job}/procedures", "job_designer"),
    (
        "POST",
        "/api/jobs/{job}/procedures/{name}/versions/{v}/approve",
        "release_manager",
    ),
    (
        "POST",
        "/api/jobs/{job}/procedures/{name}/versions/{v}/status",
        "job_designer",
    ),
    // B5 T8/T8a (J7): gate library refinements, customer reply, per-job on/off
    ("POST", "/api/jobs/{job}/gates", "job_designer"),
    ("DELETE", "/api/jobs/{job}/gates/{gate}", "job_designer"),
    (
        "PUT",
        "/api/jobs/{job}/gates/{gate}/options/{option}",
        "job_designer",
    ),
    (
        "PUT",
        "/api/jobs/{job}/gates/{gate}/threshold",
        "job_designer",
    ),
    ("PUT", "/api/jobs/{job}/gates/{gate}/rule", "job_designer"),
    ("POST", "/api/jobs/{job}/gates/review", "job_designer"),
    (
        "PUT",
        "/api/jobs/{job}/gates/{gate}/customer-reply",
        "job_designer",
    ),
    (
        "PUT",
        "/api/jobs/{job}/gates/{gate}/enabled",
        "job_designer",
    ),
    // B5 T11/T11a (J8): toolset narrowings and per-job agent role on/off
    (
        "POST",
        "/api/jobs/{job}/toolsets/{role}/narrowings",
        "job_designer",
    ),
    (
        "DELETE",
        "/api/jobs/{job}/toolsets/{role}/narrowings",
        "job_designer",
    ),
    (
        "PUT",
        "/api/jobs/{job}/toolsets/{role}/enabled",
        "job_designer",
    ),
    // B8 T12/T12a (J17): the validation grid (grading and verdicts are release_manager's, questions job_designer's)
    (
        "POST",
        "/api/jobs/{job}/validation/{release}/run",
        "release_manager",
    ),
    (
        "PUT",
        "/api/jobs/{job}/validation/{release}/rows/{row}",
        "release_manager",
    ),
    (
        "POST",
        "/api/jobs/{job}/validation/questions",
        "job_designer",
    ),
    (
        "PATCH",
        "/api/jobs/{job}/validation/questions/{id}",
        "job_designer",
    ),
    (
        "DELETE",
        "/api/jobs/{job}/validation/questions/{id}",
        "job_designer",
    ),
    (
        "POST",
        "/api/jobs/{job}/validation/questions/import",
        "job_designer",
    ),
    // B8 T14a/T15a/T15 (J18): discovery imports, scorecard weights and shadow mode
    ("POST", "/api/jobs/{job}/discovery/imports", "job_designer"),
    (
        "POST",
        "/api/jobs/{job}/discovery/imports/preview",
        "job_designer",
    ),
    (
        "DELETE",
        "/api/jobs/{job}/discovery/imports/{id}",
        "job_designer",
    ),
    (
        "PUT",
        "/api/jobs/{job}/discovery/scorecard/{activity}",
        "job_designer",
    ),
    ("POST", "/api/jobs/{job}/discovery/shadow", "job_designer"),
    ("DELETE", "/api/jobs/{job}/discovery/shadow", "job_designer"),
    // SW T7: canned replies (support_manager writes) and presence (any staff role, see ANY_OF; an observer is refused
    // with support_agent)
    ("POST", "/api/jobs/{job}/canned-replies", "support_manager"),
    (
        "DELETE",
        "/api/jobs/{job}/canned-replies/{id}",
        "support_manager",
    ),
    (
        "POST",
        "/api/jobs/{job}/conversations/{id}/presence",
        "support_agent",
    ),
    // SW-9: staff attachments (a conversation worker, see ANY_OF; an observer is refused with support_agent)
    (
        "POST",
        "/api/jobs/{job}/conversations/{id}/attachments",
        "support_agent",
    ),
    (
        "DELETE",
        "/api/jobs/{job}/conversations/{id}/attachments/{att}",
        "support_agent",
    ),
    // B8: discovery proposals, the tenant side of the generator, the draft validation run
    (
        "POST",
        "/api/jobs/{job}/discovery/proposals/{id}/accept",
        "job_designer",
    ),
    (
        "POST",
        "/api/jobs/{job}/discovery/proposals/{id}/reject",
        "job_designer",
    ),
    (
        "POST",
        "/api/jobs/{job}/generator/runs/{id}/draft",
        "job_designer",
    ),
    (
        "POST",
        "/api/jobs/{job}/generator/runs/{id}/items/{item}/decision",
        "job_designer",
    ),
    (
        "PUT",
        "/api/jobs/{job}/generator/runs/{id}/stages/{stage}",
        "job_designer",
    ),
    (
        "POST",
        "/api/jobs/{job}/generator/runs/{id}/advance",
        "job_designer",
    ),
    (
        "POST",
        "/api/jobs/{job}/generator/runs/{id}/package",
        "job_designer",
    ),
    (
        "POST",
        "/api/jobs/{job}/validation/draft/run",
        "job_designer",
    ),
];

/// The agent-edge internal door (crate::conversations::internal): `EdgeKey` only, no person and no tenant role.
const INTERNAL: &[(&str, &str)] = &[
    ("POST", "/api/internal/channel/inbound"),
    ("POST", "/api/internal/channel/attachments"),
    ("POST", "/api/internal/channel/handover/notice-delivered"),
    ("POST", "/api/internal/channel/staff"),
    ("POST", "/api/internal/channel/staff-state"),
    ("POST", "/api/internal/channel/webchat/attachments"),
    ("POST", "/api/internal/channel/webchat/sessions"),
    ("POST", "/api/internal/channel/webchat/session-check"),
    ("POST", "/api/internal/channel/webhook-status"),
];

/// Service-account doors (the route checks the account itself; no person, no tenant role).
const SERVICE: &[(&str, &str)] = &[
    ("POST", "/api/signup-approvals/intake"),
    ("POST", "/api/signup-approvals/{id}/telegram-decision"),
];

/// A person's own view (D79): any member, always (it changes no tenant data).
const PERSONAL: &[(&str, &str)] = &[("PUT", "/api/me/view-as"), ("DELETE", "/api/me/view-as")];

/// A conversation with a job (crate::interaction and its MCP connector, crate::voice): any member of the job's
/// tenant, an observer included (it changes no tenant data).
const CONVERSATION: &[(&str, &str)] = &[
    ("POST", "/api/jobs/{job}/interactions"),
    ("POST", "/api/jobs/{job}/mcp"),
    ("POST", "/api/jobs/{job}/responses"),
    ("POST", "/api/jobs/{job}/chat/completions"),
    ("POST", "/api/jobs/{job}/voice/turns"),
    ("POST", "/api/jobs/{job}/conversations/{id}/csat"),
    // B4 T9: the staff test chat (route table "Chat start and send: any member"). The same path with a `kind` is SW
    // T3's Send, whose handler checks the role by kind (tests/support_messages.rs); the old reply routes are retired.
    ("POST", "/api/jobs/{job}/conversations"),
    ("POST", "/api/jobs/{job}/conversations/{id}/messages"),
    // B3 T11: a staff rating of one agent answer (operational data, not design; any member, like CSAT)
    ("POST", "/api/jobs/{job}/answer-feedback"),
];

/// A write whose role depends on its body: the route takes any member and the handler checks (§R.1 lists the role
/// an observer is refused with). B5 T7 setProcedureStatus: → PUBLISHED is release_manager's, every other move
/// job_designer's (fixture `setProcedureStatus`, web `roles.test.ts`).
const BY_BODY: &[(&str, &str)] = &[(
    "POST",
    "/api/jobs/{job}/procedures/{name}/versions/{v}/status",
)];

/// A write any of several roles may make: the route takes any member and the handler checks before the body is read
/// (§R.1 lists the role an observer is refused with). SW T4 handoverFeedback: `support_agent`, `support_manager`,
/// `operator` or `job_designer` (tests/support_ownership.rs `operator_can_give_feedback_cannot_resolve`).
/// SW T7 presence: the same staff roles (crate::support::require_worker). Take, Hand back, Resolve, Hand over case and
/// Link customer: `support_agent` or `support_manager` (owner decision 2026-10-26, `TenantCtx::require_conversation_worker`);
/// the roles listed in §R.1 stay `support_agent` (what an observer is refused with).
const ANY_OF: &[(&str, &str)] = &[
    ("POST", "/api/jobs/{job}/conversations/{id}/take"),
    ("POST", "/api/jobs/{job}/conversations/{id}/hand-back"),
    ("POST", "/api/jobs/{job}/handover/{id}/resolve"),
    ("POST", "/api/jobs/{job}/cases/{id}/handover"),
    ("POST", "/api/jobs/{job}/cases/{id}/party"),
    ("POST", "/api/jobs/{job}/handover/{id}/feedback"),
    ("POST", "/api/jobs/{job}/conversations/{id}/presence"),
    ("POST", "/api/jobs/{job}/conversations/{id}/attachments"),
    (
        "DELETE",
        "/api/jobs/{job}/conversations/{id}/attachments/{att}",
    ),
    // SW routing: tenant_admin or support_manager
    ("PUT", "/api/support/routing"),
    // D125: a job's staff team and routing, tenant_admin or support_manager
    ("PUT", "/api/jobs/{job}/staff"),
    // D126: a job's channel bindings, tenant_admin or integrations_manager (an observer is refused with
    // integrations_manager)
    ("POST", "/api/jobs/{job}/channel-bindings"),
    ("PUT", "/api/jobs/{job}/channel-bindings/{id}"),
    ("DELETE", "/api/jobs/{job}/channel-bindings/{id}"),
    ("POST", "/api/jobs/{job}/channel-bindings/{id}/rotate-key"),
    (
        "POST",
        "/api/jobs/{job}/channel-bindings/{id}/identity-secret",
    ),
    ("POST", "/api/jobs/{job}/channel-bindings/{id}/test-message"),
    ("POST", "/api/jobs/{job}/channel-bindings/{id}/test-session"),
];

/// B8 startRun: one door for both scopes, so the route takes any member and the handler checks by `scope` (a platform
/// run is super_admin's, a tenant run job_designer's).
const BY_SCOPE: &[(&str, &str)] = &[("POST", "/api/generator/runs")];

/// A dry run over a job's design (B2 §4 evaluateRule): any member of the job's tenant; it stores nothing.
const DRY_RUN: &[(&str, &str)] = &[
    ("POST", "/api/jobs/{job}/rules/{id}/evaluate"),
    // B3 T5: the query playground (member, stores nothing)
    ("POST", "/api/knowledge/query"),
    // B5 T10: the gate playground (member; asks Laya, writes one usage row, no verdict, no design change)
    ("POST", "/api/jobs/{job}/gates/evaluate"),
    // B8 T9a: the generator's cost estimate (member; reads usage, stores nothing)
    ("POST", "/api/generator/runs/estimate"),
];

async fn setup() -> (TestEnv, Upstream, Uuid) {
    let up = fakes::upstream().await;
    // signup on, so its routes are mounted and walked too
    let e = common::env_with(|v| fakes::signup_env(&up, v)).await;
    let t = Uuid::new_v4();
    insert_tenant(&e.owner, t, "Alpha").await;
    insert_user(&e.owner, "u-admin", t, "tenant_admin").await;
    insert_user(&e.owner, "u-obs", t, "tenant_user").await;
    sqlx::query(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, 'j1', 'J1', '[]', 'test')",
    )
    .bind(t)
    .execute(&e.owner)
    .await
    .unwrap();
    (e, up, t)
}

fn with_tenant(mut r: Request<Body>, t: Uuid) -> Request<Body> {
    r.headers_mut()
        .insert("x-active-tenant-id", t.to_string().parse().unwrap());
    r
}

/// The non-GET routes of the router.
fn writes() -> Vec<RouteDef> {
    router_walk::routes()
        .into_iter()
        .filter(|r| r.method != "GET")
        .collect()
}

/// The non-GET routes that demand a role, with that role.
fn role_routes() -> Vec<(RouteDef, String)> {
    writes()
        .into_iter()
        .filter_map(|r| {
            router_walk::guards(&r.params)
                .into_iter()
                .find_map(|g| match g {
                    Guard::Role(k) => Some(k),
                    _ => None,
                })
                .map(|k| (r, k))
        })
        .collect()
}

/// Send `{}` to `r` as `token` (a super_admin names tenant `t` by header).
async fn probe(
    e: &TestEnv,
    r: &RouteDef,
    token: &str,
    sa_tenant: Option<Uuid>,
) -> (StatusCode, Value) {
    probe_with(e, r, token, sa_tenant, json!({})).await
}

/// Send `body` to `r` as `token`.
async fn probe_with(
    e: &TestEnv,
    r: &RouteDef,
    token: &str,
    sa_tenant: Option<Uuid>,
    body: Value,
) -> (StatusCode, Value) {
    let uri = router_walk::fill(&r.path, "j1", "u-ghost");
    let mut req = json(&r.method, &uri, Some(token), body);
    if let Some(t) = sa_tenant {
        req = with_tenant(req, t);
    }
    send(&e.app, req).await
}

fn missing_role(b: &Value) -> Option<&str> {
    b["error"]["missing_role"].as_str()
}

async fn roles_of(db: &PgPool, sub: &str) -> Vec<String> {
    sqlx::query_scalar("SELECT roles FROM users WHERE sub = $1")
        .bind(sub)
        .fetch_one(db)
        .await
        .unwrap()
}

fn all() -> Vec<String> {
    ALL_ROLES.iter().map(|r| r.to_string()).collect()
}

#[tokio::test]
async fn migration_0039_backfills_admins_to_all_and_users_to_observer() {
    let url = common::fresh_database().await;
    let owner = PgPool::connect(&url).await.expect("owner pool");
    sqlx::migrate!("./migrations")
        .run_to(38, &owner)
        .await
        .expect("migrate to 0038");
    let t = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO tenants (id, name, residency, updated_by) VALUES ($1, 'Legacy', 'eu', 'test')",
    )
    .bind(t)
    .execute(&owner)
    .await
    .unwrap();
    for (sub, role) in [("u-admin", "tenant_admin"), ("u-user", "tenant_user")] {
        sqlx::query("INSERT INTO users (sub, tenant_id, tenant_role, updated_by) VALUES ($1, $2, $3, 'test')")
            .bind(sub)
            .bind(t)
            .bind(role)
            .execute(&owner)
            .await
            .unwrap();
    }
    owner.close().await;
    let e = common::env_for(url).await;
    assert_eq!(
        roles_of(&e.owner, "u-admin").await,
        all(),
        "a D55 admin keeps everything"
    );
    assert!(
        roles_of(&e.owner, "u-user").await.is_empty(),
        "a tenant_user is an observer"
    );
    let gone: bool = sqlx::query_scalar(
        "SELECT NOT EXISTS (SELECT 1 FROM information_schema.columns \
          WHERE table_name = 'users' AND column_name = 'tenant_role')",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(gone, "tenant_role is dropped");
    // the recreated functions read the new column
    let (tid, roles): (Uuid, Vec<String>) =
        sqlx::query_as("SELECT tenant_id, roles FROM agent_auth_user('u-admin')")
            .fetch_one(&e.state.db)
            .await
            .unwrap();
    assert_eq!((tid, roles), (t, all()));
    let (s, b) = send(&e.app, get("/api/me", Some(&person("u-user", &[])))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["roles"], json!([]));
}

#[tokio::test]
async fn roles_check_rejects_unknown_and_duplicate() {
    let (e, _up, t) = setup().await;
    for bad in [
        vec!["boss"],
        vec!["operator", "operator"],
        vec!["tenant_user"],
    ] {
        let err = sqlx::query(
            "INSERT INTO users (sub, tenant_id, roles, updated_by) VALUES ('u-bad', $1, $2, 'test')",
        )
        .bind(t)
        .bind(&bad)
        .execute(&e.owner)
        .await
        .unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("23514"), "{bad:?}: {err}");
    }
    assert_eq!(
        RoleSet::from_keys(&["boss"]).unwrap_err().code(),
        "invalid_role"
    );
    assert_eq!(
        RoleSet::from_keys(&["operator", "operator"])
            .unwrap_err()
            .code(),
        "duplicate_role"
    );
    assert_eq!(RoleSet::from_keys::<&str>(&[]).unwrap(), RoleSet::NONE);
    let admin = person("u-admin", &[]);
    for (roles, code) in [
        (json!(["boss"]), "invalid_role"),
        (json!(["operator", "operator"]), "duplicate_role"),
    ] {
        let (s, b) = send(
            &e.app,
            json(
                "POST",
                "/api/tenant/invites",
                Some(&admin),
                json!({"email": "x@y.test", "roles": roles}),
            ),
        )
        .await;
        assert_eq!(
            (s, b["error"]["code"].as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, Some(code)),
            "{b}"
        );
        let (s, b) = send(
            &e.app,
            json(
                "PATCH",
                "/api/tenant/users/u-obs",
                Some(&admin),
                json!({"roles": roles}),
            ),
        )
        .await;
        assert_eq!(
            (s, b["error"]["code"].as_str()),
            (StatusCode::UNPROCESSABLE_ENTITY, Some(code)),
            "{b}"
        );
    }
    // the vocabulary and its order are §R.1
    assert_eq!(RoleSet::ALL.keys(), ALL_ROLES);
    assert_eq!(Role::ALL.map(Role::key), ALL_ROLES);
    let (s, b) = send(&e.app, get("/api/roles", Some(&person("u-obs", &[])))).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let keys: Vec<&str> = b
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ALL_ROLES);
    assert_eq!(b[0]["label"], "Tenant admin");
    assert!(
        b.as_array()
            .unwrap()
            .iter()
            .all(|r| !r["description"].as_str().unwrap().is_empty())
    );
}

#[tokio::test]
async fn first_member_of_created_tenant_gets_all_roles() {
    let (e, up, _) = setup().await;
    // createTenant
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenants",
            Some(&super_admin()),
            json!({
                "name": "Createco", "first_admin_email": "first@createco.test", "residency": "eu",
                "first_admin_first_name": "First", "first_admin_last_name": "Admin",
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    let created: Uuid = b["tenant"]["id"].as_str().unwrap().parse().unwrap();
    // importTenant
    lock(&up.mock)
        .users
        .push(("kc-imp".into(), "imp@imp.test".into(), Map::new()));
    let imported = Uuid::new_v4();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenants/import",
            Some(&super_admin()),
            json!({"id": imported, "name": "Importco", "first_admin": "kc-imp", "residency": "us"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{b}");
    // signup approval
    let intake = service(
        "agent-signup-intake",
        &["agent-signup-intake"],
        &["agent-signup-intake"],
    );
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/signup-approvals/intake",
            Some(&intake),
            json!({"email": "ann@globex.com", "name": "Ann Early", "company": "Globex", "source": "web"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let id: Uuid = sqlx::query_scalar("SELECT id FROM signup_approvals WHERE status = 'pending'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            &format!("/api/signup-approvals/{id}/approve"),
            Some(&super_admin()),
            json!({"tenantName": "Globex Health", "residency": "eu"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let approved: Uuid = b["tenant"]["id"].as_str().unwrap().parse().unwrap();
    for t in [created, imported, approved] {
        let rows: Vec<(String, Vec<String>, String)> =
            sqlx::query_as("SELECT sub, roles, origin FROM users WHERE tenant_id = $1")
                .bind(t)
                .fetch_all(&e.owner)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1, "{t}: one first member");
        assert_eq!(rows[0].1, all(), "{t}: the first member holds every role");
        let details: Value = sqlx::query_scalar(
            "SELECT details FROM audit_event WHERE tenant_id = $1 AND action = 'user.add'",
        )
        .bind(t)
        .fetch_one(&e.owner)
        .await
        .unwrap();
        assert_eq!(details["roles"], json!(ALL_ROLES), "{t}");
        let (s, me) = send(&e.app, get("/api/me", Some(&person(&rows[0].0, &[])))).await;
        assert_eq!(s, StatusCode::OK, "{me}");
        assert_eq!(me["roles"], json!(ALL_ROLES));
        assert_eq!(me["effective_roles"], json!(ALL_ROLES));
    }
}

#[tokio::test]
async fn invite_carries_the_role_set() {
    let (e, _up, t) = setup().await;
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenant/invites",
            Some(&person("u-admin", &[])),
            json!({"email": "sam@alpha.test", "roles": ["support_agent", "job_designer"]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    let sub = b["sub"].as_str().unwrap().to_string();
    assert_eq!(
        roles_of(&e.owner, &sub).await,
        ["job_designer", "support_agent"],
        "stored in §R.1 order"
    );
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'user.invite' AND resource_id = $1",
    )
    .bind(&sub)
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(details["roles"], json!(["job_designer", "support_agent"]));
    let (s, b) = send(
        &e.app,
        get(
            &format!("/api/tenants/{t}/users"),
            Some(&person("u-admin", &[])),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let row = b["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["sub"] == sub.as_str())
        .unwrap()
        .clone();
    assert_eq!(row["roles"], json!(["job_designer", "support_agent"]));
    assert!(row.get("tenant_role").is_none());
    // an empty set is an observer
    let (s, b) = send(
        &e.app,
        json(
            "POST",
            "/api/tenant/invites",
            Some(&person("u-admin", &[])),
            json!({"email": "obs@alpha.test", "roles": []}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{b}");
    assert!(
        roles_of(&e.owner, b["sub"].as_str().unwrap())
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn set_user_role_replaces_the_set_and_audits_from_to() {
    let (e, _up, _t) = setup().await;
    insert_user(&e.owner, "u-sam", _t, "support_agent,operator").await;
    let admin = person("u-admin", &[]);
    let (s, b) = send(
        &e.app,
        json(
            "PATCH",
            "/api/tenant/users/u-sam",
            Some(&admin),
            json!({"roles": ["knowledge_manager", "job_designer"]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        b["roles"],
        json!(["job_designer", "knowledge_manager"]),
        "the whole set replaced"
    );
    let details: Value = sqlx::query_scalar(
        "SELECT details FROM audit_event WHERE action = 'user.role' AND resource_id = 'u-sam'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert_eq!(details["from"], json!(["support_agent", "operator"]));
    assert_eq!(details["to"], json!(["job_designer", "knowledge_manager"]));
    // the same set (in any order) is a no-op: no second audit row
    let (s, _) = send(
        &e.app,
        json(
            "PATCH",
            "/api/tenant/users/u-sam",
            Some(&admin),
            json!({"roles": ["job_designer", "knowledge_manager"]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_event WHERE action = 'user.role'")
        .fetch_one(&e.owner)
        .await
        .unwrap();
    assert_eq!(n, 1);
    // the empty set makes an observer
    let (s, b) = send(
        &e.app,
        json(
            "PATCH",
            "/api/tenant/users/u-sam",
            Some(&admin),
            json!({"roles": []}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["roles"], json!([]));
}

#[tokio::test]
async fn last_admin_guard_counts_held_roles_not_effective() {
    let (e, _up, _t) = setup().await;
    insert_user(&e.owner, "u-admin2", _t, "tenant_admin").await;
    // u-admin2 narrows to nothing: still a HELD admin
    let (s, b) = send(
        &e.app,
        json(
            "PUT",
            "/api/me/view-as",
            Some(&person("u-admin2", &[])),
            json!({"roles": []}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["effective_roles"], json!([]));
    // ...so u-admin may drop its own tenant_admin (another held admin remains)
    let (s, b) = send(
        &e.app,
        json(
            "PATCH",
            "/api/tenant/users/u-admin",
            Some(&person("u-admin", &[])),
            json!({"roles": ["job_designer"]}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "held roles count, not the view: {b}");
    // u-admin2 is now the last held admin: the view does not let it act as one ...
    let (s, b) = send(
        &e.app,
        json(
            "PATCH",
            "/api/tenant/users/u-admin2",
            Some(&person("u-admin2", &[])),
            json!({"roles": []}),
        ),
    )
    .await;
    assert_eq!(
        (s, missing_role(&b)),
        (StatusCode::FORBIDDEN, Some("tenant_admin")),
        "{b}"
    );
    // ... and once the view is cleared the guard keeps the last admin
    let (s, _) = send(
        &e.app,
        json(
            "DELETE",
            "/api/me/view-as",
            Some(&person("u-admin2", &[])),
            json!({}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, b) = send(
        &e.app,
        json(
            "PATCH",
            "/api/tenant/users/u-admin2",
            Some(&person("u-admin2", &[])),
            json!({"roles": []}),
        ),
    )
    .await;
    assert_eq!(
        (s, b["error"]["code"].as_str()),
        (StatusCode::CONFLICT, Some("last_tenant_admin")),
        "{b}"
    );
}

#[tokio::test]
async fn every_tenant_write_names_one_role() {
    let (e, _up, _t) = setup().await;
    let ws = writes();
    assert!(ws.len() >= 20, "the walk found the writes: {ws:#?}");
    let mut problems = Vec::new();
    for r in &ws {
        let key = (r.method.as_str(), r.path.as_str());
        let gs = router_walk::guards(&r.params);
        let roles: Vec<&String> = gs
            .iter()
            .filter_map(|g| match g {
                Guard::Role(k) => Some(k),
                _ => None,
            })
            .collect();
        let sa = gs.contains(&Guard::SuperAdmin);
        let listed = R1_B1
            .iter()
            .find(|(m, p, _)| (*m, *p) == key)
            .map(|(_, _, k)| *k);
        let what = format!("{} {} ({}::{})", r.method, r.path, r.file, r.handler);
        if BY_BODY.contains(&key) || ANY_OF.contains(&key) {
            if gs != [Guard::Member] || listed.is_none() {
                problems.push(format!(
                    "{what}: a body- or role-set-dependent write takes Member and is listed in §R.1, got {gs:?} / {listed:?}"
                ));
            }
        } else if PERSONAL.contains(&key)
            || CONVERSATION.contains(&key)
            || DRY_RUN.contains(&key)
            || BY_SCOPE.contains(&key)
        {
            if gs != [Guard::Member] {
                problems.push(format!(
                    "{what}: a personal or conversation route takes Member only, got {gs:?}"
                ));
            }
        } else if SERVICE.contains(&key) {
            if gs != [Guard::Service] {
                problems.push(format!(
                    "{what}: a service door takes ServiceClaims only, got {gs:?}"
                ));
            }
        } else if INTERNAL.contains(&key) {
            if gs != [Guard::Internal] {
                problems.push(format!(
                    "{what}: an internal door takes EdgeKey only, got {gs:?}"
                ));
            }
        } else if roles.len() == 1 && !sa {
            if listed != Some(roles[0].as_str()) {
                problems.push(format!(
                    "{what}: role {} but §R.1 says {listed:?}",
                    roles[0]
                ));
            }
            if Role::ALL.iter().all(|x| x.key() != roles[0].as_str()) {
                problems.push(format!("{what}: {} is not a §R.1 role", roles[0]));
            }
        } else if roles.is_empty() && sa {
            if listed.is_some() {
                problems.push(format!(
                    "{what}: super_admin-only but §R.1 names {listed:?}"
                ));
            }
        } else {
            problems.push(format!(
                "{what}: needs exactly one role or super_admin-only, got {gs:?}"
            ));
        }
    }
    for (m, p, _) in R1_B1 {
        if !ws.iter().any(|r| r.method == *m && r.path == *p) {
            problems.push(format!("{m} {p}: in §R.1 but not routed"));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
    // and the router agrees: an observer is refused every write that is not personal or a service door
    let obs = person("u-obs", &[]);
    let mut not_refused = Vec::new();
    for r in &ws {
        let key = (r.method.as_str(), r.path.as_str());
        if PERSONAL.contains(&key)
            || SERVICE.contains(&key)
            || CONVERSATION.contains(&key)
            || INTERNAL.contains(&key)
            || DRY_RUN.contains(&key)
            || BY_SCOPE.contains(&key)
        {
            continue;
        }
        // an ANY_OF handler checks the role before it reads the body, so it gets a body no input type fits: a typed
        // body read first answers 400, even one whose fields all default (which `{}` would satisfy)
        let body = if ANY_OF.contains(&key) {
            json!(0)
        } else {
            json!({})
        };
        let (s, b) = probe_with(&e, r, &obs, None, body).await;
        let want = R1_B1
            .iter()
            .find(|(m, p, _)| (*m, *p) == key)
            .map(|(_, _, k)| *k);
        let got = (s, b["error"]["code"].as_str(), missing_role(&b));
        let ok = match want {
            Some(k) => got == (StatusCode::FORBIDDEN, Some("forbidden"), Some(k)),
            None => got.0 == StatusCode::FORBIDDEN && got.1 == Some("super_admin_required"),
        };
        if !ok {
            not_refused.push(format!(
                "{} {}: want 403 {want:?}, got {b}",
                r.method, r.path
            ));
        }
    }
    // every route that is not refused, not only the first
    assert!(not_refused.is_empty(), "{not_refused:#?}");
}

#[tokio::test]
async fn observer_reads_but_every_write_is_403_missing_role() {
    let (e, _up, t) = setup().await;
    let obs = person("u-obs", &[]);
    for uri in [
        "/api/roles".to_string(),
        "/api/jobs".into(),
        "/api/jobs/j1".into(),
        "/api/tenant/overview".into(),
        "/api/tenant/home".into(),
        "/api/tenant/users/activity".into(),
        "/api/audit".into(),
        "/api/audit/summary".into(),
        format!("/api/tenants/{t}/users"),
        "/api/me".into(),
    ] {
        let (s, b) = send(&e.app, get(&uri, Some(&obs))).await;
        assert_eq!(s, StatusCode::OK, "{uri}: {b}");
    }
    // The router walk covers writes only; a role-gated READ is listed here. SW-6: the CSAT report (customer comments)
    // is support_manager / tenant_admin's, whatever the query says (tests/support_csat.rs has the rest).
    for uri in ["/api/support/csat", "/api/support/csat?from=garbage"] {
        let (s, b) = send(&e.app, get(uri, Some(&obs))).await;
        assert_eq!(
            (s, missing_role(&b)),
            (StatusCode::FORBIDDEN, Some("support_manager")),
            "{uri}: {b}"
        );
    }
    let (s, b) = send(
        &e.app,
        get("/api/support/csat", Some(&person("u-admin", &[]))),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let rr = role_routes();
    assert!(!rr.is_empty());
    for (r, k) in &rr {
        let (s, b) = probe(&e, r, &obs, None).await;
        assert_eq!(
            (s, missing_role(&b)),
            (StatusCode::FORBIDDEN, Some(k.as_str())),
            "{} {}: {b}",
            r.method,
            r.path
        );
    }
}

#[tokio::test]
async fn each_role_can_call_only_its_methods() {
    let (e, _up, t) = setup().await;
    let rr = role_routes();
    for role in Role::ALL {
        let sub = format!("u-only-{}", role.key());
        sqlx::query(
            "INSERT INTO users (sub, tenant_id, roles, updated_by) VALUES ($1, $2, ARRAY[$3::text], 'test')",
        )
        .bind(&sub)
        .bind(t)
        .bind(role.key())
        .execute(&e.owner)
        .await
        .unwrap();
        let tok = person(&sub, &[]);
        for (r, k) in &rr {
            let (s, b) = probe(&e, r, &tok, None).await;
            let refused = s == StatusCode::FORBIDDEN && missing_role(&b).is_some();
            // D127: a tenant_admin also holds the configuration roles
            let holds = RoleSet::of(&[role])
                .effective(None)
                .has(Role::parse(k).expect("role key"));
            assert_eq!(
                refused,
                !holds,
                "{} holding only {}: {} {} → {s} {b}",
                sub,
                role.key(),
                r.method,
                r.path
            );
        }
    }
}

#[tokio::test]
async fn sa_in_tenant_holds_all_roles() {
    let (e, _up, t) = setup().await;
    let sa = super_admin();
    let (s, b) = send(&e.app, with_tenant(get("/api/me", Some(&sa)), t)).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(b["roles"], json!(ALL_ROLES));
    assert_eq!(b["effective_roles"], json!(ALL_ROLES));
    for (r, _) in role_routes() {
        let (s, b) = probe(&e, &r, &sa, Some(t)).await;
        assert!(
            !(s == StatusCode::FORBIDDEN && missing_role(&b).is_some()),
            "{} {}: {s} {b}",
            r.method,
            r.path
        );
    }
}

#[tokio::test]
async fn no_route_grants_or_revokes_super_admin() {
    // Owner ruling D90: super_admin is granted by hand in Keycloak (docs/runbooks/super-admin-grant.md); the platform
    // has no door that writes a role mapping.
    let routes = router_walk::routes();
    assert!(!routes.is_empty());
    for r in &routes {
        let p = r.path.to_ascii_lowercase();
        assert!(
            !(p.contains("super") || p.contains("role-mapping") || p.contains("operator")),
            "{} {}: a route about super_admin or role mappings",
            r.method,
            r.path
        );
    }
    // Every handler and everything it calls lives in src/: the only role-mapping URL is the read in keycloak_admin.
    for (file, text) in router_walk::sources() {
        let n = text.matches("role-mappings").count();
        if file.ends_with("keycloak_admin.rs") {
            assert_eq!(
                n, 1,
                "{file}: one role-mapping URL (the has_client_role read)"
            );
        } else {
            assert_eq!(n, 0, "{file}: no role-mapping URL outside keycloak_admin");
        }
        for name in [
            "grant_client_role",
            "revoke_client_role",
            "add_client_role",
            "remove_client_role",
            "assign_role",
            "grant_super_admin",
            "revoke_super_admin",
        ] {
            assert!(!text.contains(name), "{file}: {name}");
        }
    }
}

#[test]
fn keycloak_admin_exposes_no_role_mapping_write() {
    let text = router_walk::squash(include_str!("../src/keycloak_admin.rs"));
    let fns: Vec<&str> = text.split("pub async fn ").skip(1).collect();
    assert!(fns.len() >= 5, "found the client's methods");
    let mut readers = 0;
    for f in fns {
        // a method's text runs to the next `pub async fn`
        if !f.contains("role-mappings") {
            continue;
        }
        readers += 1;
        let name: String = f
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        assert_eq!(
            name, "has_client_role",
            "only the read touches role mappings"
        );
        for write in [
            ".post(",
            ".put(",
            ".delete(",
            ".patch(",
            "Method::POST",
            "Method::DELETE",
        ] {
            assert!(
                !f.contains(write),
                "{name} sends {write} near a role mapping"
            );
        }
        assert!(f.contains("self.http.get("), "{name} reads with GET");
    }
    assert_eq!(readers, 1);
}
