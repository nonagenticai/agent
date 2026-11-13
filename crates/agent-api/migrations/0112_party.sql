-- 0112 — party identity (B13 §1/§2, B9 T3): who a customer is across channels. Forward-only (pre-prod),
-- drift-tolerant.
--
-- `party` is one customer of a tenant (`anonymous` until verified or linked by staff; `merged_into` = this party was
-- merged into another and answers as it). `party_identity` maps a channel subject (the wire `party.id`: a Teams AAD
-- id, a WhatsApp number, a web chat session, …) to its party: one row per (tenant, channel, subject).
-- `channel_identity_settings` is the tenant's identity policy. Tenant-wide (no job scope): one customer talks to every
-- job of the tenant. `party_id` on the case, the conversation and the handover item. Backfill: one anonymous party
-- and identity per distinct (tenant, channel, customer_id) of `conversation`, and the rows linked to it.
-- References only ≤0033 objects, 0060, 0061 and 0063.

CREATE TABLE IF NOT EXISTS party (
    id                    uuid NOT NULL DEFAULT uuidv7(),
    tenant_id             uuid NOT NULL DEFAULT app_current_tenant(),
    kind                  text NOT NULL DEFAULT 'anonymous',
    display_name          text,
    external_customer_ref text,
    merged_into           uuid,
    created_at            timestamptz NOT NULL DEFAULT now(),
    updated_at            timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (id),
    -- the target of the tenant-safe composite foreign keys below
    CONSTRAINT party_tenant_id_key UNIQUE (tenant_id, id)
);
ALTER TABLE party DROP CONSTRAINT IF EXISTS party_tenant;
ALTER TABLE party ADD CONSTRAINT party_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id);
ALTER TABLE party DROP CONSTRAINT IF EXISTS party_kind_check;
ALTER TABLE party ADD CONSTRAINT party_kind_check CHECK (kind IN ('anonymous', 'known'));
ALTER TABLE party DROP CONSTRAINT IF EXISTS party_display_name_check;
ALTER TABLE party ADD CONSTRAINT party_display_name_check
    CHECK (display_name IS NULL OR char_length(display_name) <= 120);
ALTER TABLE party DROP CONSTRAINT IF EXISTS party_external_customer_ref_check;
ALTER TABLE party ADD CONSTRAINT party_external_customer_ref_check
    CHECK (external_customer_ref IS NULL OR char_length(external_customer_ref) <= 200);
ALTER TABLE party DROP CONSTRAINT IF EXISTS party_merged_into_check;
ALTER TABLE party ADD CONSTRAINT party_merged_into_check CHECK (merged_into IS NULL OR merged_into <> id);
ALTER TABLE party DROP CONSTRAINT IF EXISTS party_merged_into;
ALTER TABLE party ADD CONSTRAINT party_merged_into
    FOREIGN KEY (tenant_id, merged_into) REFERENCES party (tenant_id, id);
CREATE INDEX IF NOT EXISTS party_recent_idx ON party (tenant_id, id DESC) WHERE merged_into IS NULL;
CREATE INDEX IF NOT EXISTS party_merged_into_idx ON party (tenant_id, merged_into) WHERE merged_into IS NOT NULL;

ALTER TABLE party ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON party;
CREATE POLICY tenant_isolation ON party FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON party FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON party TO agent_app;

CREATE TABLE IF NOT EXISTS party_identity (
    tenant_id     uuid NOT NULL DEFAULT app_current_tenant(),
    channel       text NOT NULL,
    subject       text NOT NULL,
    party_id      uuid NOT NULL,
    verified      boolean NOT NULL DEFAULT false,
    first_seen_at timestamptz NOT NULL DEFAULT now(),
    last_seen_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, channel, subject)
);
ALTER TABLE party_identity DROP CONSTRAINT IF EXISTS party_identity_tenant;
ALTER TABLE party_identity ADD CONSTRAINT party_identity_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id);
ALTER TABLE party_identity DROP CONSTRAINT IF EXISTS party_identity_party;
ALTER TABLE party_identity ADD CONSTRAINT party_identity_party
    FOREIGN KEY (tenant_id, party_id) REFERENCES party (tenant_id, id);
ALTER TABLE party_identity DROP CONSTRAINT IF EXISTS party_identity_channel_check;
ALTER TABLE party_identity ADD CONSTRAINT party_identity_channel_check CHECK (channel IN (
    'staff', 'telegram', 'web_chat', 'teams', 'slack', 'whatsapp', 'email', 'voice', 'api'));
ALTER TABLE party_identity DROP CONSTRAINT IF EXISTS party_identity_subject_check;
ALTER TABLE party_identity ADD CONSTRAINT party_identity_subject_check
    CHECK (btrim(subject) <> '' AND char_length(subject) <= 512);
ALTER TABLE party_identity DROP CONSTRAINT IF EXISTS party_identity_seen_check;
ALTER TABLE party_identity ADD CONSTRAINT party_identity_seen_check CHECK (last_seen_at >= first_seen_at);
CREATE INDEX IF NOT EXISTS party_identity_party_idx ON party_identity (tenant_id, party_id);

