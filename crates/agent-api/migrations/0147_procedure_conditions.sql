-- 0147 — B5 procedure conditions (plan 2026-10-20-b5-job-design, Data model `0147`, T7a; F-15, F-20). Forward-only
-- (pre-prod), drift-tolerant (ADD COLUMN IF NOT EXISTS, DROP … IF EXISTS + ADD, as 0047).
--
-- Version-level fields of a tenant procedure version:
-- - `success_criteria`  B4 `Condition[]` (`runtime/conditions.rs`), checked by the orchestrator after the last phase;
--                       validated by the route (`runtime::conditions::validate`), the column only pins the JSON type.
-- - `tool_requirements` operations (`object.verb{variant}`, the `toolset_narrowing.op` shape of 0144) the procedure
--                       needs routed before its first step.
-- - `pddl_domain`       ≤ 64 KiB of text, stored verbatim and never interpreted.
-- Step-level `preconditions`, `postconditions`, `performed_by`, `max_per_case` live inside `steps jsonb` (validated at
-- create). Existing rows get the defaults (no criteria, no requirements, no domain).
--
-- Grants: 0145's table-level grants (agent_app SELECT/INSERT/UPDATE/DELETE, agent_runtime SELECT) cover new columns;
-- nothing to add.

ALTER TABLE procedure_version
    ADD COLUMN IF NOT EXISTS success_criteria  jsonb  NOT NULL DEFAULT '[]',
    ADD COLUMN IF NOT EXISTS tool_requirements text[] NOT NULL DEFAULT '{}',
    ADD COLUMN IF NOT EXISTS pddl_domain       text;

ALTER TABLE procedure_version
    DROP CONSTRAINT IF EXISTS procedure_version_success_criteria_check,
    ADD CONSTRAINT procedure_version_success_criteria_check CHECK (jsonb_typeof(success_criteria) = 'array'),
    DROP CONSTRAINT IF EXISTS procedure_version_tool_requirements_check,
    ADD CONSTRAINT procedure_version_tool_requirements_check
        CHECK (array_position(tool_requirements, NULL) IS NULL AND array_position(tool_requirements, '') IS NULL
               AND array_to_string(tool_requirements, chr(10)) ~ '^([a-z_]+\.[a-z_]+(\{[^}\n]+\})?(\n|$))*$'),
    DROP CONSTRAINT IF EXISTS procedure_version_pddl_domain_check,
    ADD CONSTRAINT procedure_version_pddl_domain_check
        CHECK (pddl_domain IS NULL OR octet_length(pddl_domain) <= 65536);
