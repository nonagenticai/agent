//! B4 T3: the `agent_runtime` role (0065, 0123) and usage metering (0064). The runtime processes connect as
//! `agent_runtime`: LOGIN, NOSUPERUSER, NOBYPASSRLS, a member of nothing, with exactly the grants below. Real
//! Postgres 18 (one container for the `b4` binary, a fresh database per test).
use std::collections::BTreeSet;

use crate::common;

use crate::common::{insert_tenant, runtime_pool, sqlstate};
use agent_api::design::{self, CaseDesign, DesignOf};
use agent_api::runtime::design::RuntimeDesign;
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

/// One explicit privilege of `agent_runtime`: (kind, object, column or "", privilege).
pub(crate) type Grant = (String, String, String, String);

/// Every explicit privilege `agent_runtime` holds in `public` (tables, views and sequences; columns; functions).
/// Anything a new migration grants the runtime must be added here on purpose.
const EXPECTED: &[(&str, &str, &str, &str)] = &[
    ("column", "agent_case", "activity", "UPDATE"),
    ("column", "agent_case", "alert", "UPDATE"),
    ("column", "agent_case", "context_space", "UPDATE"),
    ("column", "agent_case", "customer_id", "UPDATE"),
    ("column", "agent_case", "customer_name", "UPDATE"),
    ("column", "agent_case", "draft_rev", "UPDATE"),
    ("column", "agent_case", "duration_ms", "UPDATE"),
    ("column", "agent_case", "escalation_reason", "UPDATE"),
    // 0131: the vendor ticket id of an instance case.create (F4)
    ("column", "agent_case", "external_ref", "UPDATE"),
    ("column", "agent_case", "op_calls", "UPDATE"),
    ("column", "agent_case", "outcome", "UPDATE"),
    ("column", "agent_case", "procedure_name", "UPDATE"),
    ("column", "agent_case", "procedure_version", "UPDATE"),
    ("column", "agent_case", "qualified_draft_rev", "UPDATE"),
    ("column", "agent_case", "qualified_release_id", "UPDATE"),
    ("column", "agent_case", "release_id", "UPDATE"),
    ("column", "agent_case", "request_type", "UPDATE"),
    ("column", "agent_case", "resolved_at", "UPDATE"),
    ("column", "agent_case", "status", "UPDATE"),
    ("column", "agent_case", "title", "UPDATE"),
    ("column", "agent_case", "updated_at", "UPDATE"),
    (
        "column",
        "agent_handover_items",
        "clock_starts_at",
        "UPDATE",
    ),
    ("column", "agent_handover_items", "closed_at", "UPDATE"),
    // 0200 (SW T1): a close records who closed the item (`handover::move_to`)
    ("column", "agent_handover_items", "closed_by", "UPDATE"),
    // 0248 (owner 2026-11-01): the identity upgrade runs in the runner's transaction and moves the conversation's
    // wire column to the subject, and fills the web chat session row (table-level: 0248 may run before 0183's columns)
    ("table", "web_chat_session", "", "SELECT"),
    ("table", "web_chat_session", "", "UPDATE"),
    // 0200 (SW T1): queueing an item makes the queue the conversation's owner (`ownership::set`)
    ("column", "conversation_control", "by_sub", "INSERT"),
    ("column", "conversation_control", "by_sub", "UPDATE"),
    (
        "column",
        "conversation_control",
        "conversation_id",
        "INSERT",
    ),
    ("column", "conversation_control", "job_id", "INSERT"),
    ("column", "conversation_control", "mode", "INSERT"),
    ("column", "conversation_control", "mode", "UPDATE"),
    ("column", "conversation_control", "since", "INSERT"),
    ("column", "conversation_control", "since", "UPDATE"),
    ("column", "conversation_control", "tenant_id", "INSERT"),
    (
        "column",
        "agent_handover_items",
        "first_response_due",
        "UPDATE",
    ),
    ("column", "agent_handover_items", "resolve_due", "UPDATE"),
    ("column", "agent_handover_items", "state", "UPDATE"),
    ("column", "audit_event", "action", "SELECT"),
    ("column", "audit_event", "id", "SELECT"),
    ("column", "audit_event", "request_id", "SELECT"),
    ("column", "audit_event", "tenant_id", "SELECT"),
    ("column", "case_csat", "answered_at", "UPDATE"),
    ("column", "case_csat", "rating", "UPDATE"),
    ("column", "connector_credential", "ciphertext", "SELECT"),
    (
        "column",
        "connector_credential",
        "credential_version",
        "SELECT",
    ),
    ("column", "connector_credential", "dek_version", "SELECT"),
    ("column", "connector_credential", "fingerprint", "SELECT"),
    ("column", "connector_credential", "instance_id", "SELECT"),
    ("column", "connector_credential", "nonce", "SELECT"),
    ("column", "connector_credential", "tenant_id", "SELECT"),
    ("column", "connector_credential", "updated_at", "SELECT"),
    ("column", "operation_call", "detail", "UPDATE"),
    ("column", "operation_call", "finished_at", "UPDATE"),
    // 0154: a finished write records the instance and template it ran on
    ("column", "operation_call", "instance_id", "UPDATE"),
    ("column", "operation_call", "latency_ms", "UPDATE"),
    ("column", "operation_call", "object_change", "UPDATE"),
    ("column", "operation_call", "outcome_code", "UPDATE"),
    // 0181: a re-armed write records the retry's route
    ("column", "operation_call", "route", "UPDATE"),
    ("column", "operation_call", "state", "UPDATE"),
    ("column", "operation_call", "template", "UPDATE"),
    // 0131: the case party's email identity for case.create's customer_email (F4)
    // 0179: the worker notify of a daemon-queued item (`handover::notify_workers`): the job's worker bindings
    // (never the sealed credential or the verify code) and the threads the worker channels open
    ("column", "channel_conversation_refs", "channel", "INSERT"),
    (
        "column",
        "channel_conversation_refs",
        "conversation_id",
        "INSERT",
    ),
    (
        "column",
        "channel_conversation_refs",
        "conversation_id",
        "UPDATE",
    ),
    (
        "column",
        "channel_conversation_refs",
        "external_id",
        "INSERT",
    ),
    ("column", "channel_conversation_refs", "item_id", "INSERT"),
    ("column", "channel_conversation_refs", "item_id", "UPDATE"),
    ("column", "channel_conversation_refs", "job_id", "INSERT"),
    ("column", "channel_conversation_refs", "route", "INSERT"),
    ("column", "channel_conversation_refs", "route", "UPDATE"),
    ("column", "channel_conversation_refs", "side", "INSERT"),
    ("column", "channel_conversation_refs", "tenant_id", "INSERT"),
    (
        "column",
        "channel_conversation_refs",
        "updated_at",
        "INSERT",
    ),
    (
        "column",
        "channel_conversation_refs",
        "updated_at",
        "UPDATE",
    ),
    ("column", "connector_bindings", "created_at", "SELECT"),
    ("column", "connector_bindings", "enabled", "SELECT"),
    ("column", "connector_bindings", "id", "SELECT"),
    ("column", "connector_bindings", "job_id", "SELECT"),
    ("column", "connector_bindings", "kind", "SELECT"),
    ("column", "connector_bindings", "settings", "SELECT"),
    ("column", "connector_bindings", "side", "SELECT"),
    ("column", "connector_bindings", "tenant_id", "SELECT"),
    ("column", "connector_bindings", "verified_at", "SELECT"),
    ("column", "party_identity", "channel", "SELECT"),
    ("column", "party_identity", "last_seen_at", "SELECT"),
    ("column", "party_identity", "party_id", "SELECT"),
    ("column", "party_identity", "subject", "SELECT"),
    ("column", "party_identity", "tenant_id", "SELECT"),
    ("column", "party_identity", "verified", "SELECT"),
    ("column", "platform_pack", "listed", "UPDATE"),
    ("column", "platform_pack", "unreadable_at", "UPDATE"),
    // 0128: the knowledge search's tenant supersedes (`kinds::tenant_supersedes`)
    ("column", "knowledge_upload", "store_id", "SELECT"),
    ("column", "knowledge_upload", "supersedes", "SELECT"),
    ("column", "knowledge_upload", "tenant_id", "SELECT"),
    ("column", "source", "enabled", "SELECT"),
    ("column", "source", "supersedes", "SELECT"),
    ("column", "source", "target_store_id", "SELECT"),
    ("column", "source", "tenant_id", "SELECT"),
    ("column", "provider_slot_secret", "created_at", "SELECT"),
    ("column", "provider_slot_secret", "fingerprint", "SELECT"),
    ("column", "provider_slot_secret", "id", "SELECT"),
    ("column", "provider_slot_secret", "tenant_id", "SELECT"),
    ("column", "provision_outbox", "created_at", "SELECT"),
    // KH T7 (0137): the knowledge search's store listing (`stores::all`, the `ensuring` flag)
    ("column", "provision_outbox", "dedupe_key", "SELECT"),
    ("column", "provision_outbox", "id", "SELECT"),
    ("column", "provision_outbox", "op", "SELECT"),
    ("column", "provision_outbox", "status", "SELECT"),
    ("column", "provision_outbox", "target", "SELECT"),
    ("column", "provision_outbox", "tenant_id", "SELECT"),
    ("column", "provision_state", "last_success_at", "SELECT"),
    ("column", "provision_state", "target", "SELECT"),
    ("column", "provision_state", "tenant_id", "SELECT"),
    ("column", "tenants", "archived_at", "SELECT"),
    ("column", "tenants", "brand", "SELECT"),
    ("column", "tenants", "created_at", "SELECT"),
    ("column", "tenants", "description", "SELECT"),
    ("column", "tenants", "id", "SELECT"),
    ("column", "tenants", "industry", "SELECT"),
    ("column", "tenants", "name", "SELECT"),
    ("column", "tenants", "platform_owned", "SELECT"),
    ("column", "tenants", "residency", "SELECT"),
    ("column", "tenants", "status", "SELECT"),
    ("column", "tenants", "suspended_at", "SELECT"),
    ("column", "tenants", "updated_at", "SELECT"),
    ("column", "tenants", "updated_by", "SELECT"),
    ("function", "agent_all_scopes()", "", "EXECUTE"),
    ("function", "agent_licence_attribution(text)", "", "EXECUTE"),
    // 0240: the MCP task poller's one cross-tenant claim (definer, runtime only)
    (
        "function",
        "agent_mcp_task_claim(integer,integer)",
        "",
        "EXECUTE",
    ),
    // 0209: the runner's licence gate (step 2) reads its tenant's verdict through this definer only
    ("function", "agent_runtime_licence_verdict()", "", "EXECUTE"),
    ("function", "agent_next_case_id()", "", "EXECUTE"),
    ("function", "agent_next_handover_id()", "", "EXECUTE"),
    ("function", "agent_tenant_dek()", "", "EXECUTE"),
    // 0240: the runner starts MCP tasks, polls them and ensures the tenant's MCP binding (sealed token)
    ("table", "mcp_task_answers", "", "INSERT"),
    ("table", "mcp_task_answers", "", "SELECT"),
    ("table", "mcp_task_answers", "", "UPDATE"),
    ("table", "mcp_tasks", "", "INSERT"),
    ("table", "mcp_tasks", "", "SELECT"),
    ("table", "mcp_tasks", "", "UPDATE"),
    ("table", "tenant_mcp_binding", "", "INSERT"),
    ("table", "tenant_mcp_binding", "", "SELECT"),
    ("table", "tenant_mcp_binding", "", "UPDATE"),
    ("table", "agent_case", "", "INSERT"),
    ("table", "agent_case", "", "SELECT"),
    ("table", "agent_case_seq", "", "INSERT"),
    ("table", "agent_case_seq", "", "SELECT"),
    ("table", "agent_case_seq", "", "UPDATE"),
    ("table", "agent_handover_events", "", "INSERT"),
    ("table", "agent_handover_events_seq_seq", "", "USAGE"),
    ("table", "agent_handover_items", "", "INSERT"),
    ("table", "agent_handover_items", "", "SELECT"),
    // 0200 (SW T1): the derived assignee and channel
    ("table", "agent_handover_items_owned", "", "SELECT"),
    ("table", "agent_handover_seq", "", "INSERT"),
    ("table", "agent_handover_seq", "", "SELECT"),
    ("table", "agent_handover_seq", "", "UPDATE"),
    ("table", "answer_grounding", "", "INSERT"),
    ("table", "answer_grounding", "", "SELECT"),
    ("table", "audit_event", "", "INSERT"),
    ("table", "audit_event_id_seq", "", "USAGE"),
    ("table", "business_rule", "", "SELECT"),
    ("table", "case_csat", "", "INSERT"),
    ("table", "case_csat", "", "SELECT"),
    ("table", "case_event", "", "INSERT"),
    ("table", "case_event_seq_seq", "", "USAGE"),
    ("table", "channel_conversation_refs", "", "SELECT"),
    ("table", "connector_instance", "", "SELECT"),
    ("table", "connector_template", "", "SELECT"),
    ("table", "connector_template_op", "", "SELECT"),
    ("table", "conversation", "", "SELECT"),
    ("table", "conversation_attachment", "", "SELECT"),
    ("table", "conversation_control", "", "SELECT"),
    ("table", "conversation_message", "", "INSERT"),
    ("table", "conversation_message", "", "SELECT"),
    ("table", "draft_ledger", "", "SELECT"),
    ("table", "field_mapping_override", "", "SELECT"),
    ("table", "handover_policy", "", "SELECT"),
    ("table", "job_embedding_pin", "", "SELECT"),
    ("table", "job_knowledge_kind", "", "SELECT"),
    ("table", "job_overlay_row", "", "SELECT"),
    ("table", "job_release", "", "SELECT"),
    ("table", "job_setting_override", "", "SELECT"),
    ("table", "job_target", "", "SELECT"),
    ("table", "job_template_tag", "", "SELECT"),
    ("table", "jobs", "", "SELECT"),
    ("table", "knowledge_store", "", "SELECT"),
    ("table", "layer", "", "SELECT"),
    ("table", "layer_head", "", "SELECT"),
    ("table", "layer_row", "", "SELECT"),
    ("table", "layer_tag", "", "SELECT"),
    ("table", "layer_tag_row", "", "SELECT"),
    ("table", "layer_untagged", "", "SELECT"),
    ("table", "llm_provider", "", "SELECT"),
    ("table", "llm_use_binding", "", "SELECT"),
    ("table", "metering_outbox", "", "INSERT"),
    ("table", "metering_outbox_id_seq", "", "USAGE"),
    ("table", "operation_call", "", "INSERT"),
    ("table", "operation_call", "", "SELECT"),
    ("table", "operation_call_check", "", "INSERT"),
    ("table", "operation_call_check", "", "SELECT"),
    ("table", "operation_route", "", "SELECT"),
    ("table", "platform_pack", "", "SELECT"),
    // 0128: the knowledge search's pack leaves (`kinds::cached_leaves`, `kinds::release_leaves`)
    ("table", "platform_pack_leaf", "", "SELECT"),
    ("table", "release_pack_leaf", "", "SELECT"),
    ("table", "provider_slot", "", "SELECT"),
    ("table", "provider_slot_remote", "", "SELECT"),
    ("table", "release_baseline", "", "SELECT"),
    ("table", "release_bundled_draft", "", "SELECT"),
    ("table", "release_snapshot", "", "SELECT"),
    ("table", "store_generation", "", "SELECT"),
    ("table", "tenant_connector_template", "", "SELECT"),
    ("table", "tenant_connector_template_op", "", "SELECT"),
    // 0066 (T16a): the runner's out-of-hours check reads the test scope's draft calendar (value columns only)
    ("column", "tenant_calendar", "holidays", "SELECT"),
    ("column", "tenant_calendar", "tenant_id", "SELECT"),
    ("column", "tenant_calendar", "timezone", "SELECT"),
    ("table", "usage_event", "", "INSERT"),
    ("table", "usage_event_id_seq", "", "USAGE"),
    // 0145 (B5 T3): the Test env's job-design working rows (`RuntimeDesign::read`) and the gate log (gate hooks)
    ("table", "gate_answer", "", "INSERT"),
    ("table", "gate_answer", "", "SELECT"),
    ("table", "gate_evaluation", "", "INSERT"),
    ("table", "gate_evaluation", "", "SELECT"),
    ("table", "gate_evaluation_id_seq", "", "USAGE"),
    ("table", "job_gate_option_refinement", "", "SELECT"),
    ("table", "job_gate_question", "", "SELECT"),
    ("table", "job_gate_question_setting", "", "SELECT"),
    ("table", "job_agent_role_setting", "", "SELECT"),
    ("table", "job_gate_rule_refinement", "", "SELECT"),
    ("table", "ontology_entity", "", "SELECT"),
    ("table", "ontology_entity_refinement", "", "SELECT"),
    ("table", "ontology_relation", "", "SELECT"),
    ("table", "procedure_version", "", "SELECT"),
    ("table", "toolset_narrowing", "", "SELECT"),
    // 0173 (B8 T15): the shadow hook reads `shadow_mode` and appends `discovery_observation` (`discovery::shadow`)
    ("table", "discovery_observation", "", "INSERT"),
    ("table", "discovery_observation_id_seq", "", "USAGE"),
    ("table", "shadow_mode", "", "SELECT"),
    // 0231 (A1): the conversation's and the subject's state — the runner reads it (step 3), stores outputs (3c) and
    // carries a value over to the subject
    ("table", "conversation_state", "", "DELETE"),
    ("table", "conversation_state", "", "INSERT"),
    ("table", "conversation_state", "", "SELECT"),
    ("table", "conversation_state", "", "UPDATE"),
    ("table", "subject_state", "", "DELETE"),
    ("table", "subject_state", "", "INSERT"),
    ("table", "subject_state", "", "SELECT"),
    ("table", "subject_state", "", "UPDATE"),
    // 0232 (A2/A3): the job's declared IAM procedures, tool policies and attachments — read only
    ("table", "iam_procedure", "", "SELECT"),
    ("table", "job_iam_attachment", "", "SELECT"),
    // 0246: the job's own operations (design kind `job_operation`, the Test design reads the working rows)
    ("table", "job_operation", "", "SELECT"),
    ("table", "job_tool_policy", "", "SELECT"),
];

