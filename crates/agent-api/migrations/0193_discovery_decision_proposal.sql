-- B8 T15 (phase 2): a discovery decision keeps the proposal it decided, as the web shows it (`DiscoveryProposal`
-- without its status fields). A signal proposal is recomputed from live traffic on every read and disappears once
-- accepting it fixed the cause (the route exists, the kind is bound); the decided proposal stays visible from here.
ALTER TABLE discovery_decision
    ADD COLUMN IF NOT EXISTS proposal jsonb;
ALTER TABLE discovery_decision
    DROP CONSTRAINT IF EXISTS discovery_decision_proposal_check,
    ADD CONSTRAINT discovery_decision_proposal_check
        CHECK (proposal IS NULL OR jsonb_typeof(proposal) = 'object');
