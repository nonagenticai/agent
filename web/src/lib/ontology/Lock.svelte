<script lang="ts">
	// Lock glyph for safety gates (fail closed, not hideable, D29). The meaning is also in the title text.
	let { title = 'Safety gate: fails closed, cannot be hidden' }: { title?: string } = $props();
</script>

<svg class="lock" viewBox="0 0 24 24" width="13" height="13" role="img" aria-label={title}>
	<title>{title}</title>
	<path
		d="M7 11V8a5 5 0 0 1 10 0v3M5 11h14v10H5z"
		fill="none"
		stroke="currentColor"
		stroke-width="2"
		stroke-linejoin="round"
	/>
</svg>

<style>
	.lock {
		display: inline-block;
		vertical-align: -2px;
		color: var(--err);
	}
</style>