/// The SECURITY DEFINER functions `agent_runtime` may execute (explicitly or through PUBLIC): exactly these.
const DEFINERS: &[&str] = &[
    "agent_all_scopes()",
    "agent_licence_attribution(text)",
    // 0240: the MCP task poller leases due tasks across tenants (returns only tenant, job and task id)
    "agent_mcp_task_claim(integer,integer)",
    // 0209: the caller's tenant verdict (status, reason, age), no licence id or token
    "agent_runtime_licence_verdict()",
    // B4 T6: the runner opens a live system's credential (0123)
    "agent_tenant_dek()",
];

/// The explicit privileges of `agent_runtime` in `db`'s `public` schema.
pub(crate) async fn runtime_grants(db: &PgPool) -> BTreeSet<Grant> {
    let rows: Vec<Grant> = sqlx::query_as(
        "SELECT 'table', c.relname::text, '', a.privilege_type::text \
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace, aclexplode(c.relacl) a \
          WHERE n.nspname = 'public' AND a.grantee = 'agent_runtime'::regrole \
         UNION ALL \
         SELECT 'column', c.relname::text, at.attname::text, a.privilege_type::text \
           FROM pg_attribute at JOIN pg_class c ON c.oid = at.attrelid \
           JOIN pg_namespace n ON n.oid = c.relnamespace, aclexplode(at.attacl) a \
          WHERE n.nspname = 'public' AND a.grantee = 'agent_runtime'::regrole AND NOT at.attisdropped \
         UNION ALL \
         SELECT 'function', p.oid::regprocedure::text, '', a.privilege_type::text \
           FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace, aclexplode(p.proacl) a \
          WHERE n.nspname = 'public' AND a.grantee = 'agent_runtime'::regrole",
    )
    .fetch_all(db)
    .await
    .expect("read agent_runtime grants");
    rows.into_iter().collect()
}

