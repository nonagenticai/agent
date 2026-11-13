// T13 Channels: coverage, gate and web chat try-it helpers (pure).
import { describe, expect, it } from 'vitest';
import { coverage, gateCause, gateOpen, webChatBase, webChatDemoUrl } from '../../src/lib/tenant/channelsT13';
import type { ChannelJobOperation } from '../../src/lib/api/types/tenant';

describe('T13 logic', () => {
	const op = (identity_level: ChannelJobOperation['identity_level']): ChannelJobOperation => ({
		op: 'x.y',
		side_effect: 'R',
		identity_level,
	});
	const tg = { identity_start: '1' as const, identity_ceiling: '2' as const };

	it('coverage splits operations by the door’s start and ceiling; S is never reachable', () => {
		expect(coverage(op('0'), tg)).toBe('at_start');
		expect(coverage(op('1'), tg)).toBe('at_start');
		expect(coverage(op('2'), tg)).toBe('after_verify');
		expect(coverage(op('3'), tg)).toBe('refused');
		expect(coverage(op('S'), { identity_start: '1', identity_ceiling: '3' })).toBe('refused');
		expect(coverage(op('3'), { identity_start: '0', identity_ceiling: '3' })).toBe('after_verify');
		expect(coverage(op('0'), {})).toBe('refused');
	});

	it('gate is open only when the licence AND the tenant are active', () => {
		const base = { gate: { status: 'active' as const, reason: null } } as never;
		expect(gateOpen({ ...(base as object), tenant_status: 'active' } as never)).toBe(true);
		const susp = { ...(base as object), tenant_status: 'suspended' } as never;
		expect(gateOpen(susp)).toBe(false);
		expect(gateCause(susp)).toBe('tenant_suspended');
		const exp = { gate: { status: 'expired', reason: null }, tenant_status: 'active' } as never;
		expect(gateOpen(exp)).toBe(false);
		expect(gateCause(exp)).toBe('licence_expired');
	});
});

describe('T13 webhook health', () => {
	it('web chat Try it: the hosted test page sits on the snippet’s widget host, only for an enabled web chat with a key', () => {
		const snippet =
			'<script src="https://edge.example/x/webchat/widget.js" data-widget="wk_1" async></script>';
		// only the base's http(s) origin: a path is dropped, any other scheme or a non-URL falls back
		expect(webChatBase(snippet)).toBe('https://edge.example');
		expect(webChatBase(null)).toBe('https://chat.nonagentic.ai');
		expect(webChatBase('<script x></script>')).toBe('https://chat.nonagentic.ai');
		expect(webChatBase('<script src="http://localhost:9300/webchat/widget.js"></script>')).toBe(
			'http://localhost:9300',
		);
		for (const bad of ['javascript:alert(1)//x', 'data:text/html,x', 'ftp://edge.example', 'not a url'])
			expect(webChatBase(`<script src="${bad}/webchat/widget.js"></script>`), bad).toBe(
				'https://chat.nonagentic.ai',
			);
		const wc = { kind: 'web_chat' as const, enabled: true, verified: true, external_id: 'wk_1', snippet };
		expect(webChatDemoUrl(wc)).toBe('https://edge.example/webchat/demo?key=wk_1');
		expect(webChatDemoUrl({ ...wc, snippet: undefined })).toBe(
			'https://chat.nonagentic.ai/webchat/demo?key=wk_1',
		);
		expect(webChatDemoUrl({ ...wc, external_id: 'wk_a&b' })).toContain('key=wk_a%26b');
		expect(webChatDemoUrl({ ...wc, enabled: false })).toBeNull();
		// an unverified door carries no traffic: no Try it
		expect(webChatDemoUrl({ ...wc, verified: false })).toBeNull();
		expect(webChatDemoUrl({ ...wc, external_id: '' })).toBeNull();
		expect(webChatDemoUrl({ ...wc, kind: 'telegram' as const })).toBeNull();
	});
});
