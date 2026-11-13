-- 0222: the 30th layer-row kind `rule_slot` (a business-rule slot a super_admin authors on a template layer, key =
-- the rule id such as `RET-01`; fields in agent-model `kinds::rule_slot`). The compiled-in seed no longer carries
-- rule slots; the layer compiler folds these rows into `bundle.rules`, and a tenant fills the slot's values.
--
-- The constraint is re-added by name with 0191's 29 kinds plus `rule_slot`. Drift-tolerant (DROP CONSTRAINT IF
-- EXISTS + ADD). Forward-only. No overlay object is added: a tenant never refines a slot, it fills its values
-- (`business_rule.slot_values`).

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
            'journey_channel', 'stage_scope', 'use_case_card',
            'rule_slot'
        )
    );
