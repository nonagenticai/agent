-- 0191 (B8 phase 2 T1, plan 2026-10-20 revision 2026-10-25 P1): the 27th-29th layer-row kinds `journey_channel`
-- (base/industry; a channel's status, F-49), `stage_scope` (industry/department layers; whether a stage is out of
-- scope, F-45) and `use_case_card` (industry/department/job layers, never base; the activity's card and scenarios,
-- B8-4). Field schemas and scope rules live in agent-model (`kinds::journey`, `Kind::scopes`), not in a CHECK.
--
-- The constraint is re-added by name with 0133's 26 kinds plus the three. Drift-tolerant (DROP CONSTRAINT IF EXISTS +
-- ADD). Forward-only. Nothing else enumerates the layer-row kinds: the conflict board and overlay object CHECKs name
-- refinement objects (`activity`, `instruction` …), and the three new kinds are authored on template layers only;
-- a tenant's card refinement lands with T7a (`job_activity_refinement.card`, 0105-range), not as an overlay object.

ALTER TABLE layer_row
    DROP CONSTRAINT IF EXISTS layer_row_kind_check,
    ADD CONSTRAINT layer_row_kind_check CHECK (
        kind IN (
            'entity', 'attribute', 'relation', 'job',
            'context_space', 'procedure', 'connector_slot', 'knowledge_store',
            'object', 'operation', 'error_state', 'gate_question', 'agent_role', 'profile', 'axis_rule',
            'department_archetype',
            'journey_theme', 'journey_stage', 'theme_stage_prior', 'stage_knowledge_kind', 'activity',
            'activity_object', 'operation_expectation',
            'job_setting',
            'knowledge_pack',
            'instruction',
            'journey_channel', 'stage_scope', 'use_case_card'
        )
    );