fn expected() -> BTreeSet<Grant> {
    EXPECTED
        .iter()
        .map(|&(k, o, c, p)| (k.to_owned(), o.to_owned(), c.to_owned(), p.to_owned()))
        .collect()
}

async fn insert_job(db: &PgPool, tenant: Uuid, key: &str) -> Uuid {
    sqlx::query_scalar(
        "INSERT INTO jobs (tenant_id, key, label, pins, created_by) VALUES ($1, $2, $2, '[]', 'test') RETURNING id",
    )
    .bind(tenant)
    .bind(key)
    .fetch_one(db)
    .await
    .unwrap()
}

/// A tenant (unique name) with one job; `(tenant, job)`.
async fn tenant_with_job(owner: &PgPool, label: &str) -> (Uuid, Uuid) {
    let t = Uuid::new_v4();
    insert_tenant(owner, t, &format!("B4 T3 {label} {t}")).await;
    let j = insert_job(owner, t, "rt_job").await;
    (t, j)
}

/// A transaction on `db` scoped to `tenant` and, when given, `job` (transaction-local GUCs, as the runtime does).
async fn scoped(db: &PgPool, tenant: Uuid, job: Option<Uuid>) -> Transaction<'static, Postgres> {
    let mut tx = db.begin().await.unwrap();
    sqlx::query("SELECT set_config('app.current_tenant', $1, true)")
        .bind(tenant.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    if let Some(j) = job {
        sqlx::query("SELECT set_config('app.current_job', $1, true)")
            .bind(j.to_string())
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    tx
}

const INSERT_USAGE: &str = "INSERT INTO usage_event \
    (job_id, event_key, occurred_at, source, use, actor, provider_id, model, funding, tokens_in, tokens_out) \
    VALUES ($1, $2, $3::timestamptz, $4, $5, 'staff-rt', 'openai', 'gpt', 'tenant_byo', $6, 1)";

#[tokio::test]
async fn runtime_role_grant_set_is_exact() {
    let e = common::env().await;
    let got = runtime_grants(&e.owner).await;
    let want = expected();
    let extra: Vec<_> = got.difference(&want).collect();
    let missing: Vec<_> = want.difference(&got).collect();
    assert!(
        extra.is_empty() && missing.is_empty(),
        "agent_runtime grants differ\nextra (granted, not expected): {extra:#?}\nmissing (expected, not granted): {missing:#?}"
    );
    // the role itself: may log in, bypasses nothing, inherits nothing
    let (login, sup, bypass, member_of): (bool, bool, bool, i64) = sqlx::query_as(
        "SELECT r.rolcanlogin, r.rolsuper, r.rolbypassrls, \
                (SELECT count(*) FROM pg_auth_members m WHERE m.member = r.oid) \
           FROM pg_roles r WHERE r.rolname = 'agent_runtime'",
    )
    .fetch_one(&e.owner)
    .await
    .unwrap();
    assert!(
        login && !sup && !bypass,
        "login={login} super={sup} bypassrls={bypass}"
    );
    assert_eq!(member_of, 0, "agent_runtime is a member of no role");
    // no SECURITY DEFINER function reaches the runtime through a PUBLIC default
    let definers: BTreeSet<String> = sqlx::query_scalar::<_, String>(
        "SELECT p.oid::regprocedure::text FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
          WHERE n.nspname = 'public' AND p.prosecdef \
            AND has_function_privilege('agent_runtime', p.oid, 'EXECUTE')",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap()
    .into_iter()
    .collect();
    let want_definers: BTreeSet<String> = DEFINERS.iter().map(|d| d.to_string()).collect();
    assert_eq!(
        definers, want_definers,
        "definer functions agent_runtime may execute"
    );
}

#[tokio::test]
async fn runtime_role_cannot_read_signup_or_licence_tables() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "deny").await;
    let rt = runtime_pool(&e.owner).await;
    for sql in [
        "SELECT count(*) FROM signup_approvals",
        "SELECT count(*) FROM signup_provisioning_steps",
        "SELECT count(*) FROM member_entitlement",
        "SELECT licence_token_encrypted FROM tenants",
        "SELECT licence_id FROM tenants",
        "SELECT * FROM tenants",
        "SELECT count(*) FROM licence_work_backoff",
        "SELECT count(*) FROM tenant_licence_verdict",
        "UPDATE tenant_licence_verdict SET status = 'active'",
        "SELECT count(*) FROM platform_audit",
        "SELECT key_sealed FROM provider_slot_secret",
        "SELECT * FROM provider_slot_secret",
        "SELECT count(*) FROM tenant_data_key",
        // B4 T6 (0123): the runner opens a live system's credential, but never its author, never a write
        "SELECT updated_by FROM connector_credential",
        "UPDATE connector_credential SET fingerprint = 'aaaaaaaa'",
        "SELECT agent_tenant_dek_put('\\x00'::bytea, 'k', 1)",
        "SELECT payload FROM provision_outbox",
        "UPDATE platform_pack SET root_id = gen_random_uuid()",
        // 0128: PK's leaf caches and the supersedes are read-only; a source's config and an upload's keys unreadable
        "DELETE FROM platform_pack_leaf",
        "INSERT INTO release_pack_leaf (job_id) VALUES (gen_random_uuid())",
        "SELECT * FROM source",
        "SELECT * FROM knowledge_upload",
        "DELETE FROM operation_route",
        "SELECT count(*) FROM agent_list_tenants()",
        "SELECT count(*) FROM usage_event",
        "SELECT count(*) FROM usage_daily",
        "UPDATE jobs SET draft_rev = draft_rev + 1",
        "UPDATE agent_case SET env = 'prod'",
        "UPDATE agent_case SET started_at = now()",
        "DELETE FROM draft_ledger",
        "UPDATE draft_ledger SET label = 'x'",
        "DELETE FROM case_event",
        "UPDATE operation_call SET initiator = 'x'",
        "INSERT INTO job_target (job_id) VALUES (gen_random_uuid())",
        // 0131: party_identity's lookup columns only; never the party, never a write
        "SELECT first_seen_at FROM party_identity",
        "SELECT * FROM party",
        "UPDATE party_identity SET verified = true",
        "UPDATE agent_case SET party_id = NULL",
        // 0179: a worker binding's target, never its sealed credential or verify code; never a binding write
        "SELECT secret_enc FROM connector_bindings",
        "SELECT verify_code FROM connector_bindings",
        "UPDATE connector_bindings SET enabled = false",
        "DELETE FROM channel_conversation_refs",
    ] {
        let mut tx = scoped(&rt, t, Some(j)).await;
        let err = sqlx::query(sql).execute(&mut *tx).await.unwrap_err();
        assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{sql}: {err}");
    }
    // control: the same connection reads what it is granted, scoped to its tenant
    let mut tx = scoped(&rt, t, Some(j)).await;
    let (id, status): (Uuid, String) = sqlx::query_as("SELECT id, status FROM tenants")
        .fetch_one(&mut *tx)
        .await
        .expect("agent_runtime reads the tenant row's granted columns");
    assert_eq!((id, status.as_str()), (t, "active"));
    // provider_slot_secret: the metadata columns read (design::diff's slot join), never key_sealed (denied above)
    sqlx::query("SELECT id, fingerprint, created_at FROM provider_slot_secret")
        .execute(&mut *tx)
        .await
        .expect("agent_runtime reads a slot key's metadata");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM jobs")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 1);
    tx.rollback().await.unwrap();
    // the scope registry: ids and keys of active tenants, both envs
    let scopes: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
        "SELECT tenant_id, job_id, job_key, env FROM agent_all_scopes() WHERE tenant_id = $1",
    )
    .bind(t)
    .fetch_all(&rt)
    .await
    .unwrap();
    assert_eq!(
        scopes,
        vec![
            (t, j, "rt_job".to_owned(), "prod".to_owned()),
            (t, j, "rt_job".to_owned(), "test".to_owned())
        ]
    );
    rt.close().await;
}

