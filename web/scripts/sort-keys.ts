// Deterministic JSON for the golden exporters: a deep copy with object keys in sorted order.
const byStr = (a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0);

/** A deep copy with object keys in sorted order (arrays keep their order); drops `undefined`. */
export function sortKeys<T>(v: T): T {
	if (Array.isArray(v)) return v.map(sortKeys) as T;
	if (v && typeof v === 'object') {
		const out: Record<string, unknown> = {};
		for (const k of Object.keys(v).sort(byStr)) {
			const x = (v as Record<string, unknown>)[k];
			if (x !== undefined) out[k] = sortKeys(x);
		}
		return out as T;
	}
	return v;
}
