-- 0253's backfill, re-run against the seeded ledger.
--
-- This makes the migration's write-lock window reproducible (psql prints the `Time:` line for the
-- INSERT) and asserts the result. The EXCEPT ALL comparison in both directions is the real check:
-- a global sum can hide one key gaining exactly what another lost. Keep the INSERT in sync with
-- `crates/agent-api/migrations/0253_usage_rollup_daily.sql`.
TRUNCATE public.usage_rollup_daily;
\timing on
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
\timing off

DO $$
DECLARE
    raw_only  bigint;
    roll_only bigint;
BEGIN
    SELECT count(*) INTO raw_only FROM (
        SELECT tenant_id, (occurred_at AT TIME ZONE 'UTC')::date AS day, job_id,
               tenant_id::text AS project, source, use, funding, provider_id, model, actor,
               count(*)::bigint AS events, sum(calls)::bigint AS calls,
               sum(tokens_in)::bigint AS tokens_in, sum(tokens_out)::bigint AS tokens_out,
               sum(latency_ms)::bigint AS latency_ms
          FROM public.usage_event
         GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9, 10
        EXCEPT ALL
        SELECT tenant_id, day, job_id, project, source, use, funding, provider_id, model, actor,
               events, calls, tokens_in, tokens_out, latency_ms
          FROM public.usage_rollup_daily
    ) d;
    SELECT count(*) INTO roll_only FROM (
        SELECT tenant_id, day, job_id, project, source, use, funding, provider_id, model, actor,
               events, calls, tokens_in, tokens_out, latency_ms
          FROM public.usage_rollup_daily
        EXCEPT ALL
        SELECT tenant_id, (occurred_at AT TIME ZONE 'UTC')::date, job_id,
               tenant_id::text, source, use, funding, provider_id, model, actor,
               count(*)::bigint, sum(calls)::bigint, sum(tokens_in)::bigint,
               sum(tokens_out)::bigint, sum(latency_ms)::bigint
          FROM public.usage_event
         GROUP BY 1, 2, 3, 4, 5, 6, 7, 8, 9, 10
    ) d;
    IF raw_only > 0 OR roll_only > 0 THEN
        RAISE EXCEPTION 'rollup differs from the ledger: % raw-only keys, % rollup-only keys',
            raw_only, roll_only;
    END IF;
    IF (SELECT count(*) FROM public.usage_event)
         <> (SELECT coalesce(sum(events), 0) FROM public.usage_rollup_daily)
       OR (SELECT coalesce(sum(calls), 0) FROM public.usage_event)
         <> (SELECT coalesce(sum(calls), 0) FROM public.usage_rollup_daily)
       OR (SELECT coalesce(sum(tokens_in), 0) FROM public.usage_event)
         <> (SELECT coalesce(sum(tokens_in), 0) FROM public.usage_rollup_daily)
       OR (SELECT coalesce(sum(tokens_out), 0) FROM public.usage_event)
         <> (SELECT coalesce(sum(tokens_out), 0) FROM public.usage_rollup_daily)
       OR (SELECT coalesce(sum(latency_ms), 0) FROM public.usage_event)
         <> (SELECT coalesce(sum(latency_ms), 0) FROM public.usage_rollup_daily) THEN
        RAISE EXCEPTION 'rollup totals differ from the ledger';
    END IF;
END
$$;

SELECT 'backfill reconciled' AS result;
