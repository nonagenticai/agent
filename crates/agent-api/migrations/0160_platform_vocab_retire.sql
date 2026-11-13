-- 0160 — B6b T12e (F-26; plan 2026-10-20-b6-platform-model, Revision 2026-10-22 R2): retire an axis value, change
-- an axis's cardinality. References only 0042 (`axis`, `axis_value`), so it applies whichever of KH / B5 / DR merges
-- first. Forward-only (pre-prod), drift-tolerant (IF NOT EXISTS, DROP … IF EXISTS + CREATE).
--
-- Values are still never deleted (B6-8: every tag stays resolvable). A retired value is hidden from pickers and
-- refused by new profiles / rules, but kept in every read. `retired_at` goes NULL → a time once and never back.

ALTER TABLE axis_value ADD COLUMN IF NOT EXISTS retired_at timestamptz;
ALTER TABLE axis_value ADD COLUMN IF NOT EXISTS retired_by text;

ALTER TABLE axis_value DROP CONSTRAINT IF EXISTS axis_value_retired_pair;
ALTER TABLE axis_value ADD CONSTRAINT axis_value_retired_pair
    CHECK ((retired_at IS NULL) = (retired_by IS NULL) AND (retired_by IS NULL OR btrim(retired_by) <> ''));

-- The only UPDATE on axis_value: NULL → set, once; every other column is frozen.
CREATE OR REPLACE FUNCTION axis_value_retire_once() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
AS $$
BEGIN
    IF OLD.retired_at IS NOT NULL THEN
        RAISE EXCEPTION 'axis value %.% is already retired', OLD.axis, OLD.key
            USING ERRCODE = 'check_violation';
    END IF;
    IF NEW.retired_at IS NULL
       OR NEW.axis IS DISTINCT FROM OLD.axis
       OR NEW.key IS DISTINCT FROM OLD.key
       OR NEW.label IS DISTINCT FROM OLD.label
       OR NEW.parent IS DISTINCT FROM OLD.parent
       OR NEW.added_at IS DISTINCT FROM OLD.added_at
       OR NEW.added_by IS DISTINCT FROM OLD.added_by THEN
        RAISE EXCEPTION 'axis value %.%: an update may only retire it', OLD.axis, OLD.key
            USING ERRCODE = 'check_violation';
    END IF;
    RETURN NEW;
END;
$$;

DROP TRIGGER IF EXISTS axis_value_retire_once ON axis_value;
CREATE TRIGGER axis_value_retire_once BEFORE UPDATE ON axis_value
    FOR EACH ROW EXECUTE FUNCTION axis_value_retire_once();

REVOKE UPDATE ON axis_value FROM agent_app, agent_platform;
GRANT UPDATE (retired_at, retired_by) ON axis_value TO agent_platform;
GRANT UPDATE (cardinality) ON axis TO agent_platform;
