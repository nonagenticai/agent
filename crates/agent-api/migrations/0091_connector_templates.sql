-- 0091 — platform connector templates (B7 T4; plan B7 "Data model" 0091, from P6 T1): `connector_template` and its
-- operations `connector_template_op`. Forward-only (pre-prod).
--
-- Platform data: no tenant column, no RLS. `agent_app` reads (tenants pick and pin templates); `agent_platform`
-- (crate::platform_db) inserts, updates only the result/lifecycle columns of a template, and edits the operations of
-- a DRAFT. A version that left draft is immutable (trigger `connector_template_guard`): only `status` active →
-- retired and the result columns `t1`, `t2`, `drift` may change, for the owner too. `template_immutable` is a client
-- error (crate::error::KNOWN_DB_CODES → 409). The boot seeder (crate::connectors::seed) inserts
-- `seed/connectors/templates.json`, insert-only.

CREATE TABLE connector_template (
    key                text        NOT NULL CHECK (key ~ '^[a-z][a-z0-9_]*$'),
    version            text        NOT NULL CHECK (btrim(version) <> '' AND char_length(version) <= 64),
    label              text        NOT NULL CHECK (btrim(label) <> ''),
    vendor             text        NOT NULL,
    system_class       text        NOT NULL,
    description        text        NOT NULL DEFAULT '',
    wave               text        NOT NULL,
    -- strata_rag, agent_cases, builtin: provisioned down, never picked by a tenant.
    platform_connector boolean     NOT NULL DEFAULT false,
    transport          text        NOT NULL CHECK (transport IN
                           ('rest', 'graphql', 'platform', 'stub', 'soap', 'graph', 'ldap', 'mcp', 'table')),
    alt_transport      text        CHECK (alt_transport IS NULL OR alt_transport IN
                           ('rest', 'graphql', 'platform', 'stub', 'soap', 'graph', 'ldap', 'mcp', 'table')),
    auth_kind          text        NOT NULL CHECK (auth_kind IN
                           ('none', 'api_key_header', 'basic', 'bearer_static', 'oauth2_client_credentials',
                            'bundle_token', 'mtls')),
    auth_detail        text        NOT NULL DEFAULT '',
    auth_params        jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(auth_params) = 'object'),
    credential_schema  jsonb       NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(credential_schema) = 'array'),
    rate_limit_model   jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(rate_limit_model) = 'object'),
    vendor_api_pin     text        NOT NULL DEFAULT '',
    webhooks           jsonb       NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(webhooks) = 'array'),
    status             text        NOT NULL DEFAULT 'draft' CHECK (status IN ('draft', 'active', 'retired')),
    -- Results (T1 fixture tier, T2 sandbox tier, vendor drift): written after publish too.
    t1                 jsonb       NOT NULL DEFAULT '{"status": "not_run", "at": null}'
                                   CHECK (jsonb_typeof(t1) = 'object'),
    t2                 jsonb       NOT NULL DEFAULT '{"status": "not_run", "at": null}'
                                   CHECK (jsonb_typeof(t2) = 'object'),
    drift              jsonb       CHECK (drift IS NULL OR jsonb_typeof(drift) = 'object'),
    published_by       text,
    published_at       timestamptz,
    created_at         timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (key, version)
);
-- One draft per template key (the next version being designed).
CREATE UNIQUE INDEX connector_template_one_draft ON connector_template (key) WHERE status = 'draft';

