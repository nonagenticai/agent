-- 0133 (KH T4, D108): the 26th layer-row kind `instruction` (one prompt slot of a template layer, key = the slot
-- `identity` | `policy` | `tone` | `safety` | `escalation` | `format`, fields `{text, language?, mode?}`; agent-model
-- `kinds::instruction`) and the refinement object `instruction` (a tenant's own text for `tone`, `escalation` or
-- `format`; the API refuses every other slot, 422 `instruction_not_refinable`).
--
-- The constraints are re-added by name with 0125's 25 kinds / 6 objects plus `instruction`; the conflict board's
-- object CHECK (0046) takes `instruction` too — a tenant override of a slot the next release changes is a
-- `base_changed` conflict — and `knowledge_pack`, which 0125 left out although a pack tombstone whose base a release
-- removes raises `base_removed`. Drift-tolerant (DROP CONSTRAINT IF EXISTS + ADD). Forward-only.
--
-- The tenant `preamble` override is retired (D108: `identity` is never tenant-refinable): its `job_setting_override`
-- rows and their open draft changes (`draft_ledger` key `setting:preamble`) are deleted, its open conflicts are
-- superseded, and
-- `job_setting_override_key_check` takes `actor` and `stance` only. Stored release snapshots are immutable and keep
-- theirs; restoring one writes nothing for `preamble` (`release::targets::RETIRED_SETTING_KEYS`).
--
-- No layer data is rewritten: the job `preamble` becomes the job layer's `identity` instruction in the compile (a `job`
-- row's `preamble` field, and a `job_setting` row keyed `preamble`, fold as an `identity` replace in their layer unless
-- that layer has its own `identity` row), so
-- stored rows and the immutable, content-hashed tags stay as they are. `job_setting` rows may now sit in every
-- template layer; the scope rule lives in agent-model (`Kind::scopes`), not in a CHECK.

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
            'instruction'
        )
    );

ALTER TABLE job_overlay_row
    DROP CONSTRAINT IF EXISTS job_overlay_row_object_check,
    ADD CONSTRAINT job_overlay_row_object_check
        CHECK (object IN ('activity', 'context_space', 'ontology_attribute', 'gate_option', 'knowledge_store',
                          'knowledge_pack', 'instruction'));

-- The object list is the full union with B5's 0141 (agreed 2026-10-23): whichever of KH 0133/0136/0138 and B5 0141 runs
-- last leaves the same check (production applies KH's after main's).
ALTER TABLE release_conflict
    DROP CONSTRAINT IF EXISTS release_conflict_object_check,
    ADD CONSTRAINT release_conflict_object_check
        CHECK (object IN ('activity', 'context_space', 'ontology_attribute', 'gate_option', 'knowledge_store',
                          'job_setting', 'knowledge_pack', 'instruction', 'knowledge_kind',
                          'entity', 'gate', 'gate_question', 'narrowing', 'procedure'));

DELETE FROM draft_ledger WHERE key = 'setting:preamble';
UPDATE release_conflict SET status = 'superseded'
 WHERE object = 'job_setting' AND key = 'preamble' AND status = 'open';
DELETE FROM job_setting_override WHERE key = 'preamble';

ALTER TABLE job_setting_override
    DROP CONSTRAINT IF EXISTS job_setting_override_key_check,
    ADD CONSTRAINT job_setting_override_key_check CHECK (key IN ('actor', 'stance'));
