-- Tenant-authored rule definitions are overlay extension places: existing draft, release, discard and rollback
-- semantics apply to their whole definition. This adds no content and changes no existing rule or release.
ALTER TABLE job_overlay_row
    DROP CONSTRAINT IF EXISTS job_overlay_row_object_check,
    ADD CONSTRAINT job_overlay_row_object_check CHECK (
        object IN ('activity', 'context_space', 'ontology_attribute', 'gate_option', 'knowledge_store',
                   'knowledge_pack', 'instruction', 'rule_slot')
    );

-- Match the platform rule-slot key contract; old identifiers remain valid.
ALTER TABLE business_rule
    DROP CONSTRAINT IF EXISTS business_rule_id_check,
    ADD CONSTRAINT business_rule_id_check CHECK (rule_id ~ '^[A-Z][A-Z0-9-]{0,31}$');
