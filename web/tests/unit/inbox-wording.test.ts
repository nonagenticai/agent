import { cleanup, render } from '@testing-library/svelte';
import { afterEach, describe, expect, it } from 'vitest';
import type { AttentionItem } from '../../src/lib/api/types/tenant';
import Inbox from '../../src/lib/ui/Inbox.svelte';
import { prefs } from '../../src/lib/ui/prefs.svelte';

afterEach(() => {
	cleanup();
	prefs.showTech = false;
});

const item: AttentionItem = {
	id: 'release_waiting:j:cfg-2026-10-18.1',
	kind: 'release_waiting',
	severity: 'action',
	title: 'The 18 Oct release is approved and waiting to go live',
	detail: 'The 18 Oct release is approved and waiting to go live',
	code: 'cfg-2026-10-18.1',
	href: '/admin/jobs/j/release',
	action: 'Go live',
	job: 'j',
	at: null,
	owner_role: 'release_manager',
	due_at: null,
};

describe('Inbox wording', () => {
	it('shows the backend title as is and the code only with technical details on', async () => {
		const r = render(Inbox, { items: [item], roles: ['tenant_admin'] });
		expect(r.container.querySelector('.t')?.textContent).toBe(item.title);
		expect(r.container.textContent).not.toContain('cfg-2026-10-18.1');
		expect(r.container.querySelector('[data-testid="inbox-code"]')).toBeNull();
		prefs.showTech = true;
		await r.rerender({ items: [item], roles: ['tenant_admin'] });
		expect(r.container.querySelector('[data-testid="inbox-code"]')?.textContent).toBe('cfg-2026-10-18.1');
	});
});
