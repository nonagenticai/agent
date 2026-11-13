<script lang="ts">
	// J17 Validation (Job › Release): the D54 TEST step. Nothing a tenant changes is live on save: the draft is tested
	// here on the Test environment (activities × customer phrasings, draft vs live, regressions with their cause)
	// before it is published on J14. Below it, the knowledge answers each release cut gave (citation check).
	import PageHead from '$lib/ui/PageHead.svelte';
	import DraftBar from '$lib/release/DraftBar.svelte';
	import J17DraftTest from './J17DraftTest.svelte';
	import J17KnowledgeCuts from './J17KnowledgeCuts.svelte';

	let tick = $state(0);
</script>

<PageHead
	id="J17"
	sub="Test the draft before you publish it. Every scenario — an example of how a customer asks for a journey activity — runs against the draft on the Test environment and is compared with what Production answers today. A regression is a scenario that passes live and fails on the draft."
/>

<DraftBar page="J17" {tick} />

<J17DraftTest onchange={() => tick++} />

<J17KnowledgeCuts />
