// Declared tool inputs (A2) and tenant IAM procedures (A3): the wire shapes of agent-api `runtime/declared.rs`.

/** Where a `state` input comes from when its key is not in the conversation state yet. */
export type StateFrom =
	'state' | { tool: string; output: string } | { ask_user: string; secret?: boolean; retries?: number };

/** One tool input: set by the model, or taken from the conversation state. */
export type InputDecl = { source: 'model' } | { source: 'state'; key: string; from?: StateFrom };

/** One tool output field written to a state key. */
export interface OutputDecl {
	key: string;
	kind?: 'value' | 'secret';
	ttl_s?: number | null;
	persist_for_subject?: boolean;
}

export type Confirmation = 'by_user' | 'auto';
export type OnFailure = 'handover' | 'refuse';

/** A job's declaration for one operation (design-ledger kind `tool_policy`). */
export interface ToolPolicy {
	inputs: Record<string, InputDecl>;
	outputs: Record<string, OutputDecl>;
	requires: string[];
	confirmation: Confirmation;
	on_failure: OnFailure;
}

/** One step of a tenant procedure: an operation with the same input/output declarations. */
export interface IamStep {
	op: string;
	inputs: Record<string, InputDecl>;
	outputs: Record<string, OutputDecl>;
}

/**
 * What a procedure established: `identified` = we know who the person is (level 1); `verified` = the procedure
 * proved it, e.g. a one-time code (level 2).
 */
export type IdentityStrength = 'identified' | 'verified';

/** A tenant's reusable identity procedure. `jobs` = how many jobs attach it. */
export interface IamProcedure {
	id: string;
	name: string;
	description: string;
	strength: IdentityStrength;
	steps: IamStep[];
	created_at: string;
	updated_at: string;
	updated_by: string;
	jobs?: number;
}

export interface IamProcedureInput {
	name: string;
	description: string;
	strength: IdentityStrength;
	steps: IamStep[];
}

/** Answer of a policy write/clear: the draft revision after and before. */
export interface ToolPolicyWrite {
	op: string;
	policy: ToolPolicy | null;
	draft_rev: number;
	previous_rev: number;
}

export interface IamAttachmentWrite {
	procedure: string;
	attached: boolean;
	draft_rev: number;
	previous_rev: number;
}
