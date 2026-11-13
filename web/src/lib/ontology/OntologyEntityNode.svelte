<script lang="ts">
	// Read-only entity card on the ontology graph: label, the layer it comes from (scope pill) and its typed
	// attributes with a "Personal data" badge. Source and target handles on both sides: layout.ts picks the side
	// per edge so a back-edge leaves on the left instead of looping around the card. The node box gets an explicit
	// height from geometry.nodeHeight, so the card fills it and handle points match the label placement estimate. Ported from commonpractice's OntologyEntityNode onto agent tokens (light + dark); adds the
	// base-vs-tenant origin pill. J11: an inherited card shows a lock (read-only here) instead of the scope pill (its
	// lane names the layer); a tenant card keeps the scope pill so you see which layer it extends; tenant-added
	// attributes on an inherited card carry a "+"; `data.dim` fades a card outside the selected neighbourhood.
	import { Handle, Position, type NodeProps } from '@xyflow/svelte';
	import { SCOPE_LABEL, type EntityNode } from './layout';

	let { data, selected }: NodeProps<EntityNode> = $props();
	const entity = $derived(data.entity);
</script>

<div
	class="onto-node {entity.scope}"
	class:tenant={entity.origin === 'tenant'}
	class:selected
	class:hidden={entity.hidden}
	class:dim={data.dim}
	data-entity={entity.name}
>
	<Handle id="left-in" type="target" position={Position.Left} isConnectable={false} />
	<Handle id="left-out" type="source" position={Position.Left} isConnectable={false} />
	<div class="onto-head">
		<span class="onto-name" title={entity.name}>{entity.label}</span>
		{#if entity.hidden}<span class="onto-pill muted">Hidden</span>{/if}
		{#if entity.origin === 'tenant'}
			<span
				class="onto-pill tenant"
				title="Added by this tenant (refinement) · {SCOPE_LABEL[entity.scope] ?? entity.scope} scope"
				>+ {SCOPE_LABEL[entity.scope] ?? entity.scope}</span
			>
		{:else}
			<svg
				class="onto-lock"
				viewBox="0 0 24 24"
				width="11"
				height="11"
				role="img"
				aria-label="Inherited from {entity.layer}: read-only here"
			>
				<title>Inherited from {entity.layer}: read-only here</title>
				<path
					d="M7 11V8a5 5 0 0 1 10 0v3M5 11h14v10H5z"
					fill="none"
					stroke="currentColor"
					stroke-width="2.2"
					stroke-linejoin="round"
				/>
			</svg>
		{/if}
	</div>
	<div class="onto-attrs">
		{#each entity.attributes as a (a.name)}
			<div class="onto-attr">
				<span
					class="attr-name"
					class:tenant-attr={a.origin === 'tenant' && entity.origin !== 'tenant'}
					title={a.origin === 'tenant' && entity.origin !== 'tenant' ? 'Added by this tenant' : undefined}
					>{#if a.origin === 'tenant' && entity.origin !== 'tenant'}+
					{/if}{a.name}</span
				>
				{#if a.conflict}<span
						class="attr-warn"
						title="Its base parent changed: resolve on Release → Conflicts">!</span
					>{/if}
				<span class="attr-type">{a.data_type}</span>
				{#if a.pii}<span class="attr-pii">Personal data</span>{/if}
			</div>
		{:else}
			<div class="onto-attr empty">No attributes</div>
		{/each}
	</div>
	<Handle id="right-in" type="target" position={Position.Right} isConnectable={false} />
	<Handle id="right-out" type="source" position={Position.Right} isConnectable={false} />
</div>

<style>
	.onto-node {
		--scope: var(--line);
		box-sizing: border-box;
		display: flex;
		flex-direction: column;
		width: 212px;
		height: 100%;
		background: var(--panel);
		color: var(--fg);
		border: 1px solid var(--line);
		border-radius: 8px;
		box-shadow: 0 1px 3px color-mix(in srgb, var(--fg) 10%, transparent);
		font-family: var(--sans);
		overflow: hidden;
		cursor: pointer;
	}
	.onto-node.generic {
		--scope: var(--cust);
	}
	.onto-node.industry {
		--scope: var(--tool);
	}
	.onto-node.industry_dept {
		--scope: var(--work);
	}
	.onto-node.hidden {
		opacity: 0.6;
		border-style: dashed;
	}
	.onto-node.dim {
		opacity: 0.28;
	}
	.onto-lock {
		flex-shrink: 0;
		color: var(--faint);
	}
	.attr-warn {
		flex-shrink: 0;
		font: 700 9px/13px var(--sans);
		width: 13px;
		text-align: center;
		border-radius: 50%;
		color: var(--warn);
		background: color-mix(in srgb, var(--warn) 18%, transparent);
	}
	.onto-node:hover {
		border-color: var(--muted);
	}
	.onto-node.selected {
		border-color: var(--accent);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 22%, transparent);
	}
	.onto-head {
		display: flex;
		align-items: center;
		gap: 6px;
		box-sizing: border-box;
		height: 37px;
		flex-shrink: 0;
		padding: 0 11px;
		border-bottom: 1px solid var(--line-2);
		border-left: 3px solid var(--scope);
	}
	.onto-name {
		flex: 1;
		min-width: 0;
		font-size: 12.5px;
		font-weight: 600;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.onto-pill {
		flex-shrink: 0;
		font: 600 9px/1.4 var(--sans);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		padding: 1px 6px;
		border-radius: 999px;
		color: var(--scope);
		background: color-mix(in srgb, var(--scope) 14%, transparent);
	}
	.onto-pill.tenant {
		color: var(--accent);
		background: color-mix(in srgb, var(--accent) 14%, transparent);
	}
	.onto-node.tenant {
		border: 1.5px solid color-mix(in srgb, var(--accent) 70%, var(--line));
		background: color-mix(in srgb, var(--accent) 5%, var(--panel));
	}
	.tenant-attr {
		color: var(--accent);
		font-weight: 600;
	}
	.onto-pill.muted {
		color: var(--muted);
		background: var(--sunk);
	}
	.onto-attrs {
		display: flex;
		flex-direction: column;
		gap: 2px;
		padding: 6px 11px 10px;
		flex: 1;
		min-height: 0;
		overflow-y: auto;
	}
	.onto-attr {
		display: flex;
		align-items: center;
		gap: 6px;
		font: 10.5px/17px var(--mono);
	}
	.attr-name {
		min-width: 0;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.attr-type {
		margin-left: auto;
		flex-shrink: 0;
		color: var(--muted);
	}
	.attr-pii {
		flex-shrink: 0;
		font: 700 8.5px/15px var(--sans);
		letter-spacing: 0.04em;
		padding: 0 4px;
		border-radius: 4px;
		color: var(--err);
		background: color-mix(in srgb, var(--err) 13%, transparent);
	}
	.onto-attr.empty {
		color: var(--faint);
		font-family: var(--sans);
	}
	/* Read-only attachment points; a source and a target handle share each side, so they are drawn alike. */
	.onto-node :global(.svelte-flow__handle) {
		width: 7px;
		height: 7px;
		min-width: 0;
		min-height: 0;
		background: var(--panel);
		border: 1.5px solid var(--faint);
	}
</style>
