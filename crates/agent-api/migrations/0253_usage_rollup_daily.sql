-- 0253 — a physical daily usage rollup.
--
-- `usage_daily` has always been a view over the raw `usage_event` ledger (0064:127), so every 7/30/90
-- day report re-aggregates millions of rows. This table materialises exactly that view's grouping:
-- one row per tenant/job/day/source/use/funding/provider/model/actor, with the sums the reports read.
--
-- The ledger is append-only for `agent_app` (0064 REVOKEs UPDATE/DELETE), but the migration owner can
-- still correct rows (0233 already UPDATEd source/actor). INSERT/UPDATE/DELETE triggers keep the rollup
-- a faithful materialisation; a row is deleted when its event count reaches zero, so no phantom groups.
--
-- The trigger is created BEFORE the backfill and everything is one sqlx transaction: CREATE TRIGGER
-- takes ShareRowExclusiveLock on usage_event, so no insert can slip between the backfill snapshot and
-- the trigger going live. The lock is held for the backfill scan; keep it measured and short.
--
-- RLS matches usage_event (tenant_isolation + restrictive job_scope). The trigger functions are
-- SECURITY DEFINER so `agent_runtime`, which may INSERT usage_event but has no rollup grants, still
-- updates the rollup; reads go through the caller's RLS.

CREATE TABLE IF NOT EXISTS public.usage_rollup_daily (
    tenant_id   uuid NOT NULL,
    day         date NOT NULL,
    job_id      uuid,
    project     text NOT NULL,
    source      text NOT NULL,
    use         text NOT NULL,
    funding     text NOT NULL,
    provider_id text NOT NULL,
    model       text NOT NULL DEFAULT '',
    actor       text NOT NULL,
    events      bigint NOT NULL DEFAULT 0,
    calls       bigint NOT NULL DEFAULT 0,
    tokens_in   bigint NOT NULL DEFAULT 0,
    tokens_out  bigint NOT NULL DEFAULT 0,
    latency_ms  bigint NOT NULL DEFAULT 0,
    CONSTRAINT usage_rollup_daily_key UNIQUE NULLS NOT DISTINCT
        (tenant_id, day, job_id, project, source, use, funding, provider_id, model, actor),
    CONSTRAINT usage_rollup_daily_tenant FOREIGN KEY (tenant_id) REFERENCES tenants (id),
    CONSTRAINT usage_rollup_daily_job FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
CREATE INDEX IF NOT EXISTS usage_rollup_daily_job_day
    ON public.usage_rollup_daily (tenant_id, job_id, day);

ALTER TABLE public.usage_rollup_daily ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS tenant_isolation ON public.usage_rollup_daily;
CREATE POLICY tenant_isolation ON public.usage_rollup_daily FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
DROP POLICY IF EXISTS job_scope ON public.usage_rollup_daily;
CREATE POLICY job_scope ON public.usage_rollup_daily AS RESTRICTIVE FOR ALL
    USING (app_current_job() IS NULL OR job_id = app_current_job())
    WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job());
REVOKE ALL ON public.usage_rollup_daily FROM PUBLIC;
REVOKE ALL ON public.usage_rollup_daily FROM agent_app, agent_runtime;
GRANT SELECT ON public.usage_rollup_daily TO agent_app;

-- Apply one ledger row's contribution, or take it back (`p_sign` -1). A key's row disappears when the
-- last event is taken back, so the rollup never reports a group the raw ledger no longer has.
CREATE OR REPLACE FUNCTION public.usage_rollup_add(
    p_tenant uuid,
    p_day date,
    p_job uuid,
    p_project text,
    p_source text,
    p_use text,
    p_funding text,
    p_provider text,
    p_model text,
    p_actor text,
    p_events bigint,
    p_calls bigint,
    p_tokens_in bigint,
    p_tokens_out bigint,
    p_latency bigint,
    p_sign integer
) RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
BEGIN
    INSERT INTO public.usage_rollup_daily AS r
        (tenant_id, day, job_id, project, source, use, funding, provider_id, model, actor,
         events, calls, tokens_in, tokens_out, latency_ms)
    VALUES (p_tenant, p_day, p_job, p_project, p_source, p_use, p_funding, p_provider, p_model, p_actor,
            p_events * p_sign, p_calls * p_sign, p_tokens_in * p_sign, p_tokens_out * p_sign,
            p_latency * p_sign)
    ON CONFLICT (tenant_id, day, job_id, project, source, use, funding, provider_id, model, actor)
    DO UPDATE SET events = r.events + EXCLUDED.events,
                  calls = r.calls + EXCLUDED.calls,
                  tokens_in = r.tokens_in + EXCLUDED.tokens_in,
                  tokens_out = r.tokens_out + EXCLUDED.tokens_out,
                  latency_ms = r.latency_ms + EXCLUDED.latency_ms;
    IF p_sign < 0 THEN
        DELETE FROM public.usage_rollup_daily r
         WHERE r.tenant_id = p_tenant AND r.day = p_day AND r.job_id IS NOT DISTINCT FROM p_job
           AND r.project = p_project AND r.source = p_source AND r.use = p_use
           AND r.funding = p_funding AND r.provider_id = p_provider AND r.model = p_model
           AND r.actor = p_actor AND r.events <= 0;
    END IF;
END
$$;

