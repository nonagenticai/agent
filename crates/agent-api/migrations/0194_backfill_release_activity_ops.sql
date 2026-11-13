-- 0194 (B8 R2, OWNER DECISION 2026-10-26): backfill the activity -> operation grants and the space -> activity
-- mapping into the bundles of the releases cut BEFORE seed v8.
--
-- Why: R2 made the KH fold the one source of an activity's operations and deleted
-- `seed/job_design/activity_ops.json`. A release cut before v8 stores legacy activity keys and no operations, and its
-- pins are pre-v8 tags whose fold has none either, so every activity -> operation grant (and every space -> activity
-- link) of such a release would be lost and the runtime would refuse its tools. From now on a cut is self-contained
-- (`release::compile::activity_items` writes `ops` into each bundle activity item, a context_space item carries its
-- `activity`) and `job_design::base` reads them from the bundle FIRST. This migration gives the older releases the
-- same shape from the OLD file (`agent.activity-ops/2`, last version at b1bfb1af^, embedded below).
--
-- D62 says a release is immutable. The OWNER EXPLICITLY APPROVED THIS ONE-TIME BACKFILL (2026-10-26). To do it:
--   * trigger `job_release_immutable` is disabled for the statement and re-enabled in the same transaction;
--   * `bundle_hash` is RECOMPUTED, because `release::apply` refuses a bundle that no longer hashes to it. The
--     canonical JSON of `release::hash::canonical_json` (sorted keys, compact, serde string escaping, integers only)
--     is reproduced by helper functions (`m0194_*`) that are dropped at the end; a release is only touched when
--     the helper reproduces its CURRENT stored `bundle_hash` first (so a divergence skips the row with a WARNING
--     rather than corrupting it). Keys sort by byte order (`COLLATE "C"`): identical to UTF-16 order for the BMP
--     keys a bundle holds.
--   * `job_release.hash` (the release identity over bundle_hash + design snapshot) is NOT rewritten: no integrity
--     check reads it (`release::approve` recomputes a stored release's identity from `bundle_hash` + the snapshot;
--     `release::summary` only displays it and compares two stored releases), and the design snapshot rows are
--     unchanged. Activity `ops` are outside `base_projection`, so no tenant row's `base_hash` moves.
-- Idempotent: a release any of whose activity items has `ops`, or any of whose context_space items has `activity`,
-- is a cut after R2 and is left alone (so is a release with nothing to add). Forward-only.

CREATE FUNCTION m0194_canon(v jsonb) RETURNS text LANGUAGE plpgsql IMMUTABLE AS $f$
BEGIN
    CASE jsonb_typeof(v)
        WHEN 'object' THEN
            RETURN '{' || coalesce((SELECT string_agg(to_jsonb(e.key)::text || ':' || m0194_canon(e.value), ','
                                                      ORDER BY e.key COLLATE "C")
                                      FROM jsonb_each(v) e), '') || '}';
        WHEN 'array' THEN
            RETURN '[' || coalesce((SELECT string_agg(m0194_canon(e.value), ',' ORDER BY e.ord)
                                      FROM jsonb_array_elements(v) WITH ORDINALITY AS e(value, ord)), '') || ']';
        ELSE
            RETURN v::text;
    END CASE;
END $f$;

CREATE FUNCTION m0194_hash(v jsonb) RETURNS text LANGUAGE sql IMMUTABLE AS $f$
    SELECT 'sha256:' || encode(sha256(convert_to(m0194_canon(v), 'UTF8')), 'hex')
$f$;

