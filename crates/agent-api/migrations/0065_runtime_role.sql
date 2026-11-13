-- 0065 — the `agent_runtime` role's grants (B4 T3; plan Data model `0065`, R2 "a privilege split, like
-- `agent_platform`"). Forward-only (pre-prod), drift-tolerant (REVOKE ALL + GRANT, CREATE OR REPLACE FUNCTION).
--
-- The runtime processes (orchestrator, runner, interaction; `[[bin]]`s of this crate) connect as `agent_runtime`:
-- LOGIN, NOSUPERUSER, NOBYPASSRLS, a member of nothing. They read the design, providers and knowledge config of a
-- scoped job, and write cases, the timeline, operation calls, messages, handover items and usage — nothing on
-- authoring writes, platform writes, licence tokens or signup tables.
--
-- The role (pattern 0001 `agent_app`): production creates it LOGIN with its own secret in the DBA step on the
-- primary before this runs (gitops owner script, T20; never a password here). A server without it gets it NOLOGIN
-- here, so the grants below hold and the DBA step only adds LOGIN + password; a migration owner without CREATEROLE
-- skips creation and the GRANTs then fail loudly. Test binaries migrate many databases of one server concurrently,
-- so a lost CREATE ROLE race is not an error. An existing role that could bypass RLS refuses the migration.
--
-- Order (§0 R2): production already ran ≤0122 and applies this AFTER 0122; a fresh database applies it in number
-- order, before 0067 (`case_csat`), B7's 0091+ and B3's 0122 exist. So every object granted on here is ≤0065; the
-- grants on `case_csat`, `answer_grounding` (and later B7's `operation_route`) are 0123+.

DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'agent_runtime') THEN
        CREATE ROLE agent_runtime NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS;
    END IF;
EXCEPTION
    WHEN duplicate_object OR unique_violation THEN NULL;
    WHEN insufficient_privilege THEN
        RAISE NOTICE 'agent_runtime not created: the migration owner lacks CREATEROLE';
END $$;

DO $$
DECLARE
    r pg_roles%ROWTYPE;
BEGIN
    SELECT * INTO r FROM pg_roles WHERE rolname = 'agent_runtime';
    IF FOUND AND (r.rolsuper OR r.rolbypassrls OR r.rolcreaterole OR r.rolcreatedb) THEN
        RAISE EXCEPTION 'role agent_runtime must be NOSUPERUSER, NOBYPASSRLS, NOCREATEROLE and NOCREATEDB';
    END IF;
    IF FOUND AND EXISTS (SELECT 1 FROM pg_auth_members m WHERE m.member = r.oid) THEN
        RAISE EXCEPTION 'role agent_runtime must not be a member of any role';
    END IF;
END $$;

GRANT USAGE ON SCHEMA public TO agent_runtime;

-- Tenancy: the tenant row without its licence columns (ids come from agent_licence_attribution, never a token).
REVOKE ALL ON tenants, jobs FROM agent_runtime;
GRANT SELECT (id, name, status, created_at, updated_at, updated_by, suspended_at, archived_at, residency, brand,
              description, industry, platform_owned) ON tenants TO agent_runtime;
GRANT SELECT ON jobs TO agent_runtime;

-- The operation catalogue (B6a `compile_pinned`: platform template layers, read-only, as agent_app).
REVOKE ALL ON job_template_tag, layer, layer_row, layer_tag, layer_tag_row, layer_head, layer_untagged
    FROM agent_runtime;
GRANT SELECT ON job_template_tag, layer, layer_row, layer_tag, layer_tag_row, layer_head, layer_untagged
    TO agent_runtime;

-- The design (B2 `design::read`): live = release snapshot or baseline; draft = the working rows.
REVOKE ALL ON job_release, release_snapshot, release_baseline, release_bundled_draft,
              job_target, job_overlay_row, job_setting_override, business_rule FROM agent_runtime;
GRANT SELECT ON job_release, release_snapshot, release_baseline, release_bundled_draft,
                job_target, job_overlay_row, job_setting_override, business_rule TO agent_runtime;

-- Providers and knowledge config (B3 `providers::llm::resolve`, `knowledge::kinds::search`). Not
-- `provider_slot_secret`.
REVOKE ALL ON llm_provider, llm_use_binding, provider_slot, provider_slot_remote,
              knowledge_store, store_generation, job_knowledge_kind, platform_pack, job_embedding_pin
    FROM agent_runtime;
GRANT SELECT ON llm_provider, llm_use_binding, provider_slot, provider_slot_remote,
                knowledge_store, store_generation, job_knowledge_kind, platform_pack, job_embedding_pin
    TO agent_runtime;

