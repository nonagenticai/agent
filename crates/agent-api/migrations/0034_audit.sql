-- 0034 — the tenant audit log (append-only, T) and the platform audit log (P) (B1 T7; ports
-- care-bus-agents@96c608c 0001 `audit_events` + 0006 indexes, statuses widened to the UI `AuditStatus`, the job
-- KEY instead of a job id, `platform_actor` for a super_admin acting in a tenant). Rows are written only through
-- crate::audit, which checks the action vocabulary and refuses secret- or person-shaped details.

CREATE TABLE audit_event (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    tenant_id      uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    at             timestamptz NOT NULL DEFAULT now(),
    actor_id       text NOT NULL CHECK (btrim(actor_id) <> ''),
    actor_type     text NOT NULL CHECK (actor_type IN ('human', 'ai_agent', 'system')),
    platform_actor boolean NOT NULL DEFAULT false,
    action         text NOT NULL CHECK (action ~ '^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$'),
    resource_type  text NOT NULL CHECK (btrim(resource_type) <> ''),
    resource_id    text NOT NULL,
    job            text,
    status         text NOT NULL CHECK (status IN ('success', 'failure', 'denied', 'error', 'fallback')),
    details        jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(details) = 'object'),
    request_id     text CHECK (request_id IS NULL OR char_length(request_id) <= 64)
);
CREATE INDEX audit_event_tenant_id ON audit_event (tenant_id, id DESC);
CREATE INDEX audit_event_resource ON audit_event (tenant_id, resource_id, at);
-- B4's operation calls: one `execute` row per request (crate::audit::Event::once_per_request).
CREATE UNIQUE INDEX audit_event_execute_once ON audit_event (tenant_id, request_id) WHERE action = 'execute';
ALTER TABLE audit_event ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON audit_event FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
-- Append-only for the runtime role.
REVOKE ALL ON audit_event FROM agent_app;
GRANT SELECT, INSERT ON audit_event TO agent_app;
GRANT USAGE ON SEQUENCE audit_event_id_seq TO agent_app;

-- Platform decisions (signup, tenant lifecycle, rehome): no tenant column (`subject_tenant` has no FK so an
-- archived tenant keeps its history); written only as agent_platform.
CREATE TABLE platform_audit (
    id             bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    at             timestamptz NOT NULL DEFAULT now(),
    actor_id       text NOT NULL CHECK (btrim(actor_id) <> ''),
    actor_type     text NOT NULL CHECK (actor_type IN ('user', 'system')),
    action         text NOT NULL CHECK (action ~ '^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$'),
    subject_tenant uuid,
    subject_label  text NOT NULL DEFAULT '' CHECK (char_length(subject_label) <= 200),
    status         text NOT NULL CHECK (status IN ('ok', 'failed')),
    details        jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(details) = 'object'),
    request_id     text CHECK (request_id IS NULL OR char_length(request_id) <= 64)
);
CREATE INDEX platform_audit_subject ON platform_audit (subject_tenant, id DESC);
CREATE INDEX platform_audit_action ON platform_audit (action, id DESC);
REVOKE ALL ON platform_audit FROM agent_app;
GRANT SELECT, INSERT ON platform_audit TO agent_platform;
GRANT USAGE ON SEQUENCE platform_audit_id_seq TO agent_platform;
