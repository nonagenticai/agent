-- 0123 — `agent_runtime` grants on objects newer than 0065 (B4 T3, §0 R2). Forward-only (pre-prod), drift-tolerant
-- (REVOKE ALL + GRANT).
--
-- 0065 runs before 0067 and 0091–0122 on a fresh database, so the runtime's grants on those objects live here, after
-- every one of them in both orders:
--
-- - `case_csat` (0067): the interaction daemon requests CSAT after an agent-resolved case (`conversations::csat::
--   request`, `INSERT … RETURNING`); only the answer columns may change afterwards (as agent_app).
-- - `answer_grounding` (0122): the reply path writes the grounding row in the reply's transaction
--   (`knowledge::analytics::record_grounding`, §0 R3); written once, never changed.
--
-- Three grants on ≤0065 objects live here too (added after B4 T3, kept beside the other later runtime grants):
--
-- - `draft_ledger` (0044): the test-env reply trace (`runtime::trace::add_design`) reads the ledger
--   (`design::ledger::of_job` → draft fingerprint + change keys) and B2's `design::diff` (also `ledger::of_job`).
--   Read-only: the runtime never writes the design.
-- - `agent_case.release_id`, `draft_rev`: the orchestrator records the PLAN-time design on the case when it differs
--   from the qualify-time stamp (`runtime::orchestrator::record_plan`, first plan wins); `env` and the identity
--   columns stay unwritable.
-- - `provider_slot_secret` (0051), the metadata columns only: B2's `design::diff` (the trace) reads every
--   `provider_slot` place through `providers::slots::row`, which LEFT JOINs the secret for its fingerprint and age.
--   Never `key_sealed`: the runtime cannot read, let alone open, a key.
--
-- B4 T6, the operation runner (`runtime::runner`, connected as agent_runtime) on B7's connector objects (0091–0094,
-- agent#22) and B3's knowledge search; every grant read-only except the pack-cache mark. Audited call by call:
--
-- - `operation_route` (0094): R10 route resolution, `connectors::routes_store::effective` → `working_routes` (test
--   env; prod reads the release snapshot, already granted in 0065).
-- - `field_mapping_override` (0094): the test env's mapping overrides (`ConnectorExecutor::load_in`).
-- - `connector_instance` (0093): the routed system (`connectors::instances::one`), the stub checks
--   (`runtime::runner::ops::{has_demo_stub, is_stub}`).
-- - `connector_credential` (0093), columns: `instances::one`'s LEFT JOIN (fingerprint, credential_version,
--   updated_at) and `custody::open_credential` (ciphertext, nonce, dek_version) to call a live system. Not
--   `updated_by` (a person's sub). No write: the runtime never sets a credential.
-- - `agent_tenant_dek()` (0093, SECURITY DEFINER): the tenant's wrapped DEK that opens that credential
--   (`custody::tenant_dek(.., create = false)`). Never `agent_tenant_dek_put`: the runtime never mints a key.
-- - `connector_template`, `connector_template_op` (0091; platform data, no RLS) and `tenant_connector_template`,
--   `tenant_connector_template_op` (0092): the template the instance pins (`instances::pinned_template`).
-- - `provision_state`, `provision_outbox` (0035), columns: B3's data-plane gate before a `knowledge.*` search
--   (`strata::gate` → `provision::require_provisioned`). Never the payload, result or errors.
-- - `platform_pack` (0054) UPDATE (listed, unreadable_at): the search marks an unreadable pack root in the cache
--   (`knowledge::kinds::mark_unreadable`); its SELECT is 0065's.

-- route resolution (`connectors::routes_store::effective`, R10)
REVOKE ALL ON operation_route FROM agent_runtime;
GRANT SELECT ON operation_route TO agent_runtime;
-- the test env's mapping overrides (`ConnectorExecutor::load_in`)
REVOKE ALL ON field_mapping_override FROM agent_runtime;
GRANT SELECT ON field_mapping_override TO agent_runtime;
-- the routed system and the stub checks (`connectors::instances::one`, `runtime::runner::ops`)
REVOKE ALL ON connector_instance FROM agent_runtime;
GRANT SELECT ON connector_instance TO agent_runtime;
-- a live system's credential (`instances::one` join, `custody::open_credential`); never `updated_by`, never a write
REVOKE ALL ON connector_credential FROM agent_runtime;
GRANT SELECT (tenant_id, instance_id, ciphertext, nonce, credential_version, dek_version, fingerprint, updated_at)
    ON connector_credential TO agent_runtime;
-- the tenant DEK that opens it (`custody::tenant_dek`, read only)
REVOKE ALL ON FUNCTION agent_tenant_dek() FROM agent_runtime;
GRANT EXECUTE ON FUNCTION agent_tenant_dek() TO agent_runtime;
-- the pinned connector template, platform or tenant (`instances::pinned_template`)
REVOKE ALL ON connector_template, connector_template_op, tenant_connector_template, tenant_connector_template_op
    FROM agent_runtime;
GRANT SELECT ON connector_template, connector_template_op, tenant_connector_template, tenant_connector_template_op
    TO agent_runtime;
-- the knowledge search's data-plane gate (`provision::require_provisioned`): state and status columns only
REVOKE ALL ON provision_state, provision_outbox FROM agent_runtime;
GRANT SELECT (tenant_id, target, last_success_at) ON provision_state TO agent_runtime;
GRANT SELECT (id, tenant_id, target, status, created_at) ON provision_outbox TO agent_runtime;
-- the knowledge search's unreadable-pack mark (`knowledge::kinds::mark_unreadable`)
REVOKE UPDATE (listed, unreadable_at) ON platform_pack FROM agent_runtime;
GRANT UPDATE (listed, unreadable_at) ON platform_pack TO agent_runtime;

REVOKE ALL ON case_csat, answer_grounding FROM agent_runtime;
GRANT SELECT, INSERT ON case_csat TO agent_runtime;
GRANT UPDATE (rating, answered_at) ON case_csat TO agent_runtime;
GRANT SELECT, INSERT ON answer_grounding TO agent_runtime;

-- the trace's ledger read (`runtime::trace::add_design` → `design::ledger::of_job`, `design::diff`)
REVOKE ALL ON draft_ledger FROM agent_runtime;
GRANT SELECT ON draft_ledger TO agent_runtime;

-- the plan-time design stamp (`runtime::orchestrator::record_plan`)
REVOKE UPDATE (release_id, draft_rev) ON agent_case FROM agent_runtime;
GRANT UPDATE (release_id, draft_rev) ON agent_case TO agent_runtime;

-- the trace's provider slot places (`design::diff` → `providers::slots::row`): metadata, never `key_sealed`
REVOKE ALL ON provider_slot_secret FROM agent_runtime;
GRANT SELECT (tenant_id, id, fingerprint, created_at) ON provider_slot_secret TO agent_runtime;
