// D55 is superseded by D88 (2026-10-20): support actions belong to the §R.1 Support agent role (claim / reply /
// resolve / verdict on handovers, take over / hand back / staff reply in the console, claim and hand over a case);
// assignment and the SLA policy to the Support manager; unknown-outcome checks to the Operator. Without the role the
// controls stay visible, disabled, with this note.

/** The one sentence every support control shows to someone who cannot use it yet. */
export const SUPPORT_ROLES_NOTE = 'Needs the Support agent role';
/** Assignment and the handover/SLA policy (§R.1 Support manager). */
export const SUPPORT_MANAGER_NOTE = 'Needs the Support manager role';
/** Unknown-outcome checks, stuck entries, dead letters (§R.1 Operator). */
export const OPERATOR_NOTE = 'Needs the Operator role';
