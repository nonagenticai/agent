import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { createRawSnippet } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import ActionButton from '../../src/lib/ui/ActionButton.svelte';

afterEach(cleanup);

const label = createRawSnippet(() => ({ render: () => '<span>Prepare</span>' }));

describe('ActionButton', () => {
	it('is disabled and states the reason as visible text linked by aria-describedby', async () => {
		const onclick = vi.fn();
		render(ActionButton, { primary: true, disabledReason: 'Fix 3 items first', onclick, children: label });
		const btn = screen.getByRole('button', { name: 'Prepare' }) as HTMLButtonElement;
		expect(btn.disabled).toBe(true);
		const reason = screen.getByText('Fix 3 items first');
		expect(reason.id).toBeTruthy();
		expect(btn.getAttribute('aria-describedby')).toBe(reason.id);
		await fireEvent.click(btn);
		expect(onclick).not.toHaveBeenCalled();
	});

	it('is enabled, with no reason text, when the reason is null', async () => {
		const onclick = vi.fn();
		const { container } = render(ActionButton, {
			primary: true,
			disabledReason: null,
			onclick,
			children: label,
		});
		const btn = screen.getByRole('button', { name: 'Prepare' }) as HTMLButtonElement;
		expect(btn.disabled).toBe(false);
		expect(btn.hasAttribute('aria-describedby')).toBe(false);
		expect(container.querySelector('.reason')).toBeNull();
		await fireEvent.click(btn);
		expect(onclick).toHaveBeenCalledOnce();
	});
});
