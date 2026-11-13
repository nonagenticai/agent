import { describe, expect, it } from 'vitest';
import { crumbsFor } from '../../src/lib/shell/crumbs';

describe('crumbsFor', () => {
	it('workspace home is just the workspace', () => {
		expect(crumbsFor('/admin', 'Demo Retail', 'Customer care')).toEqual([
			{ label: 'Demo Retail', href: null },
		]);
	});

	it('a workspace page: workspace link, then the page title', () => {
		const c = crumbsFor('/admin/users', 'Demo Retail', null);
		expect(c).toHaveLength(2);
		expect(c[0]).toEqual({ label: 'Demo Retail', href: '/admin' });
		expect(c[1]!.href).toBeNull();
		expect(c[1]!.label).not.toBe('');
	});

	it('an agent page: workspace, agent (linked when the key is known), page', () => {
		const c = crumbsFor('/admin/jobs/[job]/ontology', 'Demo Retail', 'Customer care', 'customer_care_agent');
		expect(c.map((x) => x.label)[0]).toBe('Demo Retail');
		expect(c[0]!.href).toBe('/admin');
		expect(c[1]).toEqual({ label: 'Customer care', href: '/admin/jobs/customer_care_agent' });
		expect(c).toHaveLength(3);
		expect(c[2]!.href).toBeNull();
	});

	it('the agent home ends at the agent; no key means no link', () => {
		expect(crumbsFor('/admin/jobs/[job]', 'Demo Retail', 'Customer care')).toEqual([
			{ label: 'Demo Retail', href: '/admin' },
			{ label: 'Customer care', href: null },
		]);
	});

	it('studio routes start at Studio, not at a workspace', () => {
		const c = crumbsFor('/platform/tenants', 'Platform (no tenant)', null);
		expect(c[0]).toEqual({ label: 'Studio', href: '/platform' });
		expect(c.some((x) => x.label === 'Platform (no tenant)')).toBe(false);
		expect(c[c.length - 1]!.href).toBeNull();
	});

	it('an unknown route gives no crumbs', () => {
		expect(crumbsFor(null, 'Demo Retail', null)).toEqual([]);
	});
});
