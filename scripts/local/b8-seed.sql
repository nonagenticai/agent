-- B8 local acceptance seed. Run AFTER agent-api has booted once (its boot migrates and applies seed v8):
--   psql "$OWNER_URL" -v ON_ERROR_STOP=1 -v platform=<uuid> -v demo=<uuid> -v other=<uuid> -v provider=<uuid> \
--        -v llm_url=http://127.0.0.1:<port> -f scripts/local/b8-seed.sql
-- Idempotent (ON CONFLICT DO NOTHING). Runs as the database owner, so RLS does not apply (like the Rust tests'
-- `insert_tenant` / `insert_user` / `licence_active` fixtures in crates/agent-api/tests/common/mod.rs).

INSERT INTO tenants (id, name, residency, updated_by) VALUES
  (:'platform', 'Nonagentic Platform', 'eu', 'b8-acceptance'),
  (:'demo',     'Demo Retail',         'eu', 'b8-acceptance'),
  (:'other',    'Other Co',            'eu', 'b8-acceptance')
ON CONFLICT DO NOTHING;

-- B7 gate 2 fails closed: tenants that run turns hold a fresh active licence verdict.
INSERT INTO tenant_licence_verdict (tenant_id, status, reason, licence_id, checked_at)
SELECT t, 'active', NULL, 'lic-b8-' || t, now() FROM (VALUES (:'platform'::uuid), (:'demo'::uuid), (:'other'::uuid)) v(t)
ON CONFLICT (tenant_id) DO UPDATE SET status = 'active', reason = NULL, checked_at = now();

-- Tenant roles live in `users` (D88), not in the token. The fake IdP's persona `x` signs in as sub `b8-x`.
INSERT INTO users (sub, tenant_id, roles, display_name, updated_by) VALUES
  ('b8-ta',  :'demo',  ARRAY['tenant_admin','job_designer','release_manager','knowledge_manager','integrations_manager','support_manager','support_agent','operator'], 'B8 Tenant Admin', 'b8-acceptance'),
  ('b8-tu',  :'demo',  ARRAY[]::text[],                  'B8 Tenant User',     'b8-acceptance'),
  ('b8-jd',  :'demo',  ARRAY['job_designer'],            'B8 Job Designer',    'b8-acceptance'),
  ('b8-rm',  :'demo',  ARRAY['release_manager'],         'B8 Release Manager', 'b8-acceptance'),
  ('b8-op',  :'demo',  ARRAY['operator'],                'B8 Operator',        'b8-acceptance'),
  ('b8-ta2', :'other', ARRAY['tenant_admin','job_designer','release_manager','knowledge_manager','integrations_manager','support_manager','support_agent','operator'], 'B8 Other Admin', 'b8-acceptance')
ON CONFLICT DO NOTHING;

-- GENERATOR_PROVIDER_ID: the platform tenant's own provider record (the API refuses to create one for the
-- platform-owned tenant, providers/llm.rs `refuse_internal`). No key: the fake takes none.
INSERT INTO llm_provider (tenant_id, id, name, kind, base_url, models, updated_by)
VALUES (:'platform', :'provider', 'b8-fake-generator', 'openai', :'llm_url' || '/v1', ARRAY['fake-gen'], 'b8-acceptance')
ON CONFLICT DO NOTHING;

-- A1/A3: an industry nobody started (an `industry` layer with no rows) so `not_started` is listed.
INSERT INTO layer (key, scope, industry, family, label)
VALUES ('industry/banking', 'industry', 'banking', 'industry/banking', 'Banking'),
       -- A11 starts a second platform run (a run needs its industry layer to exist)
       ('industry/b8_hint_industry', 'industry', 'b8_hint_industry', 'industry/b8_hint_industry', 'B8 hint industry')
ON CONFLICT DO NOTHING;
