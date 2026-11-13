-- 0030 — early-access signup (owner 2026-10-19: pulled into now, D73; overrides D44–D46 "signup in M2").
--
-- `signup_approvals`: strata_configurator 0005_signup_approvals shape + commonpractice 0121 (`locale`) +
-- the requester's `source`/`trust`, + `approved_tenant_id` / `decided_via` / `reason`.
-- `signup_provisioning_steps`: the journal of what an approval did (commonpractice `tenants.provisioning`,
-- as rows): one row per (signup, step), each ok | pending | failed | skipped with its attempts, last error
-- and the per-step lease the approve request and the retry tick claim. The set-password email is the last
-- step and runs only when every earlier step is ok or skipped.
--
-- Platform data: no tenant_id column and no RLS, like the authoring tables (P1a, D44). Every route that
-- reaches these tables is guarded in code (intake service account, decider service account, super_admin).
-- Forward-only (pre-prod).

CREATE TABLE signup_approvals (
    id                 uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    email              text        NOT NULL CHECK (email <> '' AND char_length(email) <= 254),
    -- Requester-typed, normalised to one printable line by the route, at most 160 characters.
    name               text        NOT NULL CHECK (char_length(name) BETWEEN 1 AND 160),
    company            text        CHECK (company IS NULL OR char_length(company) BETWEEN 1 AND 160),
    -- Free text with line breaks kept; never logged. Telegram shows a <=300-char one-line excerpt.
    message            text        CHECK (message IS NULL OR char_length(message) <= 2000),
    source             text        NOT NULL CHECK (source IN ('web', 'chat')),
    -- The requester's language (commonpractice 0121, contract C6); NULL = not told (the email uses en).
    locale             text        CHECK (locale IS NULL OR locale IN ('ar', 'de', 'en', 'es', 'fr', 'he', 'nl', 'pl')),
    -- The trusted path that admitted the row. Only the intake writes here.
    trust              text        NOT NULL DEFAULT 'intake' CHECK (trust IN ('intake')),
    -- The intake client (`azp`) that submitted it.
    intake_client      text        NOT NULL CHECK (intake_client <> ''),
    status             text        NOT NULL DEFAULT 'pending'
                                   CHECK (status IN ('pending', 'approved', 'rejected')),
    -- The tenant an approval created; NULL until then (and for rejected rows).
    approved_tenant_id uuid        REFERENCES tenants (id) ON DELETE SET NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),
    decided_at         timestamptz,
    -- The super_admin's token `sub`, or `telegram:<user id>` for a Telegram tap relayed by chat-bridge.
    decided_by         text,
    decided_via        text        CHECK (decided_via IS NULL OR decided_via IN ('ui', 'telegram')),
    -- Optional reason a super_admin gave for a rejection (never sent anywhere).
    reason             text        CHECK (reason IS NULL OR char_length(reason) <= 500),
    CHECK ((status = 'pending') = (decided_at IS NULL)),
    CHECK ((status = 'pending') = (decided_via IS NULL)),
    CHECK (status = 'approved' OR approved_tenant_id IS NULL)
);

-- One PENDING row per address (case-insensitive), so a re-submit is a no-op while it waits; a decided row
-- does not block a later signup.
CREATE UNIQUE INDEX signup_approvals_pending_email
    ON signup_approvals (lower(email)) WHERE status = 'pending';
-- The queue listing: newest first, optionally by status.
CREATE INDEX signup_approvals_status_created_idx
    ON signup_approvals (status, created_at DESC, id DESC);
CREATE INDEX signup_approvals_created_idx
    ON signup_approvals (created_at DESC, id DESC);

CREATE TABLE signup_provisioning_steps (
    signup_id       uuid        NOT NULL REFERENCES signup_approvals (id) ON DELETE CASCADE,
    step            text        NOT NULL CHECK (step IN ('tenant', 'licences_tenant', 'keycloak_user',
                                                         'membership', 'member_licence', 'invite_email')),
    -- Run order (the steps depend on the ones before them).
    ord             smallint    NOT NULL CHECK (ord BETWEEN 1 AND 6),
    status          text        NOT NULL DEFAULT 'pending'
                                CHECK (status IN ('pending', 'ok', 'failed', 'skipped')),
    attempts        integer     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    -- Machine code of the last failure or skip reason; NULL when ok.
    code            text,
    -- Short human detail of the last failure (no personal data, no secrets).
    message         text        CHECK (message IS NULL OR char_length(message) <= 500),
    -- What a later attempt needs and an earlier one produced: the Keycloak sub, whether the identity
    -- already existed, the licence id. Never a token, never an email address.
    data            jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(data) = 'object'),
    last_attempt_at timestamptz,
    -- When the tick may run it next; NULL = it will not (ok/skipped, or failed for good).
    next_attempt_at timestamptz,
    -- The per-step claim; a step is run only by whoever set this to the future.
    lease_until     timestamptz,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (signup_id, step),
    UNIQUE (signup_id, ord)
);
-- The tick: due pending steps.
CREATE INDEX signup_provisioning_steps_due
    ON signup_provisioning_steps (next_attempt_at) WHERE status = 'pending';

-- The member licence's token, sealed (AES-256-GCM, `v1:` prefix, tenant id as associated data) under
-- LICENSE_TOKEN_ENC_KEY (strata_configurator 0004 + 0006). Tenant data: under RLS.
CREATE TABLE tenant_license (
    tenant_id       uuid        PRIMARY KEY REFERENCES tenants (id) ON DELETE CASCADE,
    license_id      text        NOT NULL CHECK (license_id <> ''),
    subject_id      text        NOT NULL CHECK (subject_id <> ''),
    holder_sub      text        NOT NULL CHECK (holder_sub <> ''),
    token_encrypted text        NOT NULL CHECK (token_encrypted LIKE 'v1:%'),
    issued_at       timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE tenant_license ALTER COLUMN tenant_id SET DEFAULT app_current_tenant();
ALTER TABLE tenant_license ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_license FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());

GRANT SELECT, INSERT, UPDATE ON signup_approvals, signup_provisioning_steps TO agent_app;
GRANT SELECT, INSERT, UPDATE ON tenant_license TO agent_app;
