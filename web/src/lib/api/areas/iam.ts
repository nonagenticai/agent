// The `IamApi` contract: tenant IAM procedures (A3) and a job's declared tool inputs (A2).
import type {
	IamAttachmentWrite,
	IamProcedure,
	IamProcedureInput,
	ToolPolicy,
	ToolPolicyWrite,
} from '$lib/api/types/iam';

export type IamApi = {
	/** `GET /api/iam-procedures` — the tenant's identity procedures, each with how many jobs attach it. */
	iamProcedures(): Promise<IamProcedure[]>;
	/** `GET /api/iam-procedures/{id}` — one procedure (404 `procedure_not_found`). */
	iamProcedure(id: string): Promise<IamProcedure>;
	/** `POST /api/iam-procedures` — tenant_admin; 409 `name_taken`; 422 with the declaration codes. */
	createIamProcedure(input: IamProcedureInput): Promise<IamProcedure>;
	/** `PUT /api/iam-procedures/{id}` — tenant_admin; replaces name, description and steps. */
	updateIamProcedure(id: string, input: IamProcedureInput): Promise<IamProcedure>;
	/** `DELETE /api/iam-procedures/{id}` — tenant_admin; 409 `procedure_attached` while a job attaches it. */
	deleteIamProcedure(id: string): Promise<void>;
	/** `GET /api/jobs/{job}/tools/{op}/policy` — the job's draft declaration of one op (`policy` null when none). */
	toolPolicy(op: string): Promise<{ op: string; policy: ToolPolicy | null }>;
	/** `PUT /api/jobs/{job}/tools/{op}/policy` — integrations_manager; saved to the draft; 422 with a code. */
	saveToolPolicy(op: string, policy: ToolPolicy): Promise<ToolPolicyWrite>;
	/** `DELETE /api/jobs/{job}/tools/{op}/policy` — integrations_manager; 404 `policy_not_found`. */
	clearToolPolicy(op: string): Promise<ToolPolicyWrite>;
	/** `GET /api/jobs/{job}/iam-procedures` — ids of the procedures the job's draft attaches. */
	jobIamProcedures(): Promise<string[]>;
	/** `PUT /api/jobs/{job}/iam-procedures/{id}` — integrations_manager; attach to the draft. */
	attachIamProcedure(id: string): Promise<IamAttachmentWrite>;
	/** `DELETE /api/jobs/{job}/iam-procedures/{id}` — integrations_manager; detach from the draft. */
	detachIamProcedure(id: string): Promise<IamAttachmentWrite>;
};
