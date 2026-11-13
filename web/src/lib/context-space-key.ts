// D61: one context-space key scheme — `<industry>:<area>:<slug>`.
//   industry  the industry key (`retail`, `utilities`, `it_services`): lower_snake_case
//   area      a D60 area of the department (`order-support`, `logistics`, `payments`, `faq`): the area dimension's value
//             key, lower-case with dashes
//   slug      the context space inside the area (`track_parcel`, `refund_status`): lower_snake_case
// `contextSpaceKey` is the only place a key is built; fixtures and API code call it (or match it, checked in tests).

const INDUSTRY = /^[a-z][a-z0-9]*(_[a-z0-9]+)*$/;
const AREA = /^[a-z][a-z0-9]*(-[a-z0-9]+)*$/;
const SLUG = /^[a-z][a-z0-9]*(_[a-z0-9]+)*$/;

export interface ContextSpaceKeyParts {
	industry: string;
	area: string;
	slug: string;
}

/** Build a context-space key. Throws on a part that breaks the scheme (a fixture/programming error, not input). */
export function contextSpaceKey(industry: string, area: string, slug: string): string {
	if (!INDUSTRY.test(industry))
		throw new Error(`context space: industry “${industry}” is not lower_snake_case`);
	if (!AREA.test(area))
		throw new Error(`context space: area “${area}” is not a lower-case-with-dashes area key`);
	if (!SLUG.test(slug)) throw new Error(`context space: slug “${slug}” is not lower_snake_case`);
	return `${industry}:${area}:${slug}`;
}

/** Split a key into its parts, or null when it does not follow the scheme. */
export function parseContextSpaceKey(key: string): ContextSpaceKeyParts | null {
	const [industry, area, slug, ...rest] = key.split(':');
	if (rest.length || !industry || !area || !slug) return null;
	if (!INDUSTRY.test(industry) || !AREA.test(area) || !SLUG.test(slug)) return null;
	return { industry, area, slug };
}

export function isContextSpaceKey(key: string): boolean {
	return parseContextSpaceKey(key) !== null;
}

/** The area a context-space key belongs to (`retail:logistics:track_parcel` → `logistics`); null when malformed. */
export function areaOfContextSpace(key: string): string | null {
	return parseContextSpaceKey(key)?.area ?? null;
}

/** The parts of a key a fixture wrote by hand; throws when it does not follow the scheme. */
export function requireContextSpaceKey(key: string): ContextSpaceKeyParts {
	const parts = parseContextSpaceKey(key);
	if (!parts) throw new Error(`context space: “${key}” is not <industry>:<area>:<snake_slug>`);
	return parts;
}
