-- 0092 — tenant-private connector templates (B7 T11, D31/D69; plan B7 "Data model" 0092). Forward-only (pre-prod).
--
-- A tenant admin (integrations_manager) defines a standard REST connector on the declarative base `generic_rest@2`:
-- their own operations (method, relative path, request template, field map, status map, idempotency), an auth kind
-- and a credential schema. Design rows (D69: ledger kind `tenant_template`, tenant-wide); `validation` and `contract`
-- are results, not design.
--
-- Tenant class: RLS `tenant_isolation`, grants to agent_app. A version that left draft is immutable (trigger
-- `tenant_connector_template_guard`): only `status` active → retired and the result columns `validation`,
-- `contract` (with `updated_at`) may change; its operations never change. `template_immutable` is a client error
-- (crate::error::KNOWN_DB_CODES → 409). 0093 (`connector_instance_template_exists`) checks that a tenant-scope
-- instance pins an existing template of the same tenant.

CREATE TABLE tenant_connector_template (
    tenant_id         uuid        NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    key               text        NOT NULL CHECK (key ~ '^[a-z][a-z0-9_]{2,40}$'),
    version           text        NOT NULL CHECK (btrim(version) <> '' AND char_length(version) <= 64),
    label             text        NOT NULL CHECK (btrim(label) <> ''),
    kind              text        NOT NULL CHECK (kind IN ('generic', 'custom')),
    -- v1: every tenant template is declarative REST on the generic base (OQ-3).
    base              text        NOT NULL DEFAULT 'generic_rest@2' CHECK (base = 'generic_rest@2'),
    transport         text        NOT NULL DEFAULT 'rest' CHECK (transport = 'rest'),
    auth_kind         text        NOT NULL CHECK (auth_kind IN
                          ('none', 'api_key_header', 'basic', 'bearer_static', 'oauth2_client_credentials', 'mtls')),
    -- Machine-readable auth parameters (header name, value template, token URL template, …); never a secret.
    auth_params       jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(auth_params) = 'object'),
    credential_schema jsonb       NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(credential_schema) = 'array'),
    status            text        NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'active', 'retired')),
    description       text        NOT NULL DEFAULT '',
    -- Results: `{at, errors[]}` of the last validation, `{at, status, passed, total}` of the last T1 run.
    validation        jsonb       CHECK (validation IS NULL OR jsonb_typeof(validation) = 'object'),
    contract          jsonb       CHECK (contract IS NULL OR jsonb_typeof(contract) = 'object'),
    created_by        text        NOT NULL CHECK (btrim(created_by) <> ''),
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, key, version)
);
-- One draft per (tenant, key): the next version being designed.
CREATE UNIQUE INDEX tenant_connector_template_one_draft ON tenant_connector_template (tenant_id, key)
    WHERE status = 'draft';
ALTER TABLE tenant_connector_template ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_connector_template FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE ON tenant_connector_template TO agent_app;

-- One operation of a tenant template version (declarative mapping only). `ord` keeps the author's order (the
-- validation paths `ops[i]` index it).
CREATE TABLE tenant_connector_template_op (
    tenant_id   uuid    NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    key         text    NOT NULL,
    version     text    NOT NULL,
    object      text    NOT NULL CHECK (btrim(object) <> ''),
    verb        text    NOT NULL CHECK (btrim(verb) <> ''),
    variant     text    NOT NULL CHECK (btrim(variant) <> ''),
    ord         integer NOT NULL CHECK (ord >= 0),
    method      text    NOT NULL CHECK (method IN ('GET', 'POST', 'PUT', 'PATCH', 'DELETE')),
    path        text    NOT NULL,
    request     text    NOT NULL DEFAULT '',
    field_map   jsonb   NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(field_map) = 'array'),
    status_map  jsonb   NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(status_map) = 'array'),
    idempotency jsonb   NOT NULL DEFAULT '{"kind": "none"}' CHECK (jsonb_typeof(idempotency) = 'object'),
    PRIMARY KEY (tenant_id, key, version, object, verb, variant),
    CONSTRAINT tenant_connector_template_op_position UNIQUE (tenant_id, key, version, ord),
    CONSTRAINT tenant_connector_template_op_template_fk FOREIGN KEY (tenant_id, key, version)
        REFERENCES tenant_connector_template (tenant_id, key, version)
);
ALTER TABLE tenant_connector_template_op ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_connector_template_op FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
-- Draft operations are replaced in place on save (the guard refuses it once the version left draft).
GRANT SELECT, INSERT, UPDATE, DELETE ON tenant_connector_template_op TO agent_app;

-- A non-draft tenant template changes only `status` active → retired and the results `validation`, `contract`
-- (with `updated_at`); it is never deleted. Compared as the row's JSON minus those columns, so a column added later
-- is covered.
CREATE FUNCTION tenant_connector_template_guard() RETURNS trigger
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
       AND (NEW.status = OLD.status OR (OLD.status = 'active' AND NEW.status = 'retired'))
       AND (to_jsonb(NEW) - 'status' - 'validation' - 'contract' - 'updated_at')::text
         = (to_jsonb(OLD) - 'status' - 'validation' - 'contract' - 'updated_at')::text THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'template_immutable'
        USING DETAIL = format('tenant connector template %s@%s is %s: only status active → retired, validation '
                              'and contract may change', OLD.key, OLD.version, OLD.status);
END $$;
CREATE TRIGGER tenant_connector_template_guard BEFORE UPDATE OR DELETE ON tenant_connector_template
    FOR EACH ROW EXECUTE FUNCTION tenant_connector_template_guard();

-- The operations of a non-draft tenant template never change: insert, update and delete need a draft parent.
CREATE FUNCTION tenant_connector_template_op_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
    AS $$
DECLARE
    r        record;
    p_status text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        r := NEW;
    ELSE
        r := OLD;
    END IF;
    SELECT t.status INTO p_status
      FROM tenant_connector_template t
     WHERE t.tenant_id = r.tenant_id AND t.key = r.key AND t.version = r.version;
    -- a missing parent is the foreign key's error, not ours
    IF p_status IS NOT NULL AND p_status <> 'draft' THEN
        RAISE EXCEPTION 'template_immutable'
            USING DETAIL = format('tenant connector template %s@%s is %s: its operations do not change',
                                  r.key, r.version, p_status);
    END IF;
    IF TG_OP = 'DELETE' THEN
        RETURN OLD;
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER tenant_connector_template_op_guard BEFORE INSERT OR UPDATE OR DELETE ON tenant_connector_template_op
    FOR EACH ROW EXECUTE FUNCTION tenant_connector_template_op_guard();