-- The old mapping: layers.<job layer>.<activity key> = [{op, variant, required}], spaces.<job layer>.<space key> = activity.
CREATE FUNCTION m0194_old() RETURNS jsonb LANGUAGE sql IMMUTABLE AS $f$
    SELECT $m${"layers":{"job/customer_care_agent":{"change:cancel_order":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"order.read","required":true,"variant":null},{"op":"order.cancel","required":false,"variant":null}],"change:modify_order":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"order.read","required":true,"variant":null},{"op":"order.modify","required":false,"variant":null}],"change:update_contact":[{"op":"party.read","required":false,"variant":null},{"op":"contact_point.update","required":false,"variant":null},{"op":"consent.update","required":false,"variant":null}],"delivery:report_problem":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"order.read","required":true,"variant":null},{"op":"delivery.read","required":true,"variant":null},{"op":"delivery.redirect","required":false,"variant":null},{"op":"delivery.report_problem","required":false,"variant":null}],"delivery:track_parcel":[{"op":"delivery.read","required":true,"variant":"by_tracking_ref"},{"op":"order.read","required":true,"variant":null},{"op":"knowledge.search","required":true,"variant":null}],"exit:close_account":[{"op":"case.create","required":false,"variant":"service_request"},{"op":"consent.update","required":false,"variant":"marketing:*"},{"op":"document.read","required":false,"variant":"invoice"}],"investigation:back_in_stock_alert":[{"op":"consent.update","required":false,"variant":"alerts:back_in_stock"}],"investigation:check_stock":[{"op":"offering.read","required":false,"variant":null},{"op":"knowledge.search","required":true,"variant":null}],"investigation:product_question":[{"op":"offering.read","required":false,"variant":null},{"op":"knowledge.search","required":true,"variant":null}],"orientation:ask_policy":[{"op":"knowledge.search","required":true,"variant":null}],"orientation:browse_catalogue":[{"op":"offering.list","required":false,"variant":null},{"op":"offering.read","required":false,"variant":null},{"op":"knowledge.search","required":true,"variant":null}],"orientation:store_info":[{"op":"knowledge.search","required":true,"variant":null}],"purchase:order_status":[{"op":"order.read","required":true,"variant":null},{"op":"delivery.read","required":true,"variant":null}],"purchase:payment_failed":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"order.read","required":true,"variant":null},{"op":"payment.list","required":true,"variant":null},{"op":"rule.evaluate","required":true,"variant":"PAY-04"},{"op":"payment.create","required":false,"variant":null}],"purchase:payment_problem":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"order.read","required":true,"variant":null},{"op":"payment.list","required":true,"variant":null},{"op":"rule.evaluate","required":true,"variant":"PAY-04"},{"op":"payment.create","required":false,"variant":null}],"support:claim_warranty":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"order.read","required":true,"variant":null},{"op":"rule.evaluate","required":true,"variant":"WAR-01"},{"op":"case.create","required":true,"variant":"warranty_claim"},{"op":"case.read","required":true,"variant":null},{"op":"knowledge.search","required":true,"variant":null}],"support:complain":[{"op":"case.create","required":true,"variant":"complaint"},{"op":"case.create","required":true,"variant":"handover"},{"op":"case.comment","required":false,"variant":null}],"support:issue_reporting":[{"op":"case.create","required":true,"variant":"complaint"},{"op":"case.create","required":true,"variant":"handover"},{"op":"case.comment","required":false,"variant":null}],"support:photo_return_evidence":[],"support:refund_tracking":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"payment.list","required":true,"variant":null},{"op":"case.read","required":true,"variant":null}],"support:return_initiation":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"order.read","required":true,"variant":null},{"op":"rule.evaluate","required":true,"variant":"RET-01"},{"op":"case.create","required":true,"variant":"return"},{"op":"payment.list","required":true,"variant":null},{"op":"knowledge.search","required":true,"variant":null}],"support:start_return":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"order.read","required":true,"variant":null},{"op":"rule.evaluate","required":true,"variant":"RET-01"},{"op":"case.create","required":true,"variant":"return"},{"op":"payment.list","required":true,"variant":null},{"op":"knowledge.search","required":true,"variant":null}],"support:track_refund":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"payment.list","required":true,"variant":null},{"op":"case.read","required":true,"variant":null}],"support:warranty_registration":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"order.read","required":true,"variant":null},{"op":"rule.evaluate","required":true,"variant":"WAR-01"},{"op":"case.create","required":true,"variant":"warranty_claim"},{"op":"case.read","required":true,"variant":null},{"op":"knowledge.search","required":true,"variant":null}]},"job/it_service_desk_agent":{"change:request_access":[{"op":"access_grant.list","required":true,"variant":null},{"op":"access_grant.create","required":true,"variant":null}],"support:incident":[{"op":"asset.list","required":false,"variant":null},{"op":"case.create","required":true,"variant":"incident"},{"op":"case.read","required":true,"variant":null},{"op":"knowledge.search","required":true,"variant":null}],"support:password_reset":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"credential.reset","required":true,"variant":null}],"support:report_incident":[{"op":"asset.list","required":false,"variant":null},{"op":"case.create","required":true,"variant":"incident"},{"op":"case.read","required":true,"variant":null},{"op":"knowledge.search","required":true,"variant":null}],"support:reset_password":[{"op":"party.list","required":true,"variant":null},{"op":"credential.verify","required":true,"variant":null},{"op":"credential.reset","required":true,"variant":null}]}},"spaces":{"job/customer_care_agent":{"retail:faq:general":"orientation:store_info","retail:logistics:delivery_problem":"delivery:report_problem","retail:logistics:track_parcel":"delivery:track_parcel","retail:order-support:cancel_order":"change:cancel_order","retail:order-support:order_status":"purchase:order_status","retail:order-support:returns":"support:return_initiation","retail:order-support:warranty":"support:warranty_registration","retail:payments:payment_errors":"purchase:payment_problem","retail:payments:refunds":"support:refund_tracking"},"job/it_service_desk_agent":{"it_services:access:password_reset":"support:password_reset","it_services:incidents:report_incident":"support:incident"}}}$m$::jsonb
