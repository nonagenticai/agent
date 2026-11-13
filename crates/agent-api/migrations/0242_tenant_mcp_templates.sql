-- 0242 — a tenant template may use the `mcp` transport (caller identity Part B, B1): its operations call the tools of an
-- MCP server (the platform's, or an external one at the instance's base URL) instead of REST paths. Forward-only.
ALTER TABLE tenant_connector_template DROP CONSTRAINT tenant_connector_template_transport_check;
ALTER TABLE tenant_connector_template
    ADD CONSTRAINT tenant_connector_template_transport_check CHECK (transport IN ('rest', 'mcp'));
