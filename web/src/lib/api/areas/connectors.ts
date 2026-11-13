// The `ConnectorsApi` contract.
import type { Paged } from '$lib/api/types/core';
import type {
	ConnectorInstance,
	McpImportResult,
	ConnectorTemplate,
	ConnectorTemplateDetail,
	ConnectorTemplateOverview,
	ConnectorTemplateOp,
	ConnectorTemplateSummary,
	ConnectorTemplateUsage,
	ContractResult,
	CredentialState,
	Env,
	JobOperations,
	JobTool,
	NewConnectorTemplateInput,
	NewInstanceInput,
	OpRef,
	RouteTarget,
	RouteWriteResult,
	SystemCatalogueRow,
	SpecExport,
	SpecFormat,
	SpecOf,
	SystemDetail,
	TenantConnectorTemplate,
	TenantTemplatePage,
	TenantTemplateUsage,
	TierResult,
	ValidationError,
} from '$lib/api/types/connectors';
import type {
	AddJobOwnOperationBody,
	JobOwnOperationWrite,
	OwnOperationsView,
	SystemInsight,
} from '$lib/api/types/connectors';
import { type OpDefLite } from '$lib/connectors/ops';

export type ConnectorsApi = {
	// ---- platform (super_admin) ----
	/** `GET /api/platform/connector-templates` — every platform template version with its implements[]. */
	connectorTemplates(): Promise<Paged<ConnectorTemplateSummary>>;
	/**
	 * `GET /api/platform/connector-templates/coverage` — P16: every version with its canonical-operation coverage,
	 * status-map completeness against the object model, publish blockers, tenant pins, and the jobs to measure against.
	 */
	connectorTemplateOverview(): Promise<ConnectorTemplateOverview>;
	/** `GET /api/platform/connector-templates/{key}/{version}` — per-op request/maps/fixtures, versions, pins. */
	connectorTemplate(key: string, version: string): Promise<ConnectorTemplateDetail>;
	/**
	 * `GET /api/platform/connector-templates/{key}/{version}/usage` — the previous version (for the diff), the newest
	 * active version (the re-pin target) and every tenant instance on any version of this key, with routed ops.
	 */
	connectorTemplateUsage(key: string, version: string): Promise<ConnectorTemplateUsage>;
	/** `POST /api/platform/connector-templates` — a new platform template as a draft (409 `template_exists`, `draft_exists`; 422). */
	createConnectorTemplate(input: NewConnectorTemplateInput): Promise<ConnectorTemplate>;
	/** `PUT /api/platform/connector-templates/{key}/{version}/ops/{op}` — drafts only (409 `template_immutable`). */
	saveConnectorTemplateOp(key: string, version: string, op: ConnectorTemplateOp): Promise<ConnectorTemplate>;
	/** `POST /api/platform/connector-templates/{key}/{version}/drafts` — a new draft version copied from this one. */
	draftConnectorTemplateVersion(key: string, fromVersion: string): Promise<ConnectorTemplate>;
	/** `POST /api/platform/connector-templates/{key}/{version}/publish` — draft → active, immutable from then on. */
	publishConnectorTemplate(key: string, version: string): Promise<ConnectorTemplate>;
	/** `POST /api/platform/connector-templates/{key}/{version}/retire` — active → retired: gone from tenant catalogues, pinned instances keep working. */
	retireConnectorTemplate(key: string, version: string): Promise<ConnectorTemplate>;
	/** `POST /api/platform/connector-templates/{key}/{version}/restore` — retired → active. */
	restoreConnectorTemplate(key: string, version: string): Promise<ConnectorTemplate>;
	/** `POST /api/platform/connector-templates/{key}/{version}/t2-runs` — live sandbox run (T2). */
	runTemplateSandbox(key: string, version: string): Promise<TierResult>;

	// ---- tenant systems (D69: instances + credentials are per-environment resources, live on save) ----
	/** `GET /api/connections/systems` — the active tenant's connector instances (prod, test, platform). */
	connectorInstances(): Promise<Paged<ConnectorInstance>>;
	/** `GET /api/connections/systems/{id}` — one instance with its pinned template, newer version and users. */
	connectorInstance(id: string): Promise<SystemDetail>;
	/**
	 * `GET /api/connections/systems/{id}/insight` — T11: per-op status/fault maps as the pin applies them (faults
	 * classified reply/escalate against `error_state_def`), the 30-day canonical outcome mix, rate limit, inert
	 * webhooks, and what re-pinning to the newest active version would change (diff + verdict).
	 */
	systemInsight(id: string): Promise<SystemInsight>;
	/**
	 * `GET /api/connections/catalogue` — templates the tenant may instantiate, "implements N of your M required ops"
	 * against the page's job, sent as `X-Job` (no job = 0 of 0).
	 */
	systemCatalogue(): Promise<Paged<SystemCatalogueRow>>;
	/** `POST /api/connections/systems` — add an instance (SSRF guard on base_url; stubs on `test` only). */
	createConnectorInstance(input: NewInstanceInput): Promise<ConnectorInstance>;
	/** `PATCH /api/connections/systems/{id}` — label, base_url, env IDs, max_rps. */
	updateConnectorInstance(
		id: string,
		patch: Partial<Pick<ConnectorInstance, 'label' | 'base_url' | 'env_ids' | 'max_rps'>>,
	): Promise<ConnectorInstance>;
	/** `PUT /api/connections/systems/{id}/credential` — write-only; answers `{set, updated_at, fingerprint}`. */
	setConnectorCredential(id: string, values: Record<string, string>): Promise<CredentialState>;
	/** `POST /api/connections/systems/{id}/repin` — pin another template version; re-runs the contract tests. */
	repinConnectorInstance(id: string, version: string): Promise<ConnectorInstance>;
	/** `POST /api/connections/systems/{id}/contract-tests` — T3 for one op on this instance's env. */
	runContractTest(id: string, op: string, opts?: { prod_write_ok?: boolean }): Promise<ContractResult>;
	/** `POST /api/connections/systems/{id}/samples` — fetch a sample response for one op from the system. */
	fetchSampleResponse(id: string, op: string): Promise<NonNullable<ConnectorInstance['sample']>>;
	/** `PUT /api/connections/systems/{id}/mappings/{op}/{field}` — JSONPath override, checked against the sample. D69:
	 * saved to the draft of every job (ledger `mapping_override`); the instance itself stays live. */
	saveMappingOverride(
		id: string,
		input: { op: string; field: string; path: string },
	): Promise<ConnectorInstance>;
	/** `DELETE /api/connections/systems/{id}/mappings/{op}/{field}` — back to the template default. */
	revertMappingOverride(id: string, op: string, field: string): Promise<ConnectorInstance>;

	// ---- tenant-private templates (D31). D69: create / save / status / new version are saved to the draft of every
	// job (ledger `tenant_template`); validation and contract-test results are not design. ----
	/** `GET /api/connections/templates` — the tenant's generic and custom templates. */
	tenantConnectorTemplates(): Promise<TenantTemplatePage>;
	/** `GET /api/connections/templates/usage` — systems pinning each tenant template version + the routes they serve. */
	tenantTemplateUsage(): Promise<TenantTemplateUsage>;
	/** `GET /api/operations?fields=io` — operation_def inputs/outputs a tenant template is validated against. */
	connectorOpDefs(): Promise<OpDefLite[]>;
	/** `POST /api/connections/templates` — a new draft `@1` built on generic REST. */
	createTenantConnectorTemplate(input: {
		key: string;
		label: string;
		kind: 'generic' | 'custom';
		auth_kind: TenantConnectorTemplate['auth_kind'];
		description: string;
		/** `rest` (default) or `mcp` (an external MCP server at the system base URL; auth_kind none = the platform MCP server). */
		transport?: 'rest' | 'mcp';
	}): Promise<TenantConnectorTemplate>;
	/** `POST /api/connections/templates/{key}/{version}/mcp-import` — read the system's MCP tools into the draft's ops. */
	importMcpTools(key: string, version: string, system: string): Promise<McpImportResult>;
	/** `PUT /api/connections/templates/{key}/{version}` — drafts only. */
	saveTenantConnectorTemplate(
		key: string,
		version: string,
		draft: Pick<
			TenantConnectorTemplate,
			'label' | 'description' | 'credential_schema' | 'ops' | 'task_tools'
		>,
	): Promise<TenantConnectorTemplate>;
	/** `POST /api/connections/templates/{key}/{version}/validate` — against operation_def + the closed transform list. */
	validateTenantConnectorTemplate(key: string, version: string): Promise<ValidationError[]>;
	/** `POST /api/connections/templates/{key}/{version}/contract-tests` — T1 over recorded fixtures. */
	runTenantTemplateContractTests(
		key: string,
		version: string,
	): Promise<NonNullable<TenantConnectorTemplate['contract']>>;
	/** `POST /api/connections/templates/{key}/{version}/status` — activate (validated + tested) or retire (unused). */
	setTenantTemplateStatus(
		key: string,
		version: string,
		status: 'active' | 'retired',
	): Promise<TenantConnectorTemplate>;
	/** `POST /api/connections/templates/{key}/{version}/drafts` — next draft version copied from this one. */
	draftTenantTemplateVersion(key: string, fromVersion: string): Promise<TenantConnectorTemplate>;

	// ---- job routes ----
	/** `GET /api/jobs/{job}/operations` — expected ops (R/O), routes per env, per-op stats, the job's draft_rev. */
	jobOperations(): Promise<JobOperations>;
	/** `GET /api/jobs/{job}/own-operations` — the job's own operations (added from the tenant's own templates) and the
	 *  ops those templates offer (`available`, with `added`). Job member reads. */
	jobOwnOperations(): Promise<OwnOperationsView>;
	/** `PUT /api/jobs/{job}/own-operations/{op}` — job_designer: add an op of a tenant template to the job at an
	 *  identity level; bumps the job's `draft_rev`. 404 `template_not_found` / `operation_not_in_template`, 409 `template_retired`, 422 `invalid_body`. */
	addJobOwnOperation(op: string, body: AddJobOwnOperationBody): Promise<JobOwnOperationWrite>;
	/** `DELETE /api/jobs/{job}/own-operations/{op}` — job_designer: remove it again; 404 `job_operation_not_found`. */
	removeJobOwnOperation(op: string): Promise<JobOwnOperationWrite>;
	/** `GET /api/jobs/{job}/tools` — J25: every expected operation of the job with its `operation_def` contract
	 *  (purpose, inputs, outputs, confirm-back, idempotency, status changes). Read-only: what a tool does is the
	 *  platform's; who serves it is the job's route (`setOperationRoute`). */
	jobTools(): Promise<JobTool[]>;
	/** `PUT /api/jobs/{job}/operations/{op}/routes/{env}` — D54: saved to the draft (test env runs it; Production at publish); bumps the job's `draft_rev` (D63: routes ride the release, no separate routing revision). */
	setOperationRoute(input: OpRef & { env: Env; target: RouteTarget }): Promise<RouteWriteResult>;
	/** `DELETE /api/jobs/{job}/operations/{op}/routes/{env}` — the op becomes unrouted on that env. */
	clearOperationRoute(input: OpRef & { env: Env }): Promise<RouteWriteResult>;
	/** `GET /api/jobs/{job}/operations/spec?format=csv|md&of=draft|live` — F-36 (T13a): the integration field spec of
	 *  the page's job as a file (one row per op × field; never a base URL, env id or credential). 422 `invalid_format` /
	 *  `invalid_of`; 409 `no_release` for `of=live` without an active release. */
	exportOperationSpec(format: SpecFormat, of?: SpecOf): Promise<SpecExport>;
};
