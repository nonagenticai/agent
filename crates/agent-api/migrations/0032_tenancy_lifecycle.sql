-- 0032 — tenant lifecycle, residency, member columns, the super_admin active tenant, the stored offboard result
-- and member sessions (B1 T5; spec 13 D55/D66, spec 16 §2; ports care-bus-agents@96c608c 0006/0007
-- `super_admin_active` + `carebus_switch_active_tenant`, renamed `agent_*` per D53). Forward-only (pre-prod).
--
-- Scope classes (tests/common/classes.rs): T = tenant table under RLS on `tenant_id = app_current_tenant()`;
-- D = SECURITY DEFINER-only (RLS on, no grant to any runtime role).

-- tenants: archived is the terminal third status; residency is required and immutable (D66).
ALTER TABLE tenants DROP CONSTRAINT tenants_status_check;
ALTER TABLE tenants
    ADD CONSTRAINT tenants_status_check CHECK (status IN ('active', 'suspended', 'archived'));
ALTER TABLE tenants
    ADD COLUMN suspended_at timestamptz,
    ADD COLUMN archived_at  timestamptz,
    ADD COLUMN residency    text,
    ADD COLUMN brand        text CHECK (brand IS NULL OR char_length(brand) <= 120),
    ADD COLUMN description  text NOT NULL DEFAULT '' CHECK (char_length(description) <= 2000),
    ADD COLUMN industry     text NOT NULL DEFAULT 'unassigned' CHECK (char_length(industry) BETWEEN 1 AND 80);
-- Rows that exist before D66 are on the EU estate (pre-prod); every new row states its residency.
UPDATE tenants SET residency = 'eu' WHERE residency IS NULL;
ALTER TABLE tenants ALTER COLUMN residency SET NOT NULL;
ALTER TABLE tenants ADD CONSTRAINT tenants_residency_check CHECK (residency IN ('eu', 'us'));
ALTER TABLE tenants
    ADD CONSTRAINT tenants_archived_at_check CHECK ((status = 'archived') = (archived_at IS NOT NULL));
CREATE UNIQUE INDEX tenants_name_lower ON tenants (lower(btrim(name)));

-- Residency never changes; an archived tenant never leaves archived. The message is the API's 409 code
-- (crate::error::KNOWN_DB_CODES).
CREATE FUNCTION tenants_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = public, pg_temp
    AS $$
BEGIN
    IF NEW.residency IS DISTINCT FROM OLD.residency THEN
        RAISE EXCEPTION 'residency_immutable';
    END IF;
    IF OLD.status = 'archived'
       AND (NEW.status IS DISTINCT FROM OLD.status OR NEW.archived_at IS DISTINCT FROM OLD.archived_at) THEN
        RAISE EXCEPTION 'tenant_archived';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER tenants_guard BEFORE UPDATE ON tenants
    FOR EACH ROW EXECUTE FUNCTION tenants_guard();

-- users: what tenant admin shows and how the member arrived.
ALTER TABLE users
    ADD COLUMN email       text CHECK (email IS NULL OR char_length(email) <= 254),
    ADD COLUMN title       text CHECK (title IS NULL OR char_length(title) <= 120),
    ADD COLUMN disabled_at timestamptz,
    ADD COLUMN invited_at  timestamptz,
    ADD COLUMN origin      text NOT NULL DEFAULT 'add_by_sub'
                           CHECK (origin IN ('tenant_create', 'invite', 'add_by_sub')),
    ADD COLUMN added_by    text;

-- A disabled member has no home tenant (the API answers 403 not_provisioned).
CREATE OR REPLACE FUNCTION agent_auth_user(p_sub text)
    RETURNS TABLE (tenant_id uuid, tenant_role text, tenant_status text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT u.tenant_id, u.tenant_role, t.status
          FROM users u JOIN tenants t ON t.id = u.tenant_id
         WHERE u.sub = p_sub AND u.disabled_at IS NULL
    $$;

-- The tenant a super_admin last acted in (D): reached only through agent_switch_active_tenant.
CREATE TABLE super_admin_active (
    sub         text PRIMARY KEY CHECK (btrim(sub) <> ''),
    tenant_id   uuid NOT NULL REFERENCES tenants (id),
    switched_at timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE super_admin_active ENABLE ROW LEVEL SECURITY;
REVOKE ALL ON super_admin_active FROM PUBLIC;

-- Record `p_tenant` as `p_sub`'s active tenant; returns the previous one (NULL the first time). The caller
-- audits `tenant.switch` when it changed.
CREATE FUNCTION agent_switch_active_tenant(p_sub text, p_tenant uuid) RETURNS uuid
    LANGUAGE plpgsql SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
DECLARE
    prev uuid;
BEGIN
    SELECT s.tenant_id INTO prev FROM super_admin_active s WHERE s.sub = p_sub FOR UPDATE;
    INSERT INTO super_admin_active AS s (sub, tenant_id, switched_at)
         VALUES (p_sub, p_tenant, now())
    ON CONFLICT (sub) DO UPDATE
        SET tenant_id = EXCLUDED.tenant_id,
            switched_at = CASE WHEN s.tenant_id = EXCLUDED.tenant_id THEN s.switched_at ELSE now() END;
    RETURN prev;
END $$;
REVOKE ALL ON FUNCTION agent_switch_active_tenant(text, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_switch_active_tenant(text, uuid) TO agent_app;

-- The stored OffboardResult (T): a repeat offboard answers it unchanged.
CREATE TABLE tenant_offboard (
    tenant_id       uuid PRIMARY KEY DEFAULT app_current_tenant() REFERENCES tenants (id),
    at              timestamptz NOT NULL DEFAULT now(),
    "by"            text NOT NULL CHECK (btrim("by") <> ''),
    licence_revoked boolean NOT NULL,
    users_disabled  integer NOT NULL CHECK (users_disabled >= 0),
    outbox          jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(outbox) = 'object'),
    data_retained   jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(data_retained) = 'object')
);
ALTER TABLE tenant_offboard ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_offboard FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT ON tenant_offboard TO agent_app;

-- One row per (member, Keycloak session): feeds memberActivity. `sid` is the token's `sid`, else an `iat`
-- bucket. Rows older than 35 days are swept hourly in-process (agent_member_session_sweep).
CREATE TABLE member_session (
    tenant_id     uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    sub           text NOT NULL CHECK (btrim(sub) <> ''),
    sid           text NOT NULL CHECK (btrim(sid) <> '' AND char_length(sid) <= 128),
    surface       text NOT NULL CHECK (surface IN ('console', 'chat', 'admin')),
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, sub, sid)
);
CREATE INDEX member_session_last_seen ON member_session (last_seen_at);
ALTER TABLE member_session ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON member_session FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON member_session TO agent_app;

-- The hourly sweep across tenants (RLS would confine a plain DELETE to one tenant). Returns rows deleted.
CREATE FUNCTION agent_member_session_sweep(p_days int) RETURNS bigint
    LANGUAGE plpgsql SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
DECLARE
    n bigint;
BEGIN
    DELETE FROM member_session WHERE last_seen_at < now() - make_interval(days => p_days);
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n;
END $$;
REVOKE ALL ON FUNCTION agent_member_session_sweep(int) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_member_session_sweep(int) TO agent_app;
