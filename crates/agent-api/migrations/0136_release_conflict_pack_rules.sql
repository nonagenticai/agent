-- 0136 — KH T3 fix round 1 (D112): pack tombstones on the upgrade surface. Forward-only, drift-tolerant (each CHECK
-- is dropped and re-added by name).
--
-- `release::conflicts::OverlayDetector` reports a job's `knowledge_pack` tombstones at an upgrade: an exact or
-- wildcard tombstone whose packs the release removes without a `replaces` successor (`base_removed`, object
-- `knowledge_pack`), and the informational `excluded_by_rule` (a wildcard tombstone also excludes a pack the release
-- adds under it; never blocking, no resolution). 0046's CHECKs allowed neither the object nor the kind.
-- The object list also carries KH T4's `instruction` (0133 widens the same constraint), so either order of the two
-- migrations leaves both values allowed.
-- The object list is the full union with B5's 0141 (agreed 2026-10-23): whichever of KH 0133/0136/0138 and B5 0141 runs
-- last leaves the same check (production applies KH's after main's).
ALTER TABLE release_conflict
    DROP CONSTRAINT IF EXISTS release_conflict_kind_check,
    ADD CONSTRAINT release_conflict_kind_check
        CHECK (kind IN ('base_changed', 'base_removed', 'base_added', 'parent_missing', 'base_shadowed',
                        'job_setting', 'excluded_by_rule')),
    DROP CONSTRAINT IF EXISTS release_conflict_object_check,
    ADD CONSTRAINT release_conflict_object_check
        CHECK (object IN ('activity', 'context_space', 'ontology_attribute', 'gate_option', 'knowledge_store',
                          'job_setting', 'knowledge_pack', 'instruction', 'knowledge_kind',
                          'entity', 'gate', 'gate_question', 'narrowing', 'procedure'));