/// 0128: the runner's knowledge search reads PK's leaf sets and the tenant supersedes as agent_runtime (the exact
/// statements of `kinds::release_leaves`, `kinds::cached_leaves` and `kinds::tenant_supersedes`).
#[tokio::test]
async fn runtime_role_reads_the_pk_leaf_sets_and_supersedes() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "pk").await;
    let rt = runtime_pool(&e.owner).await;
    let mut tx = scoped(&rt, t, Some(j)).await;
    let pins = vec!["pack@1".to_owned()];
    let released: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT pin, leaf_id FROM release_pack_leaf \
          WHERE job_id = $1 AND release_id = $2 AND pin = ANY($3::text[]) ORDER BY pin, leaf_id",
    )
    .bind(j)
    .bind("cfg-2026-10-21.1")
    .bind(&pins)
    .fetch_all(&mut *tx)
    .await
    .expect("release_pack_leaf as agent_runtime");
    assert!(released.is_empty());
    let cached: Vec<(String, Uuid)> = sqlx::query_as(
        "SELECT pack_key || '@' || version::text AS pin, leaf_id FROM platform_pack_leaf \
          WHERE pack_key || '@' || version::text = ANY($1::text[]) ORDER BY pack_key, version, leaf_id",
    )
    .bind(&pins)
    .fetch_all(&mut *tx)
    .await
    .expect("platform_pack_leaf as agent_runtime");
    assert!(cached.is_empty());
    let sup = agent_api::knowledge::kinds::tenant_supersedes(&mut tx, Uuid::new_v4())
        .await
        .expect("tenant_supersedes as agent_runtime");
    assert!(sup.is_empty());
    tx.rollback().await.unwrap();
    rt.close().await;
}

