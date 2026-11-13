<script lang="ts">
	// J13 Base & refinements — folded into Releases (the template-updates tab); `+page.ts` redirects old bookmarks.
	// Kept as a route so the page registry stays whole; the content lives in RefinementsPanel.
	import PageHead from '$lib/ui/PageHead.svelte';
	import RefinementsPanel from './RefinementsPanel.svelte';
</script>

<PageHead
	id="J13"
	sub="This job's workspace. Your agent runs a read-only template base with your refinements on top. Whatever you change on any design page is saved to the draft here — tried in the test chat, and live only when you publish."
/>

<RefinementsPanel />