-- Cases (0060). `env` and the identity columns are never updated. The design stamp (release_id, draft_rev) is set at
-- create and re-stamped once at plan time by agent_runtime (plan-time release wins; that UPDATE grant is in 0123).
REVOKE ALL ON agent_case, agent_case_seq, case_event FROM agent_runtime;
GRANT SELECT, INSERT ON agent_case TO agent_runtime;
GRANT UPDATE (status, outcome, escalation_reason, alert, activity, request_type, context_space, title,
              identity_level, customer_id, customer_name, procedure_name, procedure_version, op_calls,
              qualified_release_id, qualified_draft_rev, updated_at, resolved_at, duration_ms)
    ON agent_case TO agent_runtime;
-- agent_next_case_id() is SECURITY INVOKER: it upserts the counter as the caller
GRANT SELECT, INSERT, UPDATE ON agent_case_seq TO agent_runtime;
-- the timeline is append-only
GRANT INSERT ON case_event TO agent_runtime;
GRANT USAGE ON SEQUENCE case_event_seq_seq TO agent_runtime;

-- Conversations (0061): the runtime reads the conversation and its control row, appends messages.
REVOKE ALL ON conversation, conversation_message, conversation_control FROM agent_runtime;
GRANT SELECT ON conversation, conversation_control TO agent_runtime;
GRANT SELECT, INSERT ON conversation_message TO agent_runtime;

-- Operation calls (0062): the runner inserts a call and finishes it (the outcome columns only).
REVOKE ALL ON operation_call, operation_call_check FROM agent_runtime;
GRANT SELECT, INSERT ON operation_call TO agent_runtime;
GRANT UPDATE (state, outcome_code, finished_at, latency_ms, detail, object_change) ON operation_call TO agent_runtime;
GRANT SELECT, INSERT ON operation_call_check TO agent_runtime;

-- Handover (0063): the runtime queues items (stamping SLA due times) and may close them; `conversations::handover`
-- locks an open item FOR UPDATE, which needs an UPDATE privilege. agent_next_handover_id() is SECURITY INVOKER.
REVOKE ALL ON agent_handover_items, agent_handover_events, agent_handover_seq, agent_handover_feedback
    FROM agent_runtime;
GRANT SELECT, INSERT ON agent_handover_items TO agent_runtime;
GRANT UPDATE (state, closed_at, first_response_due, resolve_due, clock_starts_at)
    ON agent_handover_items TO agent_runtime;
GRANT INSERT ON agent_handover_events TO agent_runtime;
GRANT USAGE ON SEQUENCE agent_handover_events_seq_seq TO agent_runtime;
GRANT SELECT, INSERT, UPDATE ON agent_handover_seq TO agent_runtime;

-- Usage (0064): append-only ledger and its outbox row.
REVOKE ALL ON usage_event, metering_outbox, usage_daily FROM agent_runtime;
GRANT INSERT ON usage_event, metering_outbox TO agent_runtime;
GRANT USAGE ON SEQUENCE usage_event_id_seq, metering_outbox_id_seq TO agent_runtime;

-- Audit (0034): append; `audit::record` reads back only the id of its once-per-request row.
REVOKE ALL ON audit_event FROM agent_runtime;
GRANT INSERT ON audit_event TO agent_runtime;
GRANT SELECT (id, tenant_id, action, request_id) ON audit_event TO agent_runtime;
GRANT USAGE ON SEQUENCE audit_event_id_seq TO agent_runtime;

-- Every scope the runtime serves: each job of an active tenant × {prod, test} (pattern care-bus 0008). Read before
-- any tenant is known, so a definer function (the migration owner bypasses RLS on its own tables) instead of a
-- cross-tenant policy; ids and keys only.
CREATE OR REPLACE FUNCTION agent_all_scopes()
    RETURNS TABLE (tenant_id uuid, job_id uuid, job_key text, env text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT j.tenant_id, j.id, j.key, e.env
          FROM jobs j
          JOIN tenants t ON t.id = j.tenant_id
         CROSS JOIN (VALUES ('prod'), ('test')) AS e (env)
         WHERE t.status = 'active'
         ORDER BY j.tenant_id, j.key, e.env
    $$;
REVOKE ALL ON FUNCTION agent_all_scopes() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_all_scopes() TO agent_app, agent_runtime;

REVOKE ALL ON FUNCTION agent_next_case_id(), agent_next_handover_id(), agent_licence_attribution(text)
    FROM agent_runtime;
GRANT EXECUTE ON FUNCTION agent_next_case_id(), agent_next_handover_id(), agent_licence_attribution(text)
    TO agent_runtime;
