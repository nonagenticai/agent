-- 0040 — early access converged on commonpractice (CP main 4b5bb275, PRs #474–#478) and the member-invite schema
-- (owner ruling F-01). Forward-only (pre-prod).
--
-- 1. `signup_approvals` (Platform class, tests/common/classes.rs):
--    - `source = 'invite'`: the super_admin pre-approved invite (CP 0120, `POST /api/signup-approvals/invite`). An
--      invite row has no intake behind it: `trust` and `intake_client` are NULL and `invited_by` names the
--      super_admin; an intake row keeps `trust = 'intake'` and its client (CP 0106: trust NULL for invites).
--    - the approve claim (CP 0121 `approving_until` / `approving_claim`): ONE approver at a time across replicas.
--      A signup is marked approved only after its workspace, identity and seat landed (CP order); a failure leaves
--      it pending under no claim, and the next approve resumes into the same workspace.
-- 2. `member_invites` (Tenant class, RLS): a person invited into a workspace with a role set (D88). State pending →
--    accepted | expired | revoked; 7-day default expiry, resend bookkeeping. Written by `POST /api/tenant/invites`
--    and by a signup approval whose requester already had a portal account (CP: never seated at approval). Accepted
--    when the invitee signs in with a VERIFIED matching email (`agent_accept_invite`); list / resend / revoke are B1b.
-- 3. `agent_signup_homes(email, sub)`: the workspaces an address or subject already belongs to (CP
--    `assertApprovalAdmitsUser`), the one cross-tenant users read an approval needs.

ALTER TABLE signup_approvals DROP CONSTRAINT signup_approvals_source_check;
ALTER TABLE signup_approvals
    ADD CONSTRAINT signup_approvals_source_check CHECK (source IN ('web', 'chat', 'invite'));
ALTER TABLE signup_approvals DROP CONSTRAINT signup_approvals_trust_check;
ALTER TABLE signup_approvals ALTER COLUMN trust DROP NOT NULL;
ALTER TABLE signup_approvals
    ADD CONSTRAINT signup_approvals_trust_check CHECK (trust IS NULL OR trust IN ('intake'));
ALTER TABLE signup_approvals ALTER COLUMN intake_client DROP NOT NULL;
ALTER TABLE signup_approvals
    ADD COLUMN invited_by      text CHECK (invited_by IS NULL OR btrim(invited_by) <> ''),
    ADD COLUMN approving_until timestamptz,
    ADD COLUMN approving_claim uuid;
-- An invite has no intake; every other row has exactly its intake.
ALTER TABLE signup_approvals ADD CONSTRAINT signup_approvals_origin_check CHECK (
    CASE WHEN source = 'invite'
         THEN trust IS NULL AND intake_client IS NULL AND invited_by IS NOT NULL
         ELSE trust IS NOT NULL AND intake_client IS NOT NULL AND invited_by IS NULL END);
ALTER TABLE signup_approvals ADD CONSTRAINT signup_approvals_claim_check
    CHECK ((approving_until IS NULL) = (approving_claim IS NULL));

CREATE TABLE member_invites (
    id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id    uuid        NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    -- Lower-cased address the invite was sent to.
    email        text        NOT NULL CHECK (email <> '' AND char_length(email) <= 254 AND email = lower(email)),
    -- The role set the invitee gets on accept (D88; empty = observer).
    roles        text[]      NOT NULL DEFAULT '{}'
                             CHECK (roles <@ agent_role_keys() AND agent_roles_distinct(roles)),
    state        text        NOT NULL DEFAULT 'pending'
                             CHECK (state IN ('pending', 'accepted', 'expired', 'revoked')),
    -- The portal identity the invite names, when one is known at invite time.
    invitee_sub  text        CHECK (invitee_sub IS NULL OR btrim(invitee_sub) <> ''),
    -- A tenant_admin's sub, a super_admin's sub, or `signup:<id>` for a signup approval.
    invited_by   text        NOT NULL CHECK (btrim(invited_by) <> ''),
    -- The signup approval that recorded it, if any (platform row; no FK across the scope classes).
    signup_id    uuid,
    created_at   timestamptz NOT NULL DEFAULT now(),
    expires_at   timestamptz NOT NULL DEFAULT now() + interval '7 days',
    last_sent_at timestamptz,
    send_count   integer     NOT NULL DEFAULT 0 CHECK (send_count >= 0),
    accepted_at  timestamptz,
    accepted_by  text,
    revoked_at   timestamptz,
    updated_at   timestamptz NOT NULL DEFAULT now(),
    CHECK (expires_at > created_at),
    CHECK ((state = 'accepted') = (accepted_at IS NOT NULL)),
    CHECK ((state = 'revoked') = (revoked_at IS NOT NULL))
);
-- One open invite per address per workspace.
CREATE UNIQUE INDEX member_invites_pending_email ON member_invites (tenant_id, email) WHERE state = 'pending';
CREATE INDEX member_invites_tenant_created ON member_invites (tenant_id, created_at DESC);
ALTER TABLE member_invites ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON member_invites FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE ON member_invites TO agent_app;

-- Every workspace `p_email` (case-insensitively) or `p_sub` already has a member row in, disabled or invited
-- included: an approval must never move a member out of their workspace.
CREATE FUNCTION agent_signup_homes(p_email text, p_sub text)
    RETURNS TABLE (tenant_id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT DISTINCT u.tenant_id FROM users u
         WHERE lower(u.email) = lower(btrim(p_email)) OR (p_sub IS NOT NULL AND u.sub = p_sub)
    $$;
REVOKE ALL ON FUNCTION agent_signup_homes(text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_signup_homes(text, text) TO agent_app;

-- Accept a pending, unexpired invite as `p_sub` (CP `acceptInvite`): one bound to `p_sub` first (a signup's
-- pre-existing identity), else the oldest unbound one for `p_email`. The caller has checked that the access token
-- marks the email VERIFIED; expiry is evaluated here, on read. A bound invite whose email is not `p_email` is NOT
-- accepted: the row comes back with `o_mismatch` (CP `invite_email_mismatch`). Serialised per subject (concurrent sign-ins claim once); a subject
-- that already has a member row anywhere accepts nothing (an approval or invite never moves a member). In one
-- transaction: the invite accepted and bound to the subject, the member row with the invite's role set, the member's
-- `pending` entitlement (D77 L10: the licence is minted after COMMIT). Returns the accepted invite, else no row.
CREATE FUNCTION agent_accept_invite(p_sub text, p_email text, p_name text)
    RETURNS TABLE (o_invite uuid, o_tenant uuid, o_roles text[], o_signup uuid, o_mismatch boolean)
    LANGUAGE plpgsql SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
DECLARE
    inv member_invites%ROWTYPE;
BEGIN
    PERFORM pg_advisory_xact_lock(hashtextextended('agent_accept_invite:' || p_sub, 0));
    IF EXISTS (SELECT 1 FROM users u WHERE u.sub = p_sub) THEN
        RETURN;
    END IF;
    SELECT i.* INTO inv
      FROM member_invites i JOIN tenants t ON t.id = i.tenant_id AND t.status = 'active'
     WHERE i.state = 'pending' AND i.expires_at > now()
       AND (i.invitee_sub = p_sub OR (i.invitee_sub IS NULL AND i.email = lower(btrim(p_email))))
     ORDER BY (i.invitee_sub = p_sub) DESC NULLS LAST, i.created_at, i.id
     LIMIT 1
       FOR UPDATE OF i SKIP LOCKED;
    IF NOT FOUND THEN
        RETURN;
    END IF;
    o_invite := inv.id;
    o_tenant := inv.tenant_id;
    o_roles := inv.roles;
    o_signup := inv.signup_id;
    -- A bound invite still needs the verified email to be the invited one (CP `invite_email_mismatch`): nothing is
    -- accepted, the row says so.
    IF inv.email <> lower(btrim(p_email)) THEN
        o_mismatch := true;
        RETURN NEXT;
        RETURN;
    END IF;
    o_mismatch := false;
    UPDATE member_invites m
       SET state = 'accepted', accepted_at = now(), accepted_by = p_sub, invitee_sub = p_sub, updated_at = now()
     WHERE m.id = inv.id;
    INSERT INTO users (sub, tenant_id, roles, display_name, email, origin, added_by, updated_by)
         VALUES (p_sub, inv.tenant_id, inv.roles, p_name, inv.email, 'invite', inv.invited_by, 'invite:' || inv.id);
    UPDATE tenants t SET licence_revoke_pending = array_append(t.licence_revoke_pending, e.license_id)
      FROM member_entitlement e
     WHERE t.id = inv.tenant_id AND e.tenant_id = inv.tenant_id AND e.user_sub = p_sub
       AND e.status = 'revoking' AND e.license_id IS NOT NULL;
    INSERT INTO member_entitlement AS me (tenant_id, user_sub) VALUES (inv.tenant_id, p_sub)
        ON CONFLICT ON CONSTRAINT member_entitlement_tenant_sub DO UPDATE
       SET status = 'pending', license_id = NULL, license_subject_id = NULL, token_encrypted = NULL,
           error_code = NULL, revoked_at = NULL, updated_at = now()
     WHERE me.status IN ('revoked', 'error', 'revoking');
    RETURN NEXT;
END $$;
REVOKE ALL ON FUNCTION agent_accept_invite(text, text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION agent_accept_invite(text, text, text) TO agent_app;
