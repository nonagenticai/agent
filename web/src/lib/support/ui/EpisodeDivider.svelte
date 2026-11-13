<script lang="ts">
	// One divider per handover episode: why it was queued, when, and how it ended.
	import type { Episode } from '$lib/api/types/support';
	import { clockOf } from '$lib/support/time';

	let { episode }: { episode: Episode } = $props();
	const text = $derived(
		`${episode.reason_label} · ${clockOf(episode.queued_at)}` +
			(episode.closed_at
				? ` · ${episode.resolution ? episode.resolution.replaceAll('_', ' ') : 'closed'} ${clockOf(episode.closed_at)}`
				: ''),
	);
</script>

<div class="ep" role="separator" aria-label={text}><span>{text}</span></div>

<style>
	.ep {
		display: flex;
		align-items: center;
		gap: 10px;
		color: var(--muted);
		font-size: 12px;
		margin: 6px 0;
	}
	.ep::before,
	.ep::after {
		content: '';
		flex: 1;
		border-top: 1px solid var(--line);
	}
	span {
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
		max-width: 80%;
		font-weight: 600;
	}
</style>
