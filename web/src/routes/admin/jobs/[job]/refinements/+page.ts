// J13 folded into Releases (changes tab). Old bookmarks land there.
import { redirect } from '@sveltejs/kit';
import { redirectTarget } from '$lib/redirects';

export function load({ params }: { params: { job: string } }) {
	const to = redirectTarget('/admin/jobs/[job]/refinements', params.job);
	if (to) redirect(307, to);
}