ALTER TABLE party_identity ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON party_identity;
CREATE POLICY tenant_isolation ON party_identity FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON party_identity FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON party_identity TO agent_app;

-- No row: mode `anonymous`, merge_on_verify true.
CREATE TABLE IF NOT EXISTS channel_identity_settings (
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    mode            text NOT NULL DEFAULT 'anonymous',
    merge_on_verify boolean NOT NULL DEFAULT true,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    updated_by      text,
    PRIMARY KEY (tenant_id)
);
ALTER TABLE channel_identity_settings DROP CONSTRAINT IF EXISTS channel_identity_settings_tenant;
ALTER TABLE channel_identity_settings
    ADD CONSTRAINT channel_identity_settings_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id);
ALTER TABLE channel_identity_settings DROP CONSTRAINT IF EXISTS channel_identity_settings_mode_check;
ALTER TABLE channel_identity_settings ADD CONSTRAINT channel_identity_settings_mode_check
    CHECK (mode IN ('anonymous', 'verify_on_identity'));

ALTER TABLE channel_identity_settings ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON channel_identity_settings;
CREATE POLICY tenant_isolation ON channel_identity_settings FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE ALL ON channel_identity_settings FROM agent_app;
GRANT SELECT, INSERT, UPDATE ON channel_identity_settings TO agent_app;

-- The party of a case, a conversation, a handover item.
ALTER TABLE agent_case ADD COLUMN IF NOT EXISTS party_id uuid;
ALTER TABLE agent_case DROP CONSTRAINT IF EXISTS agent_case_party;
ALTER TABLE agent_case ADD CONSTRAINT agent_case_party
    FOREIGN KEY (tenant_id, party_id) REFERENCES party (tenant_id, id);
CREATE INDEX IF NOT EXISTS agent_case_party_idx ON agent_case (tenant_id, party_id) WHERE party_id IS NOT NULL;

ALTER TABLE conversation ADD COLUMN IF NOT EXISTS party_id uuid;
ALTER TABLE conversation DROP CONSTRAINT IF EXISTS conversation_party;
ALTER TABLE conversation ADD CONSTRAINT conversation_party
    FOREIGN KEY (tenant_id, party_id) REFERENCES party (tenant_id, id);
CREATE INDEX IF NOT EXISTS conversation_party_idx ON conversation (tenant_id, party_id) WHERE party_id IS NOT NULL;

ALTER TABLE agent_handover_items ADD COLUMN IF NOT EXISTS party_id uuid;
ALTER TABLE agent_handover_items DROP CONSTRAINT IF EXISTS agent_handover_items_party;
ALTER TABLE agent_handover_items ADD CONSTRAINT agent_handover_items_party
    FOREIGN KEY (tenant_id, party_id) REFERENCES party (tenant_id, id);
CREATE INDEX IF NOT EXISTS agent_handover_items_party_idx
    ON agent_handover_items (tenant_id, party_id) WHERE party_id IS NOT NULL;

-- Backfill (runs as the owner: RLS does not apply). Subjects that the identity CHECK refuses are left unlinked.
CREATE TEMP TABLE b13_party_backfill ON COMMIT DROP AS
SELECT uuidv7() AS party_id, s.tenant_id, s.channel, s.subject, s.first_seen, s.last_seen
  FROM (SELECT c.tenant_id, c.channel, c.customer_id AS subject,
               min(c.started_at) AS first_seen, max(c.started_at) AS last_seen
          FROM conversation c
         WHERE c.customer_id IS NOT NULL AND btrim(c.customer_id) <> '' AND char_length(c.customer_id) <= 512
         GROUP BY c.tenant_id, c.channel, c.customer_id) s
 WHERE NOT EXISTS (SELECT 1 FROM party_identity i
                    WHERE i.tenant_id = s.tenant_id AND i.channel = s.channel AND i.subject = s.subject);

INSERT INTO party (id, tenant_id, kind, created_at, updated_at)
SELECT party_id, tenant_id, 'anonymous', first_seen, last_seen FROM b13_party_backfill;

INSERT INTO party_identity (tenant_id, channel, subject, party_id, verified, first_seen_at, last_seen_at)
SELECT tenant_id, channel, subject, party_id, false, first_seen, last_seen FROM b13_party_backfill;

UPDATE conversation c SET party_id = i.party_id
  FROM party_identity i
 WHERE c.party_id IS NULL
   AND i.tenant_id = c.tenant_id AND i.channel = c.channel AND i.subject = c.customer_id;

UPDATE agent_case a SET party_id = c.party_id
  FROM conversation c
 WHERE a.party_id IS NULL AND c.party_id IS NOT NULL
   AND c.tenant_id = a.tenant_id AND c.job_id = a.job_id AND c.conversation_id = a.conversation_id;

UPDATE agent_handover_items h SET party_id = a.party_id
  FROM agent_case a
 WHERE h.party_id IS NULL AND a.party_id IS NOT NULL
   AND a.tenant_id = h.tenant_id AND a.job_id = h.job_id AND a.case_id = h.case_id;