#[tokio::test]
async fn usage_daily_buckets_utc() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "utc").await;
    let rt = runtime_pool(&e.owner).await;
    let mut tx = scoped(&rt, t, Some(j)).await;
    // a session far from UTC: 23:30Z and 00:30Z are the same local day there, two UTC days here
    sqlx::query("SET LOCAL TimeZone = 'Pacific/Auckland'")
        .execute(&mut *tx)
        .await
        .unwrap();
    for (at, tokens) in [
        ("2026-10-20T23:30:00Z", 10_i64),
        ("2026-10-21T00:30:00Z", 20),
    ] {
        sqlx::query(INSERT_USAGE)
            .bind(j)
            .bind(Uuid::new_v4())
            .bind(at)
            .bind("staff_test")
            .bind("assistant")
            .bind(tokens)
            .execute(&mut *tx)
            .await
            .expect("agent_runtime appends usage (use 'assistant' is a metering label)");
    }
    tx.commit().await.unwrap();
    rt.close().await;

    // read as agent_app (the report's role), again in a far-from-UTC session
    let mut tx = scoped(&e.state.db, t, Some(j)).await;
    sqlx::query("SET LOCAL TimeZone = 'Pacific/Auckland'")
        .execute(&mut *tx)
        .await
        .unwrap();
    let days: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT day::text, tokens_in, events FROM usage_daily WHERE use = 'assistant' ORDER BY day",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    assert_eq!(
        days,
        vec![
            ("2026-10-20".to_owned(), 10_i64, 1_i64),
            ("2026-10-21".to_owned(), 20_i64, 1_i64)
        ]
    );
    let project: String = sqlx::query_scalar("SELECT DISTINCT project FROM usage_event")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(project, t.to_string(), "project is the tenant id");
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn usage_without_job_only_outside_a_job_scope() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "nojob").await;
    let rt = runtime_pool(&e.owner).await;
    // inside a job scope a job-less row is refused by the RESTRICTIVE job_scope policy
    let mut tx = scoped(&rt, t, Some(j)).await;
    let err = sqlx::query(INSERT_USAGE)
        .bind(None::<Uuid>)
        .bind(Uuid::new_v4())
        .bind("2026-10-21T10:00:00Z")
        .bind("generator")
        .bind("generator")
        .bind(1_i64)
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    tx.rollback().await.unwrap();
    // outside any job scope it is admitted
    let mut tx = scoped(&rt, t, None).await;
    sqlx::query(INSERT_USAGE)
        .bind(None::<Uuid>)
        .bind(Uuid::new_v4())
        .bind("2026-10-21T10:00:00Z")
        .bind("generator")
        .bind("generator")
        .bind(1_i64)
        .execute(&mut *tx)
        .await
        .expect("a job-less row outside a job scope");
    tx.commit().await.unwrap();
    rt.close().await;
    // and a job-scoped reader does not see it, an unscoped one does
    let mut tx = scoped(&e.state.db, t, Some(j)).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_event")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 0);
    tx.rollback().await.unwrap();
    let mut tx = scoped(&e.state.db, t, None).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_event WHERE job_id IS NULL")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 1);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn attribution_never_returns_token() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "attr").await;
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, &format!("B4 T3 attr other {other}")).await;
    sqlx::query("UPDATE tenants SET licence_id = 'lic-tenant-a', licence_token_encrypted = 'v2:gone:TENANTTOKEN' WHERE id = $1")
        .bind(t)
        .execute(&e.owner)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO member_entitlement (tenant_id, user_sub, license_id, license_subject_id, token_encrypted, status) \
         VALUES ($1, 'rt-staff', 'lic-member', 'subj-member', 'v2:gone:MEMBERTOKEN', 'active'), \
                ($1, 'rt-pending', NULL, NULL, NULL, 'pending'), \
                ($2, 'rt-staff', 'lic-other', 'subj-other', 'v2:gone:OTHERTOKEN', 'active')",
    )
    .bind(t)
    .bind(other)
    .execute(&e.owner)
    .await
    .unwrap();
    // the function's result shape is ids only
    let cols: Vec<String> = sqlx::query_scalar(
        "SELECT unnest(proargnames[2:]) FROM pg_proc WHERE oid = 'agent_licence_attribution(text)'::regprocedure",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(cols, vec!["license_id", "subject_id"]);

    let rt = runtime_pool(&e.owner).await;
    let mut tx = scoped(&rt, t, Some(j)).await;
    let mut seen = Vec::new();
    for actor in ["rt-staff", "rt-pending", "customers:web_chat"] {
        let row: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT license_id, subject_id FROM agent_licence_attribution($1)")
                .bind(actor)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        seen.push(row);
    }
    assert_eq!(
        seen,
        vec![
            (
                Some("lic-member".to_owned()),
                Some("subj-member".to_owned())
            ),
            (Some("lic-tenant-a".to_owned()), None::<String>),
            (Some("lic-tenant-a".to_owned()), None::<String>),
        ],
        "member licence for an active member, else the tenant licence; never another tenant's"
    );
    for (l, s) in &seen {
        for v in [l, s].into_iter().flatten() {
            assert!(
                !v.contains("v2:") && !v.contains("TOKEN"),
                "a token leaked: {v}"
            );
        }
    }
    tx.rollback().await.unwrap();
    // unscoped: refused, never another tenant's licence
    let err = sqlx::query("SELECT * FROM agent_licence_attribution('rt-staff')")
        .execute(&rt)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err).as_deref(), Some("42501"), "{err}");
    rt.close().await;
}