-- One operation of a template version: `object.verb{variant}` (`variant = '*'` is the unqualified operation).
CREATE TABLE connector_template_op (
    key         text    NOT NULL,
    version     text    NOT NULL,
    object      text    NOT NULL CHECK (btrim(object) <> ''),
    verb        text    NOT NULL CHECK (btrim(verb) <> ''),
    variant     text    NOT NULL CHECK (btrim(variant) <> ''),
    side_effect text    NOT NULL CHECK (side_effect IN ('R', 'W', 'X', 'P')),
    op_kind     text    NOT NULL DEFAULT 'standard',
    method      text    NOT NULL,
    path        text    NOT NULL,
    -- Request template (GraphQL document or JSON body) with `{{canonical.*}}` / `{{secret.*}}` placeholders.
    request     text    NOT NULL DEFAULT '',
    field_map   jsonb   NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(field_map) = 'array'),
    status_map  jsonb   NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(status_map) = 'array'),
    fault_map   jsonb   NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(fault_map) = 'array'),
    pagination  text,
    idempotency jsonb   NOT NULL DEFAULT '{"kind": "none"}' CHECK (jsonb_typeof(idempotency) = 'object'),
    -- The read op that confirms an X/P write landed.
    verify_read text,
    timeout_ms  integer NOT NULL CHECK (timeout_ms BETWEEN 100 AND 30000),
    fixtures    jsonb   NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(fixtures) = 'array'),
    PRIMARY KEY (key, version, object, verb, variant),
    CONSTRAINT connector_template_op_template_fk FOREIGN KEY (key, version) REFERENCES connector_template (key, version)
);

-- A non-draft template changes only `status` active → retired and `t1`, `t2`, `drift`; it is never deleted.
-- Compared as the row's JSON minus those columns, so a column added later is covered.
CREATE FUNCTION connector_template_guard() RETURNS trigger
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
       AND (to_jsonb(NEW) - 'status' - 't1' - 't2' - 'drift')::text
         = (to_jsonb(OLD) - 'status' - 't1' - 't2' - 'drift')::text THEN
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'template_immutable'
        USING DETAIL = format('connector_template %s@%s is %s: only status active → retired, t1, t2 and drift may change',
                              OLD.key, OLD.version, OLD.status);
END $$;
CREATE TRIGGER connector_template_guard BEFORE UPDATE OR DELETE ON connector_template
    FOR EACH ROW EXECUTE FUNCTION connector_template_guard();

-- The operations of a non-draft template never change. An INSERT is allowed while the parent is a draft, or in the
-- transaction that inserted the parent (the boot seeder writes an active version and its operations together;
-- `xmin` is the inserting transaction). A parent row UPDATEd earlier in the same transaction also carries this
-- transaction's `xmin`; only result columns of a non-draft can change, and callers never add operations then.
CREATE FUNCTION connector_template_op_guard() RETURNS trigger
    LANGUAGE plpgsql
    SET search_path = pg_catalog, public, pg_temp
    AS $$
DECLARE
    p_status text;
    p_xmin   xid;
BEGIN
    IF TG_OP IN ('UPDATE', 'DELETE') THEN
        SELECT t.status INTO p_status FROM connector_template t WHERE t.key = OLD.key AND t.version = OLD.version;
        IF p_status IS DISTINCT FROM 'draft' THEN
            RAISE EXCEPTION 'template_immutable'
                USING DETAIL = format('connector_template %s@%s is %s: its operations do not change',
                                      OLD.key, OLD.version, p_status);
        END IF;
        IF TG_OP = 'DELETE' THEN
            RETURN OLD;
        END IF;
    END IF;
    SELECT t.status, t.xmin INTO p_status, p_xmin
      FROM connector_template t WHERE t.key = NEW.key AND t.version = NEW.version;
    IF p_status IS NULL OR p_status = 'draft' OR p_xmin = pg_current_xact_id()::xid THEN
        -- a missing parent is the foreign key's error, not ours
        RETURN NEW;
    END IF;
    RAISE EXCEPTION 'template_immutable'
        USING DETAIL = format('connector_template %s@%s is %s: its operations do not change',
                              NEW.key, NEW.version, p_status);
END $$;
CREATE TRIGGER connector_template_op_guard BEFORE INSERT OR UPDATE OR DELETE ON connector_template_op
    FOR EACH ROW EXECUTE FUNCTION connector_template_op_guard();

REVOKE ALL ON connector_template, connector_template_op FROM agent_app;
GRANT SELECT ON connector_template, connector_template_op TO agent_app, agent_platform;
GRANT INSERT ON connector_template TO agent_platform;
GRANT UPDATE (status, t1, t2, drift, published_by, published_at) ON connector_template TO agent_platform;
-- Draft operations are edited in place (the guard refuses it once the version left draft).
GRANT INSERT, UPDATE, DELETE ON connector_template_op TO agent_platform;
