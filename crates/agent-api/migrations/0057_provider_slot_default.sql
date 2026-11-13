-- 0057 — B3 T4b (F-08): a provider slot reset to the deployment default; and the key of the last push (the T4a
-- ruling: an embed slot change pins each existing root store's current embed model at store level first).
--
-- `provider_slot.source`: `tenant` = the tenant's own registration (or nothing, while not configured); `default` =
-- the tenant uses the deployment default for the slot (`resetProviderSlot`). A default slot stays `configured`
-- (the default is deployed: readiness counts it) and holds no kind, endpoint, model or key: what strata serves is
-- its deployment default, so agent stores nothing it would have to keep in step.
--
-- `provider_slot_remote.pushed_key_ref`: the `provider_slot_secret` row whose key the last successful push sent for
-- the slot (`NULL` = none sent: no key, a key strata keeps for an adopted drift, or a removal). The connections_put
-- sender opens it to pin a root store to the registration strata holds before it replaces that registration.
--
-- Drift-tolerant (ADD COLUMN IF NOT EXISTS, DROP CONSTRAINT IF EXISTS + ADD). References nothing from 0091–0093
-- (tests/migration_order.rs). Forward-only.

ALTER TABLE provider_slot ADD COLUMN IF NOT EXISTS source text NOT NULL DEFAULT 'tenant';
ALTER TABLE provider_slot
    DROP CONSTRAINT IF EXISTS provider_slot_source_check,
    ADD CONSTRAINT provider_slot_source_check CHECK (source IN ('tenant', 'default')),
    DROP CONSTRAINT IF EXISTS provider_slot_value_check,
    ADD CONSTRAINT provider_slot_value_check CHECK (
        CASE WHEN source = 'default'
             THEN configured AND kind IS NULL AND base_url IS NULL AND model IS NULL AND secret_id IS NULL
                  AND NOT key_kept
             WHEN configured
             THEN kind IS NOT NULL AND char_length(kind) BETWEEN 1 AND 64
                  AND model IS NOT NULL AND char_length(model) BETWEEN 1 AND 200
             ELSE kind IS NULL AND base_url IS NULL AND model IS NULL AND secret_id IS NULL AND NOT key_kept
        END);

ALTER TABLE provider_slot_remote ADD COLUMN IF NOT EXISTS pushed_key_ref uuid;
