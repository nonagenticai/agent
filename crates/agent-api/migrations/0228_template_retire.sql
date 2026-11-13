-- 0228 — retire (take back) a published template, without deleting it. Forward-only.
--
-- A platform super_admin can retire a job template tag (`job_template_tag.retired_at`) and a published connector
-- template version (`connector_template.status` active → retired, already in the 0091 CHECK). A retired template
-- disappears from the choices a tenant gets for NEW jobs / connections; jobs and instances that pin it keep working.
-- Restore clears the marker.
--
-- `job_template_tag` gets a marker column rather than a third `status` value: the tag projection
-- (platform::templates::project) rewrites `status` on every re-tag and would silently un-retire a template.
ALTER TABLE job_template_tag
    ADD COLUMN retired_at timestamptz,
    ADD COLUMN retired_by text CHECK (retired_by IS NULL OR btrim(retired_by) <> ''),
    ADD CONSTRAINT job_template_tag_retired_pair CHECK ((retired_at IS NULL) = (retired_by IS NULL));
GRANT UPDATE (retired_at, retired_by) ON job_template_tag TO agent_platform;

-- 0091's guard let a published connector template go active → retired only. Restore is retired → active.
CREATE OR REPLACE FUNCTION connector_template_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
    AS $$
BEGIN
    IF OLD.status = 'draft' THEN
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
        RETURN NEW;
    END IF;
    IF TG_OP = 'UPDATE'
       AND (NEW.status = OLD.status
            OR (OLD.status = 'active' AND NEW.status = 'retired')
            OR (OLD.status = 'retired' AND NEW.status = 'active'))
       AND (to_jsonb(NEW) - 'status' - 't1' - 't2' - 'drift')::text
         = (to_jsonb(OLD) - 'status' - 't1' - 't2' - 'drift')::text THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'template_immutable'
        USING DETAIL = format('connector_template %s@%s is %s: only status active ↔ retired, t1, t2 and drift may change',
                              OLD.key, OLD.version, OLD.status);
END $$;
