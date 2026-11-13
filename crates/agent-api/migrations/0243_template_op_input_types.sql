-- 0243 — a template operation may declare the type and description of its canonical inputs (`{"<input>": {"type",
-- "description"}}`), e.g. from an MCP tool's `inputSchema` (caller identity Part B, B1): the assistant's tool spec
-- carries them to the model instead of the bare input name. Forward-only.
ALTER TABLE connector_template_op
    ADD COLUMN input_types jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(input_types) = 'object');
ALTER TABLE tenant_connector_template_op
    ADD COLUMN input_types jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(input_types) = 'object');
