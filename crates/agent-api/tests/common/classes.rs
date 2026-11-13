//! The scope class of every table in `public` (B1 T5, spec 01 §7 anti-drift). `tests/schema.rs`
//! `every_public_table_has_a_class` fails when a migration adds a table that is not listed here, or when a table
//! lacks the grants and row-level security its class demands. Later plans append their tables in the same commit
//! as the migration that creates them.
//!
//! - **Tenant**: RLS on with a policy (`tenant_id = app_current_tenant()`), `agent_app` may read it.
//! - **Platform**: no tenant column; `agent_app` may not write it (B1 X4: written only as `agent_platform`).
//! - **Definer**: reached only through SECURITY DEFINER functions: RLS on, no table grant to `agent_app` or
//!   `agent_platform`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    Tenant,
    Platform,
    Definer,
}

pub const CLASSES: &[(&str, Class)] = &[
    ("generator_evidence", Class::Platform),
    ("generator_evidence_row", Class::Platform),
    // 0001
    ("tenants", Class::Tenant),
    ("users", Class::Tenant),
    // 0030 (platform since 0031)
    ("signup_approvals", Class::Platform),
    ("signup_provisioning_steps", Class::Platform),
    // 0032 (B1 T5)
    ("super_admin_active", Class::Definer),
    ("tenant_offboard", Class::Tenant),
    ("member_session", Class::Tenant),
    // 0033 (B1 T6)
    ("jobs", Class::Tenant),
    ("job_template_tag", Class::Platform),
    ("template_upgrade_request", Class::Tenant),
    // 0034 (B1 T7)
    ("audit_event", Class::Tenant),
    ("platform_audit", Class::Platform),
    // 0035 (B1 T9)
    ("provision_outbox", Class::Tenant),
    ("provision_state", Class::Tenant),
    // 0037 (B1 T5, D77)
    ("member_entitlement", Class::Tenant),
    // 0038 (B1 review #2): the licence reconciler's per-item backoff, written as agent_platform
    ("licence_work_backoff", Class::Platform),
    // 0039 (B1 T21, D79): view-as
    ("member_view_as", Class::Tenant),
    ("super_admin_view_as", Class::Definer),
    // 0040 (B1, owner F-01): member invites
    ("member_invites", Class::Tenant),
    // 0041 (B6a T4): the platform layer store
    ("layer", Class::Platform),
    ("layer_row", Class::Platform),
    ("layer_tag", Class::Platform),
    ("layer_tag_row", Class::Platform),
    // 0042 (B6a T4): the platform vocabularies
    ("axis", Class::Platform),
    ("axis_value", Class::Platform),
    ("axis_rule", Class::Platform),
    ("dimension", Class::Platform),
    ("dimension_value", Class::Platform),
    ("renewal_preset", Class::Platform),
    ("folded_concept", Class::Platform),
    ("compile_reference_seed", Class::Platform),
    // 0044 (B2 T2): the design ledger
    ("draft_ledger", Class::Tenant),
    ("release_baseline", Class::Tenant),
    // 0045 (B2 T2): releases
    ("job_release", Class::Tenant),
    ("release_snapshot", Class::Tenant),
    ("release_bundled_draft", Class::Tenant),
    ("published_change", Class::Tenant),
    ("release_rollback", Class::Tenant),
    // 0046 (B2 T2): release conflicts
    ("release_conflict", Class::Tenant),
    // 0047 (B2 T2): B2's working rows
    ("job_target", Class::Tenant),
    ("job_target_history", Class::Tenant),
    ("job_overlay_row", Class::Tenant),
    ("job_setting_override", Class::Tenant),
    ("business_rule", Class::Tenant),
    ("business_rule_history", Class::Tenant),
    // 0050 (B3 T3): LLM providers and their use bindings
    ("llm_provider", Class::Tenant),
    ("llm_use_binding", Class::Tenant),
    // 0051 (B3 T4): provider slots, their sealed keys and what strata holds
    ("provider_slot", Class::Tenant),
    ("provider_slot_secret", Class::Tenant),
    ("provider_slot_remote", Class::Tenant),
    // 0052 (B3 T5): the knowledge-store tree and direct uploads
    ("knowledge_store", Class::Tenant),
    ("knowledge_upload", Class::Tenant),
    // 0053 (B3 T7): a job's knowledge sources and their re-sync runs
    ("source", Class::Tenant),
    ("source_run", Class::Tenant),
    // 0054 (B3 T10): knowledge-kind bindings and the platform-pack cache
    ("job_knowledge_kind", Class::Tenant),
    ("platform_pack", Class::Tenant),
    // 0122 (B3 T11): the grounding of agent replies and staff ratings of them (keyed to conversation_message)
    ("answer_grounding", Class::Tenant),
    ("answer_feedback", Class::Tenant),
    // 0056 (B3 T4a): store generations, job pins and model changes (D83/D93)
    ("store_generation", Class::Tenant),
    ("job_embedding_pin", Class::Tenant),
    ("model_change", Class::Tenant),
    ("model_change_job", Class::Tenant),
    // 0090 (B7 T2): tenant licence verdicts and the reseal-run log
    ("tenant_licence_verdict", Class::Tenant),
    ("tenant_app_licence", Class::Tenant),
    ("tenant_mcp_binding", Class::Tenant),
    ("mcp_tasks", Class::Tenant),
    ("mcp_task_answers", Class::Tenant),
    ("licence_reseal_run", Class::Platform),
    // 0091 (B7 T4): platform connector templates
    ("connector_template", Class::Platform),
    ("connector_template_op", Class::Platform),
    // 0092 (B7 T11): tenant-private connector templates
    ("tenant_connector_template", Class::Tenant),
    ("tenant_connector_template_op", Class::Tenant),
    // 0093 (B7 T8): tenant connector instances and credential custody
    ("tenant_data_key", Class::Definer),
    ("connector_instance", Class::Tenant),
    ("connector_credential", Class::Tenant),
    ("connector_contract_result", Class::Tenant),
    ("connector_sample", Class::Tenant),
    // 0094 (B7 T12/T13): draft operation routes and mapping overrides
    ("operation_route", Class::Tenant),
    ("field_mapping_override", Class::Tenant),
    // 0096 (B7 T3c, R2): the revoked-licence tombstone and the re-issue history
    ("licence_revocation", Class::Tenant),
    ("licence_reissue", Class::Platform),
    // 0060 (B12 / B4 T2): runtime cases
    ("agent_case", Class::Tenant),
    ("agent_case_seq", Class::Tenant),
    ("case_event", Class::Tenant),
    // 0061 (B12 / B4 T3): conversations
    ("conversation", Class::Tenant),
    ("conversation_message", Class::Tenant),
    ("conversation_control", Class::Tenant),
    ("test_case", Class::Tenant),
    // 0062 (B4 T2): operation calls and operator checks of unknown outcomes
    ("operation_call", Class::Tenant),
    ("operation_call_check", Class::Tenant),
    // 0063 (B12 / B4 T2): the handover queue
    ("agent_handover_items", Class::Tenant),
    ("agent_handover_events", Class::Tenant),
    ("agent_handover_feedback", Class::Tenant),
    ("agent_handover_seq", Class::Tenant),
    // 0064 (B4 T3): usage ledger and metering outbox
    ("usage_event", Class::Tenant),
    ("metering_outbox", Class::Tenant),
    // 0067 (B12 / B4 T16b): CSAT
    ("case_csat", Class::Tenant),
    // 0120, 0121 (B12 §2): channel bindings and provider conversation refs
    ("connector_bindings", Class::Tenant),
    ("channel_webhook_status", Class::Tenant),
    ("channel_conversation_refs", Class::Tenant),
    // 0112 (B13 §2): party identity
    ("party", Class::Tenant),
    ("party_identity", Class::Tenant),
    // 0113 (B13 §2): per-job API keys
    ("job_api_keys", Class::Tenant),
    // 0114 (B13 §2): durable async interactions
    ("interaction_deliveries", Class::Tenant),
    // 0124 (B13 §2): web chat sessions; the durable rate counters and abuse events (definer functions only)
    ("web_chat_session", Class::Tenant),
    ("channel_rate_counter", Class::Definer),
    ("channel_abuse_event", Class::Definer),
    // 0183 (B4 known customer): single-use assertion ids, definer function only
    ("webchat_assertion_jti", Class::Definer),
    // 0126 (PK T1): the tenant's pack-leaf cache and the pack leaves a release froze
    ("platform_pack_leaf", Class::Tenant),
    ("release_pack_leaf", Class::Tenant),
    // 0115 (B13 live acceptance): names of staff who are not tenant members (definer functions only)
    ("actor_names", Class::Definer),
    // 0069 (B4 T16): the working row of ledger kind handover_policy
    ("handover_policy", Class::Tenant),
    // 0066 (B4 T16a): the working row of tenant-scope ledger kind tenant_calendar
    ("tenant_calendar", Class::Tenant),
    // 0140 (B5 T2): tenant ontology rows (bases are read from compile_pinned, never stored)
    ("ontology_entity", Class::Tenant),
    ("ontology_entity_refinement", Class::Tenant),
    ("ontology_relation", Class::Tenant),
    // 0142 (B5 T2): tenant procedure versions and their append-only history
    ("procedure_version", Class::Tenant),
    ("procedure_history", Class::Tenant),
    // 0143 (B5 T3): tenant gate questions, refinements and the gate log
    ("job_gate_question", Class::Tenant),
    ("job_gate_question_setting", Class::Tenant),
    ("job_agent_role_setting", Class::Tenant),
    ("job_gate_option_refinement", Class::Tenant),
    ("job_gate_rule_refinement", Class::Tenant),
    ("gate_evaluation", Class::Tenant),
    ("gate_answer", Class::Tenant),
    // 0144 (B5 T3): toolset narrowings
    ("toolset_narrowing", Class::Tenant),
    // 0171 (B8 T8/T9): the journey generator's runs (platform runs live under the platform-owned tenant)
    ("generator_run", Class::Tenant),
    ("generator_step", Class::Tenant),
    ("generator_stage", Class::Tenant),
    ("generator_item", Class::Tenant),
    ("generator_cost_line", Class::Tenant),
    // 0172 (B8 T12): validation questions, results, cut runs and draft test runs
    ("validation_question", Class::Tenant),
    ("validation_result", Class::Tenant),
    ("validation_cut_run", Class::Tenant),
    ("draft_test_run", Class::Tenant),
    // 0173 (B8 T14a/T15): discovery imports, their rows/clusters/decisions, shadow mode and its observations
    ("discovery_import", Class::Tenant),
    ("discovery_row", Class::Tenant),
    ("discovery_cluster", Class::Tenant),
    ("discovery_decision", Class::Tenant),
    ("shadow_mode", Class::Tenant),
    ("discovery_observation", Class::Tenant),
    // 0177 (B8 T12a): validation grid runs
    ("validation_grid_run", Class::Tenant),
    // 0178 (B8 T15a): discovery scorecard weights
    ("discovery_weight", Class::Tenant),
    // 0193 (SW T7): a job's canned replies
    ("support_canned_reply", Class::Tenant),
    // 0204 (SW routing): the per-tenant setting and who was handed an item last
    ("support_routing", Class::Tenant),
    ("support_routing_member", Class::Tenant),
    // 0213 (TJ, D125): a job's staff team and its routing mode
    ("job_staff", Class::Tenant),
    ("job_routing", Class::Tenant),
    // 0205 (SW-9): photos / PDFs on a conversation (0223: metadata only; the bytes live in artifact_service)
    ("conversation_attachment", Class::Tenant),
    // 0223: the remote-deletion outbox of attachment bytes (artifact_service), reached through definer functions
    ("conversation_attachment_tombstone", Class::Definer),
    // 0223: the committed intent of an upload in flight (resolved by the sweep when the process died before the row)
    ("conversation_attachment_upload_intent", Class::Definer),
    // 0231/0232 (caller identity plan A1, A2/A3): conversation state, tenant IAM procedures and their job rows
    ("conversation_state", Class::Tenant),
    ("subject_state", Class::Tenant),
    ("iam_procedure", Class::Tenant),
    ("job_tool_policy", Class::Tenant),
    ("job_iam_attachment", Class::Tenant),
    // 0246: the ops a job adds from its tenant's connector templates (design kind `job_operation`)
    ("job_operation", Class::Tenant),
];

/// The class of `table`, if listed.
pub fn class_of(table: &str) -> Option<Class> {
    CLASSES.iter().find(|(t, _)| *t == table).map(|(_, c)| *c)
}
