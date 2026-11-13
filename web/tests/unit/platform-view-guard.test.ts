// The platform view (a super_admin with no tenant) never sends a tenant-only call: the http door refuses it with the
// API's own 409 `active_tenant_required` before any fetch (R1, plan 2026-10-30-super-admin-platform-fixes).
import { describe, expect, it } from 'vitest';
import { ApiError } from '../../src/lib/api/errors';
import { isTenantOnly, makeRequest } from '../../src/lib/api/http-client';

function door(platformOnly: boolean) {
	const sent: string[] = [];
	const req = makeRequest({
		activeTenant: () => null,
		surface: () => 'admin',
		token: async () => 't',
		platformOnly: () => platformOnly,
		fetchImpl: async (input) => {
			sent.push(String(input));
			return new Response('{"items":[],"next":null}', {
				status: 200,
				headers: { 'Content-Type': 'application/json' },
			});
		},
	});
	return { req, sent };
}

describe('platform view request guard', () => {
	it('names the tenant-only routes and leaves the platform ones alone', () => {
		for (const p of ['/api/jobs', '/api/jobs?view=board', '/api/jobs/care', '/api/inbox', '/api/tenant/home'])
			expect(isTenantOnly(p), p).toBe(true);
		for (const p of [
			'/api/me',
			'/api/tenants',
			'/api/job-templates',
			'/api/layers',
			'/api/platform/industries',
		])
			expect(isTenantOnly(p), p).toBe(false);
	});

	it('refuses a tenant-only call in the platform view without sending it', async () => {
		const { req, sent } = door(true);
		const err = await req('GET', '/api/jobs').catch((e: unknown) => e);
		expect(err).toBeInstanceOf(ApiError);
		expect((err as ApiError).status).toBe(409);
		expect((err as ApiError).code).toBe('active_tenant_required');
		expect(sent).toEqual([]);
		await req('GET', '/api/tenants');
		expect(sent).toEqual(['/api/tenants']);
	});

	it('sends tenant calls once a tenant is active', async () => {
		const { req, sent } = door(false);
		await req('GET', '/api/inbox');
		expect(sent).toEqual(['/api/inbox']);
	});
});
