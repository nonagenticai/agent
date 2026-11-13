-- 0139 — KH T9 (D111, D112, D115 (7)): region is an add-on sub-layer of each platform layer. Forward-only (pre-prod).
--
-- A base, industry or industry_dept layer may carry region sub-layers (`<r>` a jurisdiction code):
--   base/region/<r>, industry/<i>/region/<r>, industry_dept/<i>/<d>/region/<r>
-- A sub-layer keeps its parent's scope (so it holds the same row kinds — rows, `instruction`, `stage_knowledge_kind`,
-- `knowledge_pack`, rule floors), carries the new key part `region`, and is a member of its PARENT's tag family
-- (`family` = the parent's key): one tag of the parent pins parent and sub-layer together, one upgrade moves both.
-- It folds right after its parent when the job's `selection.region` names `<r>` (agent_model::compile::fold_order);
-- without a region no sub-layer applies. No region content is seeded. The region vocabulary a selection may name is
-- the platform's `region` dimension values (0042 `dimension_value`, curated in the platform seed); a selection
-- naming another region is 422 `unknown_region` (agent-api `platform::templates::region_refusal`). A layer's own
-- region is not tied to that vocabulary here (a sub-layer of a retired region is unreachable data, not an error).

ALTER TABLE layer
    ADD COLUMN IF NOT EXISTS region text;

ALTER TABLE layer
    DROP CONSTRAINT IF EXISTS layer_region_check,
    ADD CONSTRAINT layer_region_check CHECK (region ~ '^[^/=[:space:]]+$'),
    -- one layer per key-part tuple, the region included
    DROP CONSTRAINT IF EXISTS layer_key_unique,
    ADD CONSTRAINT layer_key_unique UNIQUE NULLS NOT DISTINCT
        (scope, industry, department, job, rule, dimension_key, dimension_value, region),
    -- 0041 `layer_scope_keys` + D111: only base / industry / industry_dept layers may carry a region
    DROP CONSTRAINT IF EXISTS layer_scope_keys,
    ADD CONSTRAINT layer_scope_keys CHECK (
        CASE scope
            WHEN 'base' THEN industry IS NULL AND department IS NULL AND job IS NULL
                AND rule IS NULL AND dimension_key IS NULL
            WHEN 'profile' THEN rule IS NOT NULL AND industry IS NULL AND department IS NULL
                AND job IS NULL AND dimension_key IS NULL AND region IS NULL
            WHEN 'industry' THEN industry IS NOT NULL AND department IS NULL AND job IS NULL
                AND rule IS NULL AND dimension_key IS NULL
            WHEN 'industry_dept' THEN industry IS NOT NULL AND department IS NOT NULL
                AND job IS NULL AND rule IS NULL AND dimension_key IS NULL
            WHEN 'job' THEN job IS NOT NULL AND industry IS NULL AND department IS NULL
                AND rule IS NULL AND region IS NULL
            ELSE false
        END
    ),
    -- key = web layerLabel: [scope, industry, department, region && `region/<r>`, job, rule, dimension.join('=')]
    DROP CONSTRAINT IF EXISTS layer_key_label,
    ADD CONSTRAINT layer_key_label CHECK (
        key = scope || coalesce('/' || industry, '') || coalesce('/' || department, '')
            || coalesce('/region/' || region, '') || coalesce('/' || job, '')
            || coalesce('/' || rule, '') || coalesce('/' || dimension_key || '=' || dimension_value, '')
    ),
    -- B6-3 + D111: heads are their own family; profile, job×dimension and region layers belong to another one.
    DROP CONSTRAINT IF EXISTS layer_family_head,
    ADD CONSTRAINT layer_family_head
        CHECK ((scope <> 'profile' AND dimension_key IS NULL AND region IS NULL) = (family = key)),
    -- D111: a region sub-layer is a member of its parent's family (the parent's key = the key without `/region/<r>`)
    DROP CONSTRAINT IF EXISTS layer_region_family,
    ADD CONSTRAINT layer_region_family CHECK (
        region IS NULL
        OR family = scope || coalesce('/' || industry, '') || coalesce('/' || department, '')
    ),
    -- D111: the template metadata stays on the parent (archetype, areas, default selection, rollout wave)
    DROP CONSTRAINT IF EXISTS layer_region_metadata,
    ADD CONSTRAINT layer_region_metadata CHECK (
        region IS NULL
        OR (extends IS NULL AND areas = '{}' AND template_selection IS NULL
            AND rollout_wave IS NULL AND wave_label IS NULL)
    );

-- D111 (KH T9 fix round 1): a region sub-layer holds region-specific content only. `axis_rule` and `profile` rows
-- define the platform's business-model rules (read from every member of a tag, whatever its layer) and
-- `department_archetype` rows the archetypes (read from the base step only): on a sub-layer they would fire for, or
-- silently miss, every job whatever its region. Refused here and at the row write (`platform::db::rows`, 422
-- `kind_not_allowed_in_layer`); the message is an invariant violation (a caller bug), not a client error.
CREATE OR REPLACE FUNCTION layer_row_region_kind() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
    AS $$
BEGIN
    IF NEW.kind IN ('axis_rule', 'profile', 'department_archetype')
       AND EXISTS (SELECT 1 FROM layer l WHERE l.id = NEW.layer_id AND l.region IS NOT NULL) THEN
        RAISE EXCEPTION 'layer_row_region_kind'
            USING DETAIL = format('layer_row %s: a %s row cannot live in a region sub-layer', NEW.id, NEW.kind);
    END IF;
    RETURN NEW;
END $$;
DROP TRIGGER IF EXISTS layer_row_region_kind ON layer_row;
CREATE TRIGGER layer_row_region_kind BEFORE INSERT ON layer_row
    FOR EACH ROW EXECUTE FUNCTION layer_row_region_kind();
