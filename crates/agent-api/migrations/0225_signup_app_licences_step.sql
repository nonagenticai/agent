-- 0225 — tenant app licences: the signup journal gains its seventh step, `app_licences`
-- (crate::signup::provisioning::STEPS, crate::tenants::app_licence). Forward-only.
--
-- 0030 bounded `signup_provisioning_steps` to the six original steps (`step IN (...)`, `ord BETWEEN 1 AND 6`), so
-- journaling the seventh failed every approval with a 500. Both checks now admit it; the run order is STEPS order.

ALTER TABLE signup_provisioning_steps
    DROP CONSTRAINT signup_provisioning_steps_step_check,
    DROP CONSTRAINT signup_provisioning_steps_ord_check,
    ADD CONSTRAINT signup_provisioning_steps_step_check
        CHECK (step IN ('tenant', 'licences_tenant', 'keycloak_user', 'membership', 'member_licence', 'invite_email',
                        'app_licences')),
    ADD CONSTRAINT signup_provisioning_steps_ord_check CHECK (ord BETWEEN 1 AND 7);