CREATE OR REPLACE FUNCTION public.usage_rollup_ins() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
BEGIN
    PERFORM public.usage_rollup_add(
        NEW.tenant_id, (NEW.occurred_at AT TIME ZONE 'UTC')::date, NEW.job_id,
        NEW.tenant_id::text, NEW.source, NEW.use, NEW.funding, NEW.provider_id, NEW.model, NEW.actor,
        1, NEW.calls, NEW.tokens_in, NEW.tokens_out, NEW.latency_ms, 1);
    RETURN NULL;
END
$$;

CREATE OR REPLACE FUNCTION public.usage_rollup_upd() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
BEGIN
    PERFORM public.usage_rollup_add(
        OLD.tenant_id, (OLD.occurred_at AT TIME ZONE 'UTC')::date, OLD.job_id,
        OLD.tenant_id::text, OLD.source, OLD.use, OLD.funding, OLD.provider_id, OLD.model, OLD.actor,
        1, OLD.calls, OLD.tokens_in, OLD.tokens_out, OLD.latency_ms, -1);
    PERFORM public.usage_rollup_add(
        NEW.tenant_id, (NEW.occurred_at AT TIME ZONE 'UTC')::date, NEW.job_id,
        NEW.tenant_id::text, NEW.source, NEW.use, NEW.funding, NEW.provider_id, NEW.model, NEW.actor,
        1, NEW.calls, NEW.tokens_in, NEW.tokens_out, NEW.latency_ms, 1);
    RETURN NULL;
END
$$;

CREATE OR REPLACE FUNCTION public.usage_rollup_del() RETURNS trigger
LANGUAGE plpgsql SECURITY DEFINER SET search_path = public, pg_temp AS $$
BEGIN
    PERFORM public.usage_rollup_add(
        OLD.tenant_id, (OLD.occurred_at AT TIME ZONE 'UTC')::date, OLD.job_id,
        OLD.tenant_id::text, OLD.source, OLD.use, OLD.funding, OLD.provider_id, OLD.model, OLD.actor,
        1, OLD.calls, OLD.tokens_in, OLD.tokens_out, OLD.latency_ms, -1);
    RETURN NULL;
END
$$;

-- PUBLIC is the only default grant, but name the runtime roles explicitly: a direct call to
-- `usage_rollup_add` would let them desync billing totals without a ledger row. Triggers still fire.
REVOKE ALL ON FUNCTION public.usage_rollup_add(uuid, date, uuid, text, text, text, text, text, text,
    text, bigint, bigint, bigint, bigint, bigint, integer)
    FROM PUBLIC, agent_app, agent_runtime;
REVOKE ALL ON FUNCTION public.usage_rollup_ins() FROM PUBLIC, agent_app, agent_runtime;
REVOKE ALL ON FUNCTION public.usage_rollup_upd() FROM PUBLIC, agent_app, agent_runtime;
REVOKE ALL ON FUNCTION public.usage_rollup_del() FROM PUBLIC, agent_app, agent_runtime;

DROP TRIGGER IF EXISTS usage_event_rollup_ins ON public.usage_event;
CREATE TRIGGER usage_event_rollup_ins AFTER INSERT ON public.usage_event
    FOR EACH ROW EXECUTE FUNCTION public.usage_rollup_ins();
DROP TRIGGER IF EXISTS usage_event_rollup_upd ON public.usage_event;
CREATE TRIGGER usage_event_rollup_upd AFTER UPDATE ON public.usage_event
    FOR EACH ROW EXECUTE FUNCTION public.usage_rollup_upd();
DROP TRIGGER IF EXISTS usage_event_rollup_del ON public.usage_event;
CREATE TRIGGER usage_event_rollup_del AFTER DELETE ON public.usage_event
    FOR EACH ROW EXECUTE FUNCTION public.usage_rollup_del();

-- Backfill once, under the trigger's lock, as the owner. It reads the whole ledger; no row can be
-- inserted past the lock, so the trigger and this snapshot cover the ledger exactly once.
INSERT INTO public.usage_rollup_daily
    (tenant_id, day, job_id, project, source, use, funding, provider_id, model, actor,
     events, calls, tokens_in, tokens_out, latency_ms)
SELECT tenant_id,
       (occurred_at AT TIME ZONE 'UTC')::date,
       job_id,
       tenant_id::text,
       source,
       use,
       funding,
       provider_id,
       model,
       actor,
       count(*)::bigint,
       sum(calls)::bigint,
       sum(tokens_in)::bigint,
       sum(tokens_out)::bigint,
       sum(latency_ms)::bigint
  FROM public.usage_event
 GROUP BY tenant_id, (occurred_at AT TIME ZONE 'UTC')::date, job_id, tenant_id::text,
          source, use, funding, provider_id, model, actor;

-- The original view's 14 columns, same order and types. The reloption MUST be repeated: CREATE OR
-- REPLACE VIEW replaces reloptions even when WITH is omitted, and without security_invoker the view
-- runs as its owner and bypasses the rollup's RLS (a cross-tenant read in analytics).
CREATE OR REPLACE VIEW public.usage_daily WITH (security_invoker = true) AS
    SELECT tenant_id, job_id, day, source, use, funding, provider_id, model, actor,
           events, calls, tokens_in, tokens_out, latency_ms
      FROM public.usage_rollup_daily;
REVOKE ALL ON public.usage_daily FROM PUBLIC;
GRANT SELECT ON public.usage_daily TO agent_app;
