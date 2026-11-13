-- 0141 — B5 release conflicts (plan 2026-10-20-b5-job-design, T4 `JobDesignDetector`): widen
-- `release_conflict_object_check` (0046) with the objects B5's detector raises conflicts on — `entity`, `gate`,
-- `gate_question`, `narrowing`, `procedure` — keeping every B2 value. Without it an apply or cut whose B5 rows
-- conflict with the new bases fails with 23514.
--
-- The list is the UNION with KH's values (`knowledge_pack`, `instruction`, `knowledge_kind` from KH 0133/0136/0138), so whichever of B5/KH
-- lands last keeps every value; KH's migrations carry B5's five values too (agreed with the KH lane 2026-10-23).
--
-- Drift-tolerant as 0046 (DROP … IF EXISTS + ADD). Forward-only.

ALTER TABLE release_conflict
    DROP CONSTRAINT IF EXISTS release_conflict_object_check,
    ADD CONSTRAINT release_conflict_object_check
        CHECK (object IN ('activity', 'context_space', 'ontology_attribute', 'gate_option', 'knowledge_store',
                          'job_setting', 'knowledge_pack', 'instruction', 'knowledge_kind',
                          'entity', 'gate', 'gate_question', 'narrowing', 'procedure'));