/// 0209: the runner's licence gate reads the verdict of the scoped tenant only, as status / reason / mark / age, never
/// a licence id, plan, kid or token; another tenant's row and an unscoped call give nothing.
#[tokio::test]
async fn licence_verdict_definer_returns_own_tenant_only_and_no_token() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "verdict").await;
    let other = Uuid::new_v4();
    insert_tenant(&e.owner, other, &format!("B4 0209 other {other}")).await;
    for (tenant, status, reason, age) in [
        (t, "inactive", Some("license_inactive"), 42_i64),
        (other, "active", None, 1),
    ] {
        sqlx::query(
            "INSERT INTO tenant_licence_verdict (tenant_id, status, reason, licence_id, checked_at) \
             VALUES ($1, $2, $3, 'lic-SECRET', now() - make_interval(secs => $4))",
        )
        .bind(tenant)
        .bind(status)
        .bind(reason)
        .bind(age as f64)
        .execute(&e.owner)
        .await
        .unwrap();
    }
    let cols: Vec<String> = sqlx::query_scalar(
        "SELECT unnest(proargnames[1:]) FROM pg_proc WHERE oid = 'agent_runtime_licence_verdict()'::regprocedure",
    )
    .fetch_all(&e.owner)
    .await
    .unwrap();
    assert_eq!(
        cols,
        vec!["status", "reason", "platform_owned", "age_seconds"]
    );

    let rt = runtime_pool(&e.owner).await;
    let mut tx = scoped(&rt, t, Some(j)).await;
    let rows: Vec<(String, Option<String>, bool, i64)> = sqlx::query_as(
        "SELECT status, reason, platform_owned, age_seconds FROM agent_runtime_licence_verdict()",
    )
    .fetch_all(&mut *tx)
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(rows.len(), 1, "own tenant's row only: {rows:?}");
    let (status, reason, owned, age) = &rows[0];
    assert_eq!(
        (status.as_str(), reason.as_deref(), *owned),
        ("inactive", Some("license_inactive"), false)
    );
    assert!((42..=60).contains(age), "age in seconds: {age}");
    // unscoped: no tenant, no row
    let none: Vec<(String,)> = sqlx::query_as("SELECT status FROM agent_runtime_licence_verdict()")
        .fetch_all(&rt)
        .await
        .unwrap_or_default();
    assert!(none.is_empty(), "unscoped sees nothing: {none:?}");
    rt.close().await;
}

