<script lang="ts">
	// P16 concept strip: what a connector template is and where it sits — vendor API → template@version (the
	// mapping) → tested (T1/T2) → published, immutable → tenant system pins a version (T3 per tenant). Doc 10 §4.2.
	import type { ConnectorTemplateSummary } from '$lib/api/types/connectors';

	let { example }: { example: ConnectorTemplateSummary | null } = $props();
</script>

<section class="tm-p16" data-testid="p16-model" aria-label="How a connector template works">
	<ol class="tm-lane">
		<li class="tm-step">
			<span class="tm-k">Vendor API</span>
			<strong>{example ? example.label : 'A system’s API'}</strong>
			<span class="tm-d">{example ? example.vendor_api_pin : 'pinned vendor API version'}</span>
		</li>
		<li class="tm-step tm-core">
			<span class="tm-k">Template <span class="mono">@version</span></span>
			<strong class="mono">{example ? `${example.key}@${example.version}` : 'key@version'}</strong>
			<ul class="tm-maps">
				<li><span>system calls</span><b aria-hidden="true">→</b><span>canonical operations</span></li>
				<li><span>system statuses</span><b aria-hidden="true">→</b><span>object statuses</span></li>
				<li><span>vendor faults</span><b aria-hidden="true">→</b><span>error codes</span></li>
			</ul>
			<span class="tm-d">+ auth, credential schema, rate limit, idempotency</span>
		</li>
		<li class="tm-step">
			<span class="tm-k">Tested</span>
			<strong>T1 · T2</strong>
			<span class="tm-d"
				>T1 recorded fixtures in CI · T2 live vendor sandbox, weekly + per vendor release</span
			>
		</li>
		<li class="tm-step">
			<span class="tm-k">Published</span>
			<strong>Immutable</strong>
			<span class="tm-d">a change is a new version; drift opens a re-record</span>
		</li>
		<li class="tm-step tm-tenant">
			<span class="tm-k">Tenant system</span>
			<strong>Pins a version</strong>
			<span class="tm-d">instance + credentials; T3 per tenant before routes go live</span>
		</li>
	</ol>
	<p class="tm-foot">
		Pull only in v1: webhooks are inert metadata, writes are confirmed by the template’s
		<span class="mono">verify_read</span>. No OAuth authorization-code installs yet (K49).
	</p>
</section>

<style>
	.tm-p16 {
		border: 1px solid var(--line);
		background: var(--panel);
		border-radius: var(--r);
		padding: 14px;
		margin-bottom: 16px;
	}
	.tm-lane {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: 1fr 1.6fr 1fr 1fr 1fr;
		gap: 22px;
	}
	.tm-step {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: 3px;
		padding: 10px 12px;
		border-radius: 8px;
		background: var(--sunk);
		border: 1px solid var(--line);
		min-width: 0;
	}
	.tm-step + .tm-step::before {
		content: '→';
		position: absolute;
		left: -18px;
		top: 50%;
		transform: translateY(-50%);
		color: var(--faint);
		font-size: 14px;
	}
	.tm-core {
		border-color: color-mix(in srgb, var(--accent) 45%, var(--line));
		background: color-mix(in srgb, var(--accent) 7%, var(--panel));
	}
	.tm-tenant {
		border-style: dashed;
	}
	.tm-k {
		font-size: 11px;
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.tm-step strong {
		font-size: 13.5px;
		overflow-wrap: anywhere;
	}
	.tm-d {
		font-size: 12px;
		color: var(--muted);
		line-height: 1.35;
	}
	.tm-maps {
		list-style: none;
		margin: 4px 0;
		padding: 0;
		display: grid;
		gap: 2px;
		font-size: 12.5px;
	}
	.tm-maps li {
		display: grid;
		grid-template-columns: 1fr auto 1fr;
		gap: 6px;
	}
	.tm-maps b {
		color: var(--accent);
		font-weight: 600;
	}
	.tm-foot {
		margin: 10px 0 0;
		font-size: 12.5px;
		color: var(--muted);
	}
	@media (max-width: 900px) {
		.tm-lane {
			grid-template-columns: 1fr;
			gap: 18px;
		}
		.tm-step + .tm-step::before {
			content: '↓';
			left: 50%;
			top: -16px;
			transform: translateX(-50%);
		}
	}
</style>
