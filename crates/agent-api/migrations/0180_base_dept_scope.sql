-- 0180 — D116 (plan 2026-10-24-kc-layer-contract §9 S1, S6): the base-department layer scope. Forward-only (pre-prod).
--
-- A `base_dept/<d>` layer (`<d>` a department archetype key, e.g. `base_dept/customer_service`) holds what every
-- department of that archetype shares, whatever the industry: `department` is set, `industry` and `job` are NULL.
-- It ranks between industry and industry_dept (agent_model::layer::Scope::BaseDept), composes under every
-- industry_dept and job head like base does (D116 S2), and folds at its department's archetype position
-- (agent_model::compile::FoldPosition::Archetype). Like base, industry and industry_dept it may carry region
-- sub-layers (D111): `base_dept/<d>/region/<r>`, a member of the `base_dept/<d>` family.
--
-- A base_dept carries no template metadata: `extends`, `areas` and `template_selection` stay industry_dept / job only
-- (`layer_extends_scope`, `layer_areas_scope`, `layer_template_selection_scope` of 0041 are left as they are). An
-- industry_dept's `extends` may name an existing `base_dept/<a>` layer as well as a base `department_archetype` row;
-- that is checked by the writers (agent-api `platform::seeder`), not here (no FK: an archetype row is not a layer).
-- `ontology_entity.scope` (0140) is unchanged: a base_dept layer's entity is shown at the department level
-- (`industry_dept`, agent-api `job_design::ontology::scope_of`). No base_dept layer is created here.

ALTER TABLE layer
    -- 0041's column CHECK on `scope` + D116 `base_dept`
    DROP CONSTRAINT IF EXISTS layer_scope_check,
    ADD CONSTRAINT layer_scope_check
        CHECK (scope IN ('base', 'profile', 'industry', 'base_dept', 'industry_dept', 'job')),
    -- 0139 `layer_scope_keys` + D116: a base_dept names its department only (and may carry a region)
    DROP CONSTRAINT IF EXISTS layer_scope_keys,
    ADD CONSTRAINT layer_scope_keys CHECK (
        CASE scope
            WHEN 'base' THEN industry IS NULL AND department IS NULL AND job IS NULL
                AND rule IS NULL AND dimension_key IS NULL
            WHEN 'profile' THEN rule IS NOT NULL AND industry IS NULL AND department IS NULL
                AND job IS NULL AND dimension_key IS NULL AND region IS NULL
            WHEN 'industry' THEN industry IS NOT NULL AND department IS NULL AND job IS NULL
                AND rule IS NULL AND dimension_key IS NULL
            WHEN 'base_dept' THEN department IS NOT NULL AND industry IS NULL AND job IS NULL
                AND rule IS NULL AND dimension_key IS NULL
            WHEN 'industry_dept' THEN industry IS NOT NULL AND department IS NOT NULL
                AND job IS NULL AND rule IS NULL AND dimension_key IS NULL
            WHEN 'job' THEN job IS NOT NULL AND industry IS NULL AND department IS NULL
                AND rule IS NULL AND region IS NULL
            ELSE false
        END
    ),
    -- key = web layerLabel (0139): `base_dept/<d>` and `base_dept/<d>/region/<r>` (industry is NULL)
    DROP CONSTRAINT IF EXISTS layer_key_label,
    ADD CONSTRAINT layer_key_label CHECK (
        key = scope || coalesce('/' || industry, '') || coalesce('/' || department, '')
            || coalesce('/region/' || region, '') || coalesce('/' || job, '')
            || coalesce('/' || rule, '') || coalesce('/' || dimension_key || '=' || dimension_value, '')
    ),
    -- D111 (0139) + D116: a region sub-layer is a member of its parent's family; a base_dept sub-layer's parent is
    -- `base_dept/<d>`
    DROP CONSTRAINT IF EXISTS layer_region_family,
    ADD CONSTRAINT layer_region_family CHECK (
        region IS NULL
        OR family = scope || coalesce('/' || industry, '') || coalesce('/' || department, '')
    );
