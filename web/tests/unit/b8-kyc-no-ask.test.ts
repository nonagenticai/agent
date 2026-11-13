// Owner ruling 2026-10-26: customers are KYC'd and known at conversation start; the agent never asks for an order
// number, id, postcode, email or any lookup key. Guards the B8 generator prompts and the v8 seed / journey fixtures.
import { describe, expect, it } from 'vitest';
import { readFileSync, readdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '../../..');
const PROMPTS = join(ROOT, 'crates/agent-api/src/generator/prompts');
const text = (p: string) => readFileSync(join(ROOT, p), 'utf8');

const BANNED = [
	/order number/i,
	/order id\b/i,
	/postcode/i,
	/postal code/i,
	/\b(what|which)( is|’s|'s)? (your|the) (order|email|tracking|reference)/i,
	/which order or/i,
	/provide your/i,
	/email address (you|used)/i,
	/\bO-\d{4}\b.*@/,
];

describe('B8 KYC ruling: no ask-for-id', () => {
	it('the shared contract and the journey, use-case, cluster and object prompts carry the identity rule', () => {
		for (const f of ['_contract.md', 'journey.md', 'use_cases.md', 'clusters.md', 'objects.md']) {
			const t = readFileSync(join(PROMPTS, f), 'utf8');
			expect(t, f).toMatch(/order number/i);
			expect(t, f).toMatch(/never/i);
		}
		expect(readFileSync(join(PROMPTS, '_contract.md'), 'utf8')).toContain('KYC');
	});

	it('every prompt file is covered by the contract (appended to each call)', () => {
		const files = readdirSync(PROMPTS).filter((f) => f.endsWith('.md'));
		expect(files).toContain('_contract.md');
	});

	it('the journey sources and acceptance scripts never ask the customer for a reference', () => {
		const files = [
			'web/src/lib/journey/grids.ts',
			'web/src/lib/journey/catalogue.ts',
			'crates/agent-api/src/journey/job.rs',
			'crates/agent-api/src/release/overlay.rs',
		];
		for (const f of files) {
			const t = text(f);
			for (const re of BANNED) expect(re.test(t), `${f} matches ${re}`).toBe(false);
		}
		// acceptance scripts state the rule in comments and carry data blobs; scan only the asks, in code lines
		const ASKS = [
			/\b(what|which)( is|’s|'s)? (your|the) (order|email|tracking|reference)/i,
			/which order or/i,
			/provide your/i,
			/(enter|give|send|tell) (me )?your (order|postcode|email|tracking)/i,
		];
		for (const f of readdirSync(join(ROOT, 'scripts/local')).filter((n) => n.endsWith('.mjs'))) {
			const code = text(`scripts/local/${f}`)
				.split('\n')
				.filter((l) => !l.trim().startsWith('//') && l.length < 600)
				.join('\n');
			for (const re of ASKS) expect(re.test(code), `scripts/local/${f} matches ${re}`).toBe(false);
		}
	});

	it('the track-parcel grid activity takes no lookup key and the credential object holds none', () => {
		const g = text('web/src/lib/journey/grids.ts');
		expect(g).not.toMatch(/'tracking_ref',\s*\]/);
		const c = text('web/src/lib/journey/catalogue.ts');
		expect(c).not.toMatch(/credential: \[[^\]]*(order_number|email_or_postcode)/);
	});

	it('the identity levels say the customer is already verified (KYC)', () => {
		expect(text('crates/agent-api/src/journey/job.rs')).toContain('already verified (KYC)');
	});
});
