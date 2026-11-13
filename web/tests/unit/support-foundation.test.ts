// T8 web foundation of the staff workspace: the one time format, per-channel limits and keyboard bindings.
import { describe, expect, it, vi } from 'vitest';
import { limitFor } from '../../src/lib/support/limits';
import { bindKeys } from '../../src/lib/support/keys';
import { clockOf, relative, zoneLabel } from '../../src/lib/support/time';
import { stamp } from '../../src/lib/conversations/labels';

describe('limits and time', () => {
	it('limitFor mirrors the server constants: telegram 4000, others 8000', () => {
		expect(limitFor('telegram')).toBe(4000);
		expect(limitFor('web_chat')).toBe(8000);
		expect(limitFor('helpdesk')).toBe(8000);
	});

	it('stamp carries no zone name', () => {
		expect(stamp('2026-10-21T20:38:00Z')).not.toMatch(/CES?T|CET|UTC|GMT/);
		expect(stamp('2026-10-21T20:38:00Z')).toMatch(/21 Oct 2026/);
	});

	it('zoneLabel is an IANA zone and clockOf has no zone suffix', () => {
		expect(zoneLabel()).toMatch(/^[A-Za-z_]+(\/[A-Za-z_+\-0-9]+)*$|^UTC$/);
		expect(clockOf('2026-10-21T20:38:00Z', Date.parse('2026-10-21T21:00:00Z'))).toMatch(/^\d\d:\d\d$/);
	});

	it('relative reads past and future', () => {
		const now = Date.parse('2026-10-21T12:00:00Z');
		expect(relative('2026-10-21T11:59:40Z', now)).toBe('just now');
		expect(relative('2026-10-21T11:55:00Z', now)).toBe('5 min ago');
		expect(relative('2026-10-21T09:00:00Z', now)).toBe('3 h ago');
		expect(relative('2026-10-19T12:00:00Z', now)).toBe('2 d ago');
		expect(relative('2026-10-21T12:30:00Z', now)).toBe('in 30 min');
	});
});

describe('keys', () => {
	const press = (target: EventTarget, init: KeyboardEventInit) =>
		target.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init }));

	it('keys ignore T while typing, fire Mod+Enter', () => {
		const t = vi.fn();
		const send = vi.fn();
		const esc = vi.fn();
		const off = bindKeys(window, { t, 'Mod+Enter': send, Escape: esc });
		const area = document.createElement('textarea');
		document.body.append(area);
		area.focus();
		press(area, { key: 't' });
		expect(t).not.toHaveBeenCalled();
		press(area, { key: 'Enter', ctrlKey: true });
		expect(send).toHaveBeenCalledTimes(1);
		press(area, { key: 'Enter', metaKey: true });
		expect(send).toHaveBeenCalledTimes(2);
		press(area, { key: 'Escape' });
		expect(esc).toHaveBeenCalledTimes(1);
		press(document.body, { key: 't' });
		expect(t).toHaveBeenCalledTimes(1);
		press(document.body, { key: 't', ctrlKey: true }); // a browser shortcut, not ours
		expect(t).toHaveBeenCalledTimes(1);
		off();
		press(document.body, { key: 't' });
		expect(t).toHaveBeenCalledTimes(1);
		area.remove();
	});

	it('single keys are ignored in inputs and contenteditable, too', () => {
		const j = vi.fn();
		const el = document.createElement('div');
		const off = bindKeys(el, { j });
		const input = document.createElement('input');
		const rich = document.createElement('div');
		rich.setAttribute('contenteditable', 'true');
		el.append(input, rich);
		document.body.append(el);
		press(input, { key: 'j' });
		press(rich, { key: 'j' });
		expect(j).not.toHaveBeenCalled();
		press(el, { key: 'j' });
		expect(j).toHaveBeenCalledTimes(1);
		off();
		el.remove();
	});
});
