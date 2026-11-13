-- 0066 — the working (draft) row of B2 ledger kind `tenant_calendar` (B4 T16a, F-14; plan §0 R2: `tenant_calendar`
-- only — the handover item columns `outside_hours` / `callback` / `clock_starts_at` are B12's 0063, and the job's
-- business hours are the nullable `hours` column of 0069 `handover_policy`). One row per tenant: its `timezone` and
-- `holidays` (`[{date: "YYYY-MM-DD", label ≤ 80, closed: true}]`, ≤ 366, sorted, one per date). A tenant-scope kind
-- (D69, like `llm_use`): this row is the draft of every job of the tenant; the live value per job is the job's
-- active release snapshot (D62). Written only through `design::track_tenant` (D54) or the kind's target
-- (crate::handover::calendar). No row = no tenant calendar.
--
-- Order (§0 R2): production already ran every migration on main (≤0131) and applies this AFTER them; a fresh
-- database applies it in number order. It references only objects ≤0066: `tenants` and `app_current_tenant()`
-- (0001) and the `agent_runtime` role (0065). Grants on later objects go to a 0132+ migration.
--
-- Drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + ADD, REVOKE ALL + GRANT). Forward-only.

CREATE TABLE IF NOT EXISTS tenant_calendar (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant(),
    timezone   text NOT NULL,
    holidays   jsonb NOT NULL DEFAULT '[]'::jsonb,
    updated_by text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id)
);
ALTER TABLE tenant_calendar
    DROP CONSTRAINT IF EXISTS tenant_calendar_tenant,
    ADD CONSTRAINT tenant_calendar_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    DROP CONSTRAINT IF EXISTS tenant_calendar_timezone_check,
    ADD CONSTRAINT tenant_calendar_timezone_check CHECK (char_length(btrim(timezone)) BETWEEN 1 AND 64),
    DROP CONSTRAINT IF EXISTS tenant_calendar_holidays_check,
    ADD CONSTRAINT tenant_calendar_holidays_check
        CHECK (jsonb_typeof(holidays) = 'array' AND jsonb_array_length(holidays) <= 366),
    DROP CONSTRAINT IF EXISTS tenant_calendar_updated_by_check,
    ADD CONSTRAINT tenant_calendar_updated_by_check CHECK (btrim(updated_by) <> '');
ALTER TABLE tenant_calendar ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON tenant_calendar;
CREATE POLICY tenant_isolation ON tenant_calendar FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON tenant_calendar FROM agent_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON tenant_calendar TO agent_app;
-- The runner's out-of-hours check on `case.create` reads the test scope's calendar from the draft (plan R8), as it
-- reads B2's other working rows (0065): the value columns and the key only (not who changed it).
REVOKE ALL ON tenant_calendar FROM agent_runtime;
GRANT SELECT (tenant_id, timezone, holidays) ON tenant_calendar TO agent_runtime;