/// The runtime's write path on B12/B3 tables works with exactly its grants (0065 + 0123): case, timeline, agent
/// reply + its grounding (0122), CSAT request (0067), an operation call finished, a handover item queued.
#[tokio::test]
async fn runtime_role_writes_the_reply_path() {
    let e = common::env().await;
    let (t, j) = tenant_with_job(&e.owner, "path").await;
    sqlx::query(
        "INSERT INTO conversation (tenant_id, job_id, conversation_id, env, channel) \
         VALUES ($1, $2, 'conv-rt', 'test', 'staff')",
    )
    .bind(t)
    .bind(j)
    .execute(&e.owner)
    .await
    .unwrap();
    let rt = runtime_pool(&e.owner).await;
    let mut tx = scoped(&rt, t, Some(j)).await;
    let case: String = sqlx::query_scalar(
        "INSERT INTO agent_case (case_id, conversation_id, channel, env, draft_rev) \
         VALUES (agent_next_case_id(), 'conv-rt', 'staff', 'test', 1) RETURNING case_id",
    )
    .fetch_one(&mut *tx)
    .await
    .expect("insert a case");
    assert_eq!(case, "CS-1");
    sqlx::query("INSERT INTO case_event (case_id, kind) VALUES ($1, 'state')")
        .bind(&case)
        .execute(&mut *tx)
        .await
        .expect("append the timeline");
    let msg: Uuid = sqlx::query_scalar(
        "INSERT INTO conversation_message (conversation_id, role, text, at_ms, case_id) \
         VALUES ('conv-rt', 'agent', 'hello', 1, $1) RETURNING message_id",
    )
    .bind(&case)
    .fetch_one(&mut *tx)
    .await
    .expect("append the agent reply");
    sqlx::query(
        "INSERT INTO answer_grounding (job_id, message_id, used_knowledge, grounding) \
         VALUES ($1, $2, false, 'no_knowledge')",
    )
    .bind(j)
    .bind(msg)
    .execute(&mut *tx)
    .await
    .expect("record the reply's grounding (0123)");
    sqlx::query(
        "INSERT INTO case_csat (case_id, conversation_id, channel, env, draft_rev) \
         VALUES ($1, 'conv-rt', 'staff', 'test', 1) ON CONFLICT DO NOTHING RETURNING case_id",
    )
    .bind(&case)
    .fetch_optional(&mut *tx)
    .await
    .expect("request CSAT (0123)");
    sqlx::query(
        "INSERT INTO operation_call (call_id, case_id, operation, route, env, initiator, draft_rev) \
         VALUES ('opc_00000000-0000-7000-8000-0000000000aa', $1, 'order.read', 'instance', 'test', 'runner@rt', 1)",
    )
    .bind(&case)
    .execute(&mut *tx)
    .await
    .expect("insert an operation call");
    sqlx::query(
        "UPDATE operation_call SET state = 'ok', outcome_code = 'ok', finished_at = now(), latency_ms = 3 \
          WHERE call_id = 'opc_00000000-0000-7000-8000-0000000000aa'",
    )
    .execute(&mut *tx)
    .await
    .expect("finish the call");
    let item: String = sqlx::query_scalar(
        "INSERT INTO agent_handover_items (job_id, id, case_id, conversation_id, env, reason, draft_rev) \
         VALUES ($1, agent_next_handover_id(), $2, 'conv-rt', 'test', 'customer_asked', 1) RETURNING id",
    )
    .bind(j)
    .bind(&case)
    .fetch_one(&mut *tx)
    .await
    .expect("queue a handover item");
    sqlx::query(
        "INSERT INTO agent_handover_events (job_id, item_id, actor, to_state) VALUES ($1, $2, 'agent', 'queued')",
    )
    .bind(j)
    .bind(&item)
    .execute(&mut *tx)
    .await
    .expect("log the enqueue");
    let locked: Option<String> = sqlx::query_scalar(
        "SELECT id FROM agent_handover_items WHERE case_id = $1 \
            AND state NOT IN ('resolved', 'returned_to_agent') FOR UPDATE",
    )
    .bind(&case)
    .fetch_optional(&mut *tx)
    .await
    .expect("lock the open item (conversations::handover)");
    assert_eq!(locked.as_deref(), Some(item.as_str()));
    // 0200 (SW T1): queueing makes the queue the owner (`ownership::set`, an upsert) and the item's derived
    // assignee / channel are read through `agent_handover_items_owned`, with the item row locked alone
    // the exact text of `ownership::set`, twice: the second run takes the DO UPDATE path (it reads the existing row's
    // mode / by_sub / since), which needs the runtime's SELECT and UPDATE grants on conversation_control
    for round in ["insert or update", "do update"] {
        let done = sqlx::query(agent_api::conversations::ownership::SET_SQL)
            .bind("conv-rt")
            .bind("queue")
            .bind(None::<&str>)
            .execute(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("set the owner (conversations::ownership), {round}: {e}"));
        assert_eq!(done.rows_affected(), 1, "{round}");
    }
    let owned: (String, Option<String>) = sqlx::query_as(
        "SELECT o.channel, o.assignee FROM agent_handover_items h \
           JOIN agent_handover_items_owned o ON o.tenant_id = h.tenant_id AND o.job_id = h.job_id AND o.id = h.id \
          WHERE h.id = $1 FOR UPDATE OF h",
    )
    .bind(&item)
    .fetch_one(&mut *tx)
    .await
    .expect("read the owned item (conversations::handover)");
    assert_eq!(owned, ("staff".to_string(), None));
    sqlx::query("UPDATE agent_case SET status = 'handover', updated_at = now() WHERE case_id = $1")
        .bind(&case)
        .execute(&mut *tx)
        .await
        .expect("update the case status");
    tx.commit().await.unwrap();
    rt.close().await;
}

