-- 0038 — per-item backoff for the licence reconciler (B1 review finding #2; D77 L7). Forward-only.
--
-- Before this the reconciler took the first 10 items per kind every tick: an item that fails forever (a licence id
-- licences keeps refusing to revoke, a tenant it refuses to mint for) was re-picked every pass, could starve good
-- items behind it, and audited `license.revoke_failed` every tick. Now a failed item gets a row here: `attempts`,
-- exponential `next_attempt_at` (30 s doubling, capped at 1 h; computed in crate::signup::provisioning), and
-- `status = 'error'` once it has failed RECONCILE_MAX_ATTEMPTS times (still retried at the cap, never audited again
-- unless it recovers). Success deletes the row; a row whose work is gone (the entitlement left pending/revoking, the
-- id was unparked, the tenant got its licence or is no longer active) is pruned by agent_licence_backoff_prune().
--
-- `item` is the member's sub (`mint`, `revoke`), the licence id (`parked`) or '' (`tenant`). Platform data: read and
-- written as agent_platform (the reconciler's begin_platform transaction); agent_app has no grant.
CREATE TABLE licence_work_backoff (
    kind            text NOT NULL CHECK (kind IN ('mint', 'revoke', 'parked', 'tenant')),
    tenant_id       uuid NOT NULL REFERENCES tenants (id),
    item            text NOT NULL DEFAULT '',
    attempts        integer NOT NULL CHECK (attempts > 0),
    status          text NOT NULL DEFAULT 'retrying' CHECK (status IN ('retrying', 'error')),
    last_error      text NOT NULL CHECK (char_length(last_error) BETWEEN 1 AND 80),
    next_attempt_at timestamptz NOT NULL,
    first_failed_at timestamptz NOT NULL DEFAULT now(),
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, tenant_id, item)
);
CREATE INDEX licence_work_backoff_due ON licence_work_backoff (next_attempt_at);
GRANT SELECT, INSERT, UPDATE, DELETE ON licence_work_backoff TO agent_platform;

-- The work list (0036) again, now skipping items whose backoff is not due and taking the never-failed and the
-- longest-overdue first, so a stuck item can no longer hold a batch slot every tick.
CREATE OR REPLACE FUNCTION agent_licence_work(p_limit bigint)
    RETURNS TABLE (kind text, tenant_id uuid, item text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        (SELECT 'mint'::text, e.tenant_id, e.user_sub
           FROM member_entitlement e JOIN tenants t ON t.id = e.tenant_id
           LEFT JOIN licence_work_backoff b
                  ON b.kind = 'mint' AND b.tenant_id = e.tenant_id AND b.item = e.user_sub
          WHERE e.status = 'pending' AND t.status = 'active'
            AND (b.next_attempt_at IS NULL OR b.next_attempt_at <= now())
          ORDER BY b.next_attempt_at NULLS FIRST, e.updated_at LIMIT p_limit)
        UNION ALL
        (SELECT 'revoke'::text, e.tenant_id, e.user_sub
           FROM member_entitlement e
           LEFT JOIN licence_work_backoff b
                  ON b.kind = 'revoke' AND b.tenant_id = e.tenant_id AND b.item = e.user_sub
          WHERE e.status = 'revoking' AND e.license_id IS NOT NULL
            AND (b.next_attempt_at IS NULL OR b.next_attempt_at <= now())
          ORDER BY b.next_attempt_at NULLS FIRST, e.updated_at LIMIT p_limit)
        UNION ALL
        (SELECT 'parked'::text, t.id, p.id
           FROM tenants t CROSS JOIN LATERAL unnest(t.licence_revoke_pending) AS p (id)
           LEFT JOIN licence_work_backoff b
                  ON b.kind = 'parked' AND b.tenant_id = t.id AND b.item = p.id
          WHERE b.next_attempt_at IS NULL OR b.next_attempt_at <= now()
          ORDER BY b.next_attempt_at NULLS FIRST, t.id, p.id LIMIT p_limit)
        UNION ALL
        (SELECT 'tenant'::text, t.id, NULL::text
           FROM tenants t
           LEFT JOIN licence_work_backoff b
                  ON b.kind = 'tenant' AND b.tenant_id = t.id AND b.item = ''
          WHERE t.status = 'active' AND t.licence_id IS NULL
            AND (b.next_attempt_at IS NULL OR b.next_attempt_at <= now())
          ORDER BY b.next_attempt_at NULLS FIRST, t.created_at LIMIT p_limit)
    $$;

-- Drop the backoff rows whose work no longer exists (the item was done by another path, or left the work list).
-- Returns how many. EXECUTE for agent_platform only.
CREATE FUNCTION agent_licence_backoff_prune() RETURNS bigint
    LANGUAGE sql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        WITH gone AS (
            DELETE FROM licence_work_backoff b
             WHERE NOT CASE b.kind
                 WHEN 'mint' THEN EXISTS (
                     SELECT 1 FROM member_entitlement e JOIN tenants t ON t.id = e.tenant_id
                      WHERE e.tenant_id = b.tenant_id AND e.user_sub = b.item
                        AND e.status = 'pending' AND t.status = 'active')
                 WHEN 'revoke' THEN EXISTS (
                     SELECT 1 FROM member_entitlement e
                      WHERE e.tenant_id = b.tenant_id AND e.user_sub = b.item
                        AND e.status = 'revoking' AND e.license_id IS NOT NULL)
                 WHEN 'parked' THEN EXISTS (
                     SELECT 1 FROM tenants t WHERE t.id = b.tenant_id AND b.item = ANY (t.licence_revoke_pending))
                 WHEN 'tenant' THEN EXISTS (
                     SELECT 1 FROM tenants t WHERE t.id = b.tenant_id AND t.status = 'active' AND t.licence_id IS NULL)
                 ELSE false
             END
            RETURNING 1
        )
        SELECT count(*) FROM gone
    $$;
REVOKE ALL ON FUNCTION agent_licence_backoff_prune() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_licence_backoff_prune() TO agent_platform;
