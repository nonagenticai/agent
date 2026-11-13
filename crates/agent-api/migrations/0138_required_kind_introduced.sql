-- 0138 — KH T8 fix round 1 (D110): an upgrade that introduces a required knowledge kind is listed on the upgrade
-- surface. Forward-only, drift-tolerant (each CHECK is dropped and re-added by name).
--
-- `release::conflicts::raise_upgrade` records the informational finding `required_kind_introduced` (object
-- `knowledge_kind`, key `<department>:<stage>|<kind>`, no tenant row: `tenant_row = 'none'`) when a cut's new pins fold
-- a required kind the old pins did not. It never blocks and offers no resolution: readiness blocks the apply of that
-- release until the kind resolves (crate::knowledge::readiness). The lists carry 0136's values.
-- The object list is the full union with B5's 0141 (agreed 2026-10-23): whichever of KH 0133/0136/0138 and B5 0141 runs
-- last leaves the same check (production applies KH's after main's).
ALTER TABLE release_conflict
    DROP CONSTRAINT IF EXISTS release_conflict_kind_check,
    ADD CONSTRAINT release_conflict_kind_check
        CHECK (kind IN ('base_changed', 'base_removed', 'base_added', 'parent_missing', 'base_shadowed',
                        'job_setting', 'excluded_by_rule', 'required_kind_introduced')),
    DROP CONSTRAINT IF EXISTS release_conflict_object_check,
    ADD CONSTRAINT release_conflict_object_check
        CHECK (object IN ('activity', 'context_space', 'ontology_attribute', 'gate_option', 'knowledge_store',
                          'job_setting', 'knowledge_pack', 'instruction', 'knowledge_kind',
                          'entity', 'gate', 'gate_question', 'narrowing', 'procedure')),
    DROP CONSTRAINT IF EXISTS release_conflict_tenant_row_check,
    ADD CONSTRAINT release_conflict_tenant_row_check
        CHECK (tenant_row IN ('override', 'extension', 'tombstone', 'none'));
