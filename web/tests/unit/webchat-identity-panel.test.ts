// Known-customer web chat, console side: the identity settings the web chat panel saves (pure).
import { describe, expect, it } from 'vitest';
import { identityError, identityOf, parseJwks } from '../../src/lib/api/types/conversations';

const JWK = { kty: 'EC', crv: 'P-256', kid: '2026-1', x: 'x', y: 'y' };
const form = (over: Partial<Parameters<typeof identityOf>[0]> = {}) => ({
	mode: 'hs256' as const,
	issuer: ' https://shop.example ',
	signInUrl: '',
	hours: '',
	jwks: '',
	...over,
});

describe('identity settings', () => {
	it('sends only what the mode uses', () => {
		expect(identityOf(form({ mode: 'off' }))).toEqual({ mode: 'off' });
		expect(identityOf(form())).toEqual({ mode: 'hs256', issuer: 'https://shop.example', session_hours: 8 });
		expect(
			identityOf(
				form({
					mode: 'es256',
					signInUrl: 'https://shop.example/login',
					hours: '12',
					jwks: JSON.stringify(JWK),
				}),
			),
		).toEqual({
			mode: 'es256',
			issuer: 'https://shop.example',
			session_hours: 12,
			sign_in_url: 'https://shop.example/login',
			jwks: [JWK],
		});
		// the HS256 secret is never part of the settings
		expect(JSON.stringify(identityOf(form()))).not.toMatch(/secret/i);
	});

	it('says in words why it cannot be saved', () => {
		expect(identityError(form({ mode: 'off', issuer: '' }))).toBeNull();
		expect(identityError(form())).toBeNull();
		expect(identityError(form({ issuer: ' ' }))).toMatch(/issuer/);
		expect(identityError(form({ signInUrl: 'http://shop.example' }))).toMatch(/https/);
		expect(identityError(form({ hours: '25' }))).toMatch(/1 to 24/);
		expect(identityError(form({ hours: '1.5' }))).toMatch(/1 to 24/);
		expect(identityError(form({ mode: 'es256' }))).toMatch(/public key/);
		expect(identityError(form({ mode: 'es256', jwks: '{nope' }))).toMatch(/JSON/);
		expect(identityError(form({ mode: 'es256', jwks: '[{"x":"1"}]' }))).toMatch(/kid/);
		expect(identityError(form({ mode: 'es256', jwks: JSON.stringify([JWK, JWK, JWK]) }))).toMatch(
			/one or two/,
		);
		expect(identityError(form({ mode: 'es256', jwks: JSON.stringify(JWK) }))).toBeNull();
		expect(parseJwks('').jwks).toEqual([]);
	});
});
