<script lang="ts">
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { superAdminAttrs } from '$lib/auth/roles';
	import type { ConnectorTemplate, NewConnectorTemplateInput } from '$lib/api/types/connectors';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import { templateHref } from './coverage-P16';

	let { oncreated }: { oncreated: () => void | Promise<void> } = $props();
	const blank = (): NewConnectorTemplateInput => ({
		key: '',
		version: '1',
		label: '',
		vendor: '',
		system_class: '',
		description: '',
		wave: 'v1',
		transport: 'rest',
		auth_kind: 'none',
		auth_detail: '',
		vendor_api_pin: '',
	});
	let open = $state(false);
	let busy = $state(false);
	let form = $state(blank());
	let params = $state('');
	let schema = $state('');
	let error = $state('');
	let created = $state<ConnectorTemplate | null>(null);
	function start() {
		form = blank();
		params = schema = error = '';
		created = null;
		open = true;
	}
	async function save() {
		if (busy) return;
		error = '';
		const input: NewConnectorTemplateInput = {
			...form,
			key: form.key.trim(),
			version: form.version?.trim(),
			label: form.label.trim(),
		};
		try {
			if (params.trim()) {
				const parsed: unknown = JSON.parse(params);
				if (
					!parsed ||
					Array.isArray(parsed) ||
					typeof parsed !== 'object' ||
					Object.values(parsed).some((v) => typeof v !== 'string')
				)
					throw new Error('Authentication parameters must be a JSON object with string values.');
				input.auth_params = parsed as Record<string, string>;
			}
			if (schema.trim()) {
				const parsed: unknown = JSON.parse(schema);
				if (!Array.isArray(parsed)) throw new Error('Credential fields must be a JSON array.');
				input.credential_schema = parsed;
			}
		} catch (e) {
			error = e instanceof Error ? e.message : String(e);
			return;
		}
		busy = true;
		if (
			await guard(async () => {
				created = await api().createConnectorTemplate(input);
			}, 'Connector template draft created.')
		)
			await oncreated();
		busy = false;
	}
</script>

<button class="btn primary" type="button" {...superAdminAttrs(session)} onclick={start}
	>Create connector template</button
>
<Drawer bind:open title="Create connector template" dirty={!created && !!(form.key || form.label)}>
	{#if created}
		<Callout kind="info" title="Connector template created">
			<p>{created.label} ({created.key}@{created.version}) is a draft.</p>
			<p>
				<a href={templateHref(created)}>Add operations, run contract tests and publish</a>. Tenants can select
				the published version.
			</p>
		</Callout>
	{:else}
		<form
			class="stack"
			onsubmit={(e) => {
				e.preventDefault();
				void save();
			}}
		>
			<div class="field">
				<label for="new-template-key">Template key</label><input
					id="new-template-key"
					class="input"
					bind:value={form.key}
					required
					pattern={'[a-z][a-z0-9_]{2,40}'}
					placeholder="support_system"
					disabled={busy}
				/><span class="muted">3–41 lower-case letters, digits or underscores; start with a letter.</span>
			</div>
			<div class="field">
				<label for="new-template-version">Version</label><input
					id="new-template-version"
					class="input"
					bind:value={form.version}
					required
					maxlength="64"
					pattern=".*\S.*"
					disabled={busy}
				/>
			</div>
			<div class="field">
				<label for="new-template-label">Label</label><input
					id="new-template-label"
					class="input"
					bind:value={form.label}
					required
					pattern=".*\S.*"
					disabled={busy}
				/>
			</div>
			<div class="field">
				<label for="new-template-vendor">Vendor</label><input
					id="new-template-vendor"
					class="input"
					bind:value={form.vendor}
					disabled={busy}
				/>
			</div>
			<div class="field">
				<label for="new-template-system-class">System class</label><input
					id="new-template-system-class"
					class="input"
					bind:value={form.system_class}
					placeholder="commerce, helpdesk…"
					disabled={busy}
				/>
			</div>
			<div class="field">
				<label for="new-template-wave">Catalogue group</label><select
					id="new-template-wave"
					class="input"
					bind:value={form.wave}
					disabled={busy}
					><option value="v1">Customer care</option><option value="next">Next</option><option value="itsd"
						>IT service desk</option
					></select
				>
			</div>
			<div class="field">
				<label for="new-template-transport">Transport</label><select
					id="new-template-transport"
					class="input"
					bind:value={form.transport}
					disabled={busy}
					><option value="rest">REST</option><option value="graphql">GraphQL</option><option value="soap"
						>SOAP</option
					><option value="graph">Microsoft Graph</option><option value="ldap">LDAP</option><option value="mcp"
						>MCP</option
					><option value="table">Table</option></select
				>
			</div>
			<div class="field">
				<label for="new-template-auth">Authentication</label><select
					id="new-template-auth"
					class="input"
					bind:value={form.auth_kind}
					disabled={busy}
					><option value="none">None</option><option value="api_key_header">API key in header</option><option
						value="basic">Basic</option
					><option value="bearer_static">Static bearer token</option><option value="oauth2_client_credentials"
						>OAuth2 client credentials</option
					><option value="mtls">Mutual TLS</option><option value="bundle_token">Licence bundle token</option
					></select
				>
			</div>
			<div class="field">
				<label for="new-template-auth-detail">Authentication instructions</label><textarea
					id="new-template-auth-detail"
					class="input"
					bind:value={form.auth_detail}
					disabled={busy}></textarea>
			</div>
			<div class="field">
				<label for="new-template-api-pin">Vendor API version</label><input
					id="new-template-api-pin"
					class="input"
					bind:value={form.vendor_api_pin}
					disabled={busy}
				/>
			</div>
			<details>
				<summary>Authentication parameters and credential fields</summary>
				<div class="stack advanced">
					<p class="muted">
						Use placeholders for tenant credentials. Each tenant supplies its own secrets when connecting the
						system.
					</p>
					<div class="field">
						<label for="new-template-params">Authentication parameters (JSON object)</label><textarea
							id="new-template-params"
							class="input mono"
							bind:value={params}
							placeholder={'{"header":"Authorization","value_template":"Bearer {{secret.api_key}}"}'}
							disabled={busy}></textarea>
					</div>
					<div class="field">
						<label for="new-template-schema">Credential fields (JSON array, optional)</label><textarea
							id="new-template-schema"
							class="input mono"
							bind:value={schema}
							placeholder={'[{"name":"api_key","label":"API key","secret":true}]'}
							disabled={busy}></textarea><span class="muted"
							>Leave blank to use the authentication method's default fields.</span
						>
					</div>
				</div>
			</details>
			<div class="field">
				<label for="new-template-description">Description</label><textarea
					id="new-template-description"
					class="input"
					bind:value={form.description}
					disabled={busy}></textarea>
			</div>
			{#if error}<p role="alert">{error}</p>{/if}
			<p class="muted">
				The draft editor lets you add mappings and test fixtures. Publishing requires validation and passing
				contract tests.
			</p>
			<button class="btn primary" type="submit" disabled={busy}
				>{busy ? 'Creating…' : 'Create connector template draft'}</button
			>
		</form>
	{/if}
</Drawer>

<style>
	.stack {
		display: grid;
		gap: 14px;
	}
	.advanced {
		margin-top: 12px;
	}
</style>
