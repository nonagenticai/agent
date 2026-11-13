-- 0125 (PK T1, plan PK-1/PK-2): the 25th layer-row kind `knowledge_pack` (a template layer's reference to a strata
-- platform pack, key `<stage>|<kind>|<pack_key>`, fields `{version}`; agent-model `kinds::knowledge_pack`) and the
-- refinement object `knowledge_pack` (a job-level tombstone of an inherited pack reference).
--
-- Both constraints are re-added by name with 0041's 24 kinds / 0047's 5 objects plus `knowledge_pack`; B6b/B8 widen
-- the same names by DROP/ADD, so whichever lands later carries `knowledge_pack` too. References only 0041 and 0047
-- (on main). Drift-tolerant (DROP CONSTRAINT IF EXISTS + ADD). Forward-only.

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
            'knowledge_pack'
        )
    );

ALTER TABLE job_overlay_row
    DROP CONSTRAINT IF EXISTS job_overlay_row_object_check,
    ADD CONSTRAINT job_overlay_row_object_check
        CHECK (object IN ('activity', 'context_space', 'ontology_attribute', 'gate_option', 'knowledge_store',
                          'knowledge_pack'));