/// The runtime reads its design as `agent_runtime` (B4 T4 `RuntimeDesign::load`): Live (the applied release's
/// snapshot) and Draft (the working rows at `draft_rev`, the rule places through the pinned template compile), plus
/// the test-env trace's draft-ledger read (0123). A missing grant is a 42501 here, not a degraded design.
#[tokio::test]
async fn runtime_role_loads_live_and_draft_design() {
    let fx = crate::b4::design::setup_on(None, true).await;
    let at = crate::b4::design::put_target(&fx).await;
    crate::b4::design::put_languages(&fx, json!(["en", "de"])).await;
    let r1 = crate::b4::design::approve(&fx, &at, true).await;
    crate::b4::design::put_languages(&fx, json!(["en", "de", "fr"])).await;
    let rev = crate::b4::design::draft_rev(&fx).await;
    let rt = runtime_pool(&fx.e.owner).await;
    let cases = [
        (
            DesignOf::Live,
            vec!["en", "de"],
            CaseDesign::Live {
                release_id: Some(r1.clone()),
            },
        ),
        (
            DesignOf::Draft,
            vec!["en", "de", "fr"],
            CaseDesign::Draft { draft_rev: rev },
        ),
    ];
    for (of, langs, case) in cases {
        let mut tx = fx.f.job.begin(&rt).await.unwrap();
        let d = RuntimeDesign::load(&mut tx, &fx.e.state.design, &fx.f.job_ref(), &of)
            .await
            .unwrap_or_else(|e| panic!("agent_runtime loads {of:?}: {e:?}"));
        tx.commit().await.unwrap();
        assert_eq!(d.languages, langs, "{of:?}");
        assert_eq!(d.case, case, "{of:?}");
        assert!(
            d.template_errors.is_empty(),
            "{of:?}: {:?}",
            d.template_errors
        );
        assert!(
            d.procedures.iter().any(|p| p.name == "return_request"),
            "{of:?}: the pinned template compiled"
        );
        assert_eq!(d.selection.industry, "retail", "{of:?}");
        assert_eq!(
            d.selection.departments,
            ["industry_dept/retail/customer_service"],
            "{of:?}"
        );
    }
    // the trace's ledger read: the unpublished languages change is visible to the runtime
    let mut tx = fx.f.job.begin(&rt).await.unwrap();
    let keys = design::ledger::keys(&mut tx, &fx.f.job_ref())
        .await
        .expect("agent_runtime reads draft_ledger (0123)");
    tx.commit().await.unwrap();
    assert!(!keys.is_empty(), "the draft has an unpublished change");
    rt.close().await;
}