$f$;

-- The bundle with `ops` / `activity` added where missing; NULL when there is nothing to add.
CREATE FUNCTION m0194_backfill(bundle jsonb, pins jsonb) RETURNS jsonb LANGUAGE plpgsql IMMUTABLE AS $f$
DECLARE
    layer text;
    ops_of jsonb;
    spaces_of jsonb;
    acts jsonb;
    cs jsonb;
    out jsonb := bundle;
BEGIN
    SELECT p->>'layer' INTO layer FROM jsonb_array_elements(pins) p WHERE p->>'layer' LIKE 'job/%' LIMIT 1;
    IF layer IS NULL THEN RETURN NULL; END IF;
    -- a cut after R2 is self-contained (its items carry `ops` / `activity`): never touched
    IF jsonb_path_exists(bundle, '$.objects.activity[*] ? (exists(@.ops))')
       OR jsonb_path_exists(bundle, '$.objects.context_space[*] ? (exists(@.activity))') THEN
        RETURN NULL;
    END IF;
    ops_of := m0194_old() #> ARRAY['layers', layer];
    spaces_of := m0194_old() #> ARRAY['spaces', layer];

    IF jsonb_typeof(bundle #> '{objects,activity}') = 'array' AND ops_of IS NOT NULL THEN
        SELECT coalesce(jsonb_agg(CASE
                   WHEN i.item ? 'ops' OR NOT coalesce(ops_of ? (i.item->>'key'), false) THEN i.item
                   ELSE i.item || jsonb_build_object('ops', ops_of -> (i.item->>'key')) END ORDER BY i.ord), '[]'::jsonb)
          INTO acts
          FROM jsonb_array_elements(bundle #> '{objects,activity}') WITH ORDINALITY AS i(item, ord);
        out := jsonb_set(out, '{objects,activity}', acts);
    END IF;
    IF jsonb_typeof(bundle #> '{objects,context_space}') = 'array' AND spaces_of IS NOT NULL THEN
        SELECT coalesce(jsonb_agg(CASE
                   WHEN coalesce(i.item->>'activity', '') <> '' OR NOT coalesce(spaces_of ? (i.item->>'key'), false) THEN i.item
                   ELSE i.item || jsonb_build_object('activity', spaces_of ->> (i.item->>'key')) END ORDER BY i.ord),
                   '[]'::jsonb)
          INTO cs
          FROM jsonb_array_elements(bundle #> '{objects,context_space}') WITH ORDINALITY AS i(item, ord);
        out := jsonb_set(out, '{objects,context_space}', cs);
    END IF;
    IF out = bundle THEN RETURN NULL; END IF;
    RETURN out;
END $f$;

ALTER TABLE job_release DISABLE TRIGGER job_release_immutable;

DO $do$
DECLARE
    r record;
    nb jsonb;
BEGIN
    FOR r IN SELECT tenant_id, job_id, release_id, bundle, bundle_hash, pins FROM job_release LOOP
        nb := m0194_backfill(r.bundle, r.pins);
        CONTINUE WHEN nb IS NULL;
        IF m0194_hash(r.bundle) IS DISTINCT FROM r.bundle_hash THEN
            RAISE WARNING '0194: release % of job % not backfilled: the stored bundle_hash is not reproducible in SQL',
                r.release_id, r.job_id;
            CONTINUE;
        END IF;
        UPDATE job_release
           SET bundle = nb, bundle_hash = m0194_hash(nb)
         WHERE tenant_id = r.tenant_id AND job_id = r.job_id AND release_id = r.release_id;
    END LOOP;
END $do$;

ALTER TABLE job_release ENABLE TRIGGER job_release_immutable;

DROP FUNCTION m0194_backfill(jsonb, jsonb);
DROP FUNCTION m0194_old();
DROP FUNCTION m0194_hash(jsonb);
DROP FUNCTION m0194_canon(jsonb);
