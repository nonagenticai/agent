// Pure helpers of the connectors area, shared by its fixture methods and its pages (never by other areas):
// operation keys, route resolution (variant → `*`), the URL guard, a small JSONPath evaluator, the closed transform
// list and tenant-template validation. Each mirrors a rule of docs 10–12 so the backend can be checked against it.
import type {
	ExpectedOperation,
	Idempotency,
	JobOperations,
	OpRef,
	OperationRoute,
	ResolvedRoute,
	RouteTarget,
	SideEffect,
	TenantConnectorTemplate,
	ValidationError,
	Env,
} from '$lib/api/types/connectors';

/** `order.read`, `case.create{return}` (variant `*` is left off). */
export function opKey(o: OpRef): string {
	return `${o.object}.${o.verb}${o.variant && o.variant !== '*' ? `{${o.variant}}` : ''}`;
}

export function parseOpKey(key: string): OpRef {
	const m = /^([a-z_]+)\.([a-z_]+)(?:\{([^}]+)\})?$/.exec(key.trim());
	if (!m) return { object: '', verb: '', variant: '*' };
	return { object: m[1]!, verb: m[2]!, variant: m[3] ?? '*' };
}

export const sameOp = (a: OpRef, b: OpRef) =>
	a.object === b.object && a.verb === b.verb && (a.variant || '*') === (b.variant || '*');

/** K48 counting unit: `(object, verb)`, variants not counted separately. */
export function opCounts(ops: ExpectedOperation[]): { ops: number; objects: number; required: number } {
	const pairs = new Set(ops.map((o) => `${o.object}.${o.verb}`));
	const required = new Set(ops.filter((o) => o.required).map((o) => `${o.object}.${o.verb}`));
	return { ops: pairs.size, objects: new Set(ops.map((o) => o.object)).size, required: required.size };
}

export function targetLabel(t: RouteTarget, instanceLabel: (id: string) => string = (id) => id): string {
	switch (t.kind) {
		case 'instance':
			return instanceLabel(t.instance_id) + (t.detail ? ` · ${t.detail}` : '');
		case 'builtin':
			return 'built-in (agent)';
		case 'agent_cases':
			return 'agent_cases (fallback)';
		case 'deep_link':
			return 'deep link';
		case 'not_offered':
			return 'not offered';
	}
}

export const sameTarget = (a: RouteTarget, b: RouteTarget) => JSON.stringify(a) === JSON.stringify(b);

function findRoute(routes: OperationRoute[], op: OpRef, env: Env): OperationRoute | null {
	return routes.find((r) => r.env === env && sameOp(r, op)) ?? null;
}

/** What route readiness reads: the routes, each instance's stub/platform flags and its T3-green ops. */
export type RouteLookup = Pick<JobOperations, 'routes' | 'tested'> & {
	instances: Pick<JobOperations['instances'][number], 'id' | 'stub' | 'platform'>[];
};

/**
 * The route the runner would use for `op` on `env` (doc 12 §3.3 step 6): the variant's own route, else the `*`
 * route of the same (object, verb); plus readiness per doc 12 §3.5. No suggestion (see `resolveRoute`).
 */
export function routeReadiness(data: RouteLookup, op: OpRef, env: Env): Omit<ResolvedRoute, 'suggested'> {
	let route = findRoute(data.routes, op, env);
	let inherited = false;
	if (!route && op.variant !== '*') {
		route = findRoute(data.routes, { ...op, variant: '*' }, env);
		inherited = !!route;
	}
	const key = opKey(op);
	if (!route) return { route: null, inherited: false, readiness: 'missing' };
	const t = route.target;
	if (t.kind === 'not_offered') return { route, inherited, readiness: 'not_offered' };
	if (t.kind !== 'instance') return { route, inherited, readiness: 'ok' };
	const inst = data.instances.find((i) => i.id === t.instance_id);
	if (!inst || inst.stub) return { route, inherited, readiness: 'stub' };
	if (inst.platform) return { route, inherited, readiness: 'ok' };
	const tested = data.tested[inst.id] ?? [];
	const servedAs = inherited ? opKey({ ...op, variant: '*' }) : key;
	return { route, inherited, readiness: tested.includes(servedAs) ? 'ok' : 'untested' };
}

/** `routeReadiness` plus, when no route exists, the route doc 10 §5.2 step 5 would suggest. */
export function resolveRoute(data: Omit<JobOperations, 'cells'>, op: OpRef, env: Env): ResolvedRoute {
	const r = routeReadiness(data, op, env);
	return { ...r, suggested: r.route ? null : suggestRoute(data, op, env) };
}

/** The built-in operations, for messages. */
export const BUILTIN_OPS_TEXT = 'knowledge.search, knowledge.answer and rule.evaluate';

/**
 * The operations the agent itself runs (`builtin` targets), as Rust `runtime::routes::builtin_family`:
 * `knowledge.search`, `knowledge.answer` (no variant) and `rule.evaluate` (any variant).
 */
export function isBuiltinOp(op: OpRef): boolean {
	const noVariant = !op.variant || op.variant === '*';
	if (op.object === 'rule') return op.verb === 'evaluate';
	if (op.object === 'knowledge') return noVariant && (op.verb === 'search' || op.verb === 'answer');
	return false;
}

/**
 * Doc 10 §5.2 step 5: built-in for `isBuiltinOp`; else the first instance on that env that implements the op;
 * case/handover → agent_cases.
 */
export function suggestRoute(data: Omit<JobOperations, 'cells'>, op: OpRef, env: Env): RouteTarget | null {
	const key = opKey(op);
	const star = opKey({ ...op, variant: '*' });
	if (isBuiltinOp(op)) return { kind: 'builtin' };
	const implementsIt = (id: string) => {
		const impl = data.implements[id] ?? [];
		return impl.includes(key) || impl.includes(star);
	};
	// Tenant systems first (the first selected system that implements it), then platform connectors.
	const candidates = [
		...data.instances.filter((i) => !i.platform),
		...data.instances.filter((i) => i.platform),
	];
	for (const inst of candidates) {
		if (inst.stub) continue;
		if (inst.env !== env && inst.env !== 'all') continue;
		if (env === 'prod' && data.draft_instances.includes(inst.id)) continue;
		if (!implementsIt(inst.id)) continue;
		if (inst.template_key === 'agent_cases') return { kind: 'agent_cases' };
		return { kind: 'instance', instance_id: inst.id };
	}
	if (op.object === 'case' || op.object === 'handover') return { kind: 'agent_cases' };
	return null;
}

// ---- URL guard (doc 10 §5.2 step 4, doc 12 §3.3 step 8) -------------------------------------------------------

/** Shape check + SSRF guard (`not ip.is_global`, CGNAT blocked). Returns the refusal, or null when acceptable. */
export function checkTarget(raw: string): { code: string; message: string } | null {
	let u: URL;
	try {
		u = new URL(raw.trim());
	} catch {
		return { code: 'invalid_url', message: 'Enter a full URL such as https://shop.example.com.' };
	}
	if (u.protocol !== 'https:')
		return { code: 'invalid_url', message: 'Only https:// base URLs are allowed for connector instances.' };
	if (u.username || u.password)
		return { code: 'invalid_url', message: 'Put credentials in the credential fields, never in the URL.' };
	if (u.search || u.hash) return { code: 'invalid_url', message: 'A base URL has no query or fragment.' };
	const host = u.hostname.replace(/^\[|\]$/g, '').toLowerCase();
	const internal =
		host === 'localhost' ||
		host.endsWith('.localhost') ||
		host.endsWith('.internal') ||
		host.endsWith('.local') ||
		host.endsWith('.svc') ||
		host.endsWith('.cluster.local') ||
		(host.includes(':') && !publicV6(host));
	const v4 = /^(\d+)\.(\d+)\.(\d+)\.(\d+)$/.exec(host);
	let privateV4 = false;
	if (v4) {
		const [a, b] = [Number(v4[1]), Number(v4[2])];
		privateV4 =
			a === 10 ||
			a === 127 ||
			a === 0 ||
			(a === 172 && b >= 16 && b <= 31) ||
			(a === 192 && b === 168) ||
			(a === 169 && b === 254) ||
			(a === 100 && b >= 64 && b <= 127);
	}
	if (internal || privateV4)
		return {
			code: 'invalid_target',
			message: `${host} resolves to a non-public address (private, loopback, link-local or CGNAT). The runner only calls public hosts.`,
		};
	return null;
}

/** The eight 16-bit groups of a URL-normalised IPv6 hostname (`::ffff:7f00:1`), or null when it is not one. */
function v6Segments(host: string): number[] | null {
	const halves = host.split('::');
	if (halves.length > 2) return null;
	const groups = (h: string) => (h ? h.split(':') : []);
	const head = groups(halves[0]!);
	const tail = halves.length === 2 ? groups(halves[1]!) : [];
	const fill = 8 - head.length - tail.length;
	if (halves.length === 2 ? fill < 1 : fill !== 0) return null;
	const all = [...head, ...Array<string>(fill).fill('0'), ...tail];
	if (!all.every((g) => /^[0-9a-f]{1,4}$/i.test(g))) return null;
	return all.map((g) => parseInt(g, 16));
}

/** Server `public_v4` (netguard.rs): false for every special-purpose IPv4 range. */
function publicV4([a, b, c, d]: number[]): boolean {
	return !(
		(a === 0 && b === 0 && c === 0 && d === 0) ||
		a === 127 ||
		a === 10 ||
		(a === 172 && b! >= 16 && b! <= 31) ||
		(a === 192 && b === 168) ||
		(a === 169 && b === 254) ||
		(a === 255 && b === 255 && c === 255 && d === 255) ||
		(a! >= 224 && a! <= 239) ||
		(a === 192 && b === 0 && c === 2) ||
		(a === 198 && b === 51 && c === 100) ||
		(a === 203 && b === 0 && c === 113) ||
		a === 0 ||
		(a === 100 && b! >= 64 && b! <= 127) ||
		(a === 192 && b === 0 && c === 0) ||
		(a === 198 && (b === 18 || b === 19)) ||
		a! >= 240
	);
}

/**
 * Server `public_v6` (netguard.rs): IPv4-mapped (`::ffff:a.b.c.d`) and NAT64 (`64:ff9b::/96`) are judged by their
 * IPv4; unspecified, loopback, multicast, ULA fc00::/7, link-local fe80::/10, documentation, 6to4, Teredo and
 * IPv4-compatible `::/96` are never public. Anything that does not parse as IPv6 is not public.
 */
function publicV6(host: string): boolean {
	const s = v6Segments(host);
	if (!s) return false;
	const v4 = (hi: number, lo: number) => [hi >> 8, hi & 0xff, lo >> 8, lo & 0xff];
	if (s.slice(0, 5).every((x) => x === 0) && s[5] === 0xffff) return publicV4(v4(s[6]!, s[7]!));
	if (s[0] === 0x64 && s[1] === 0xff9b && s.slice(2, 6).every((x) => x === 0))
		return publicV4(v4(s[6]!, s[7]!));
	return !(
		s.slice(0, 6).every((x) => x === 0) || // ::/96 incl. :: and ::1 and IPv4-compatible
		(s[0]! & 0xff00) === 0xff00 ||
		(s[0]! & 0xfe00) === 0xfc00 ||
		(s[0]! & 0xffc0) === 0xfe80 ||
		(s[0] === 0x2001 && s[1] === 0x0db8) ||
		s[0] === 0x2002 ||
		(s[0] === 0x2001 && s[1] === 0)
	);
}

// ---- JSONPath (the subset mapping overrides use: $, .key, ['key'], [n], [*]) ---------------------------------

export function jsonPath(
	doc: unknown,
	path: string,
): { ok: true; value: unknown } | { ok: false; error: string } {
	const p = path.trim();
	if (!p.startsWith('$')) return { ok: false, error: 'A JSONPath starts with $.' };
	const tokens: (string | number | '*')[] = [];
	const re = /\.([A-Za-z_][\w-]*)|\[(\d+)\]|\[\*\]|\['([^']+)'\]|\.\*/gy;
	re.lastIndex = 1;
	while (re.lastIndex < p.length) {
		const at = re.lastIndex;
		const m = re.exec(p);
		if (!m || m.index !== at) return { ok: false, error: `Cannot read the path at “${p.slice(at)}”.` };
		if (m[1] !== undefined) tokens.push(m[1]);
		else if (m[2] !== undefined) tokens.push(Number(m[2]));
		else if (m[3] !== undefined) tokens.push(m[3]);
		else tokens.push('*');
	}
	let cur: unknown[] = [doc];
	let wild = false;
	for (const t of tokens) {
		const next: unknown[] = [];
		for (const c of cur) {
			if (c === null || typeof c !== 'object') continue;
			if (t === '*') {
				wild = true;
				next.push(...(Array.isArray(c) ? c : Object.values(c)));
			} else if (typeof t === 'number') {
				if (Array.isArray(c) && t < c.length) next.push(c[t]);
			} else if (!Array.isArray(c) && t in (c as Record<string, unknown>)) {
				next.push((c as Record<string, unknown>)[t]);
			}
		}
		cur = next;
	}
	if (cur.length === 0) return { ok: false, error: 'No match in the sample response.' };
	return { ok: true, value: wild ? cur : cur[0] };
}

// ---- Tenant templates: declarative mapping only (OQ-3) --------------------------------------------------------

/** The closed transform list a tenant mapping may use. Anything else is refused at validation. */
export const TRANSFORMS = [
	'none',
	'trim',
	'lowercase',
	'uppercase',
	'to_int',
	'to_decimal',
	'iso_date',
	'enum_map',
	'split_first',
	'join',
	'mask_last4',
] as const;

/**
 * The one parameterised transform: `kv_line:<KEY>` reads the value of the first `KEY:value` line of a string
 * (`KEY` matching `^[A-Z0-9_]+$`), e.g. a script gateway's `stdout`. Returns `KEY`, or null.
 */
export function kvKey(transform: unknown): string | null {
	if (typeof transform !== 'string' || !transform.startsWith('kv_line:')) return null;
	const key = transform.slice('kv_line:'.length);
	return /^[A-Z0-9_]+$/.test(key) ? key : null;
}

/** A transform a mapping may use: one of `TRANSFORMS`, or `kv_line:<KEY>`. */
export const transformAllowed = (t: unknown) =>
	(TRANSFORMS as readonly unknown[]).includes(t) || kvKey(t) !== null;

/**
 * The value of the first `KEY:value` line of `text` (lines split on `\n`; a line's leading whitespace and the
 * whitespace around the value are ignored, so CRLF output reads the same), or null when no line has the key.
 */
export function kvLine(text: string, key: string): string | null {
	for (const line of text.split('\n')) {
		const l = line.trimStart();
		if (l.startsWith(`${key}:`)) return l.slice(key.length + 1).trim();
	}
	return null;
}

/**
 * Does a `BODY <jsonpath> [<transform>] = <value>` fault-map entry match a 2xx body? The path must match one scalar;
 * `<transform>` is `none` or `kv_line:<KEY>`; the (transformed) string, number or boolean must equal `<value>` as text
 * (trimmed). Any other `system` text never matches. A gateway that answers HTTP 2xx for a failed action (a script
 * gateway's `STATUS:FAILED`) is classified as a fault this way:
 * `{ system: 'BODY $.stdout kv_line:STATUS = FAILED', code: 'upstream_unavailable' }`.
 */
export function matchBodyFault(system: string, body: unknown): boolean {
	const s = system.trim();
	if (!/^BODY\s/.test(s)) return false;
	const eq = s.indexOf('=');
	if (eq < 0) return false;
	const parts = s.slice(4, eq).split(/\s+/).filter(Boolean);
	if (parts.length < 1 || parts.length > 2) return false;
	const m = jsonPath(body, parts[0]!);
	if (!m.ok) return false;
	let v = m.value;
	const t = parts[1];
	if (t !== undefined && t !== 'none') {
		const key = kvKey(t);
		if (key === null || typeof v !== 'string') return false;
		v = kvLine(v, key);
		if (v === null) return false;
	}
	const text = typeof v === 'string' ? v : typeof v === 'number' || typeof v === 'boolean' ? String(v) : null;
	return text !== null && text === s.slice(eq + 1).trim();
}

const REDACTED = '[redacted]';
/** A key naming a secret: case-insensitive, `_`/`-` ignored; `authorization` exactly, else containing one of these. */
const SECRET_KEY_PARTS = ['password', 'passwd', 'secret', 'token', 'privatekey', 'apikey'];
const secretKey = (key: string) => {
	const k = key.replace(/[_-]/g, '').toLowerCase();
	return k === 'authorization' || SECRET_KEY_PARTS.some((p) => k.includes(p));
};
/** A `KEY:value` line whose KEY (`^[A-Z0-9_]+$`) names a secret; group 1 = indent + `KEY:`, group 2 = a trailing CR. */
const SECRET_LINE = /^(\s*[A-Z0-9_]*(?:PASSWORD|SECRET|TOKEN|PRIVATE_KEY)[A-Z0-9_]*:).*?(\r?)$/;

/**
 * The backend's `netguard::scrub_secrets`: a vendor response body as it may be stored or served (contract-test
 * responses, samples). The value of every key naming a secret becomes `[redacted]` (a boolean or null is kept: no
 * secret, maybe a mapped flag such as `temporary_password_sent`); in every string, a `KEY:value` line naming a secret
 * becomes `KEY:[redacted]` (the key stays, so mapping authors still see the shape). Recursive; idempotent.
 */
export function scrubSecrets(v: unknown): unknown {
	if (typeof v === 'string')
		return v.includes(':')
			? v
					.split('\n')
					.map((l) => l.replace(SECRET_LINE, `$1${REDACTED}$2`))
					.join('\n')
			: v;
	if (Array.isArray(v)) return v.map(scrubSecrets);
	if (v !== null && typeof v === 'object')
		return Object.fromEntries(
			Object.entries(v).map(([k, x]) => [
				k,
				typeof x === 'boolean' || x === null ? x : secretKey(k) ? REDACTED : scrubSecrets(x),
			]),
		);
	return v;
}

/**
 * Placeholders a request template may use: canonical input fields, env IDs and declared secrets.
 * `{{ns.name ?? ns2.name2}}` (R10) falls back to `ns2.name2` when `ns.name` has no value (an optional input such as
 * `canonical.customer_email` → the configured `env.requester_id`). A trailing `?` (`{{canonical.from?}}`,
 * `{{a ?? b?}}`) marks it OPTIONAL: with no value the renderer drops what it fills. Only a whole query value
 * (`?k={{x?}}`) and a whole JSON object member value (`"k":"{{x?}}"`) can drop; an optional anywhere else is refused
 * (`optional_not_droppable`). Rust twin: `connectors::validate::placeholders`.
 */
const PLACEHOLDER =
	/\{\{\s*([a-z_]+)\.([a-z_][a-z0-9_]*)(?:\s*\?\?\s*([a-z_]+)\.([a-z_][a-z0-9_]*))?\s*(\?)?\s*\}\}/g;

export interface Placeholder {
	start: number;
	end: number;
	ns: string;
	name: string;
	fallback?: [string, string];
	optional: boolean;
}

/** Every placeholder of `s`, left to right (Rust twin: `connectors::validate::placeholders`). */
export function placeholders(s: string): Placeholder[] {
	return [...s.matchAll(PLACEHOLDER)].map((m) => ({
		start: m.index,
		end: m.index + m[0].length,
		ns: m[1]!,
		name: m[2]!,
		fallback: m[3] === undefined ? undefined : [m[3], m[4]!],
		optional: m[5] !== undefined,
	}));
}

/** Byte-for-byte twin of `mapping::query_start`: the first `?` outside a `{{…}}` placeholder. */
function queryStart(raw: string): number {
	let depth = 0;
	for (let i = 0; i < raw.length;) {
		if (raw.startsWith('{{', i)) {
			depth++;
			i += 2;
		} else if (depth > 0 && raw.startsWith('}}', i)) {
			depth--;
			i += 2;
		} else if (depth === 0 && raw[i] === '?') return i;
		else i++;
	}
	return -1;
}

/** An optional in a path may drop only as a whole query value (`?k={{x?}}`, `&k={{x?}}`). */
function droppableInPath(path: string, p: Placeholder): boolean {
	const q = queryStart(path);
	return q >= 0 && q < p.start && path[p.start - 1] === '=' && (p.end === path.length || path[p.end] === '&');
}

/** An optional in a request may drop only as a whole-string JSON object member value, `"k": "{{x?}}"` then `,` or `}`. */
function droppableInRequest(request: string, p: Placeholder): boolean {
	const head = request.slice(0, p.start);
	const tail = request.slice(p.end);
	return (
		head.endsWith('"') &&
		tail.startsWith('"') &&
		/:\s*$/.test(head.slice(0, -1)) &&
		/^\s*[,}]/.test(tail.slice(1))
	);
}
/** Literal values that look like credentials (a secret must come only from a typed `{{secret.*}}` placeholder). */
const SECRETISH =
	/(sk_(live|test)_[A-Za-z0-9]+|rk_live_[A-Za-z0-9]+|shpat_[A-Za-z0-9]+|Bearer\s+[A-Za-z0-9._-]{12,}|Basic\s+[A-Za-z0-9+/=]{12,}|apikey:[A-Za-z0-9]{8,})/;

export const needsKey = (s: SideEffect) => s !== 'R';

/** What the operation catalogue says a template op must produce and accept (subset of operation_def). */
export interface OpDefLite extends OpRef {
	side_effect: SideEffect;
	input_fields: string[];
	output_fields: string[];
}

/**
 * Validates a tenant template against `operation_def` (doc 12 §4, D31): known ops, output fields that exist,
 * transforms from the closed list, placeholders that name inputs / env IDs / declared secrets, no literal secrets,
 * idempotency other than `none` for W/X/P.
 */
export function validateTenantTemplate(t: TenantConnectorTemplate, defs: OpDefLite[]): ValidationError[] {
	const errors: ValidationError[] = [];
	const secrets = new Set(t.credential_schema.filter((f) => f.secret).map((f) => f.name));
	const config = new Set(t.credential_schema.filter((f) => !f.secret).map((f) => f.name));
	if (t.ops.length === 0)
		errors.push({ path: 'ops', code: 'no_operations', message: 'Add at least one operation.' });
	t.ops.forEach((op, i) => {
		const at = `ops[${i}] ${opKey(op)}`;
		const def =
			defs.find((d) => sameOp(d, op)) ??
			defs.find((d) => sameOp(d, { ...op, variant: '*' })) ??
			(op.def ? { object: op.object, verb: op.verb, variant: op.variant, ...op.def } : null);
		if (!def) {
			errors.push({
				path: at,
				code: 'unknown_operation',
				message: `${opKey(op)} is not in the operation catalogue.`,
			});
			return;
		}
		for (const m of op.request.matchAll(PLACEHOLDER)) {
			const refs =
				m[3] === undefined
					? [[m[1], m[2]]]
					: [
							[m[1], m[2]],
							[m[3], m[4]],
						];
			// A canonical name on the LEFT of `??` need not be a declared input: an optional future input
			// (`canonical.customer_email` before the B6 catalogue declares it) resolves to absent and the fallback —
			// still checked — runs. Rust twin: `connectors::validate::validate_op`.
			refs.forEach(([ns, name], k) => {
				const optionalPrimary = k === 0 && m[3] !== undefined && ns === 'canonical';
				const ok =
					(ns === 'canonical' && (optionalPrimary || def.input_fields.includes(name!))) ||
					(ns === 'secret' && secrets.has(name!)) ||
					(ns === 'config' && config.has(name!)) ||
					ns === 'env' ||
					(ns === 'step' && name === 'idempotency_key');
				if (!ok)
					errors.push({
						path: `${at}.request`,
						code: 'unknown_placeholder',
						message: `{{${ns}.${name}}} is not an input of ${opKey(def)}, a declared secret, a config field or an env ID.`,
					});
			});
		}
		for (const [where, text, droppable] of [
			['path', op.path, droppableInPath],
			['request', op.request, droppableInRequest],
		] as const)
			for (const p of placeholders(text))
				if (p.optional && !droppable(text, p))
					errors.push({
						path: `${at}.${where}`,
						code: 'optional_not_droppable',
						message: `${text.slice(p.start, p.end)} is optional but sits where it cannot be dropped: an optional placeholder must be a whole query value (?k={{canonical.x?}}) or a whole JSON object member value ("k": "{{canonical.x?}}").`,
					});
		if (SECRETISH.test(op.request) || SECRETISH.test(op.path))
			errors.push({
				path: `${at}.request`,
				code: 'literal_secret',
				message:
					'A credential is written into the template. Declare it as a secret field and use {{secret.<name>}}.',
			});
		op.field_map.forEach((f, j) => {
			if (!def.output_fields.includes(f.field))
				errors.push({
					path: `${at}.field_map[${j}]`,
					code: 'unknown_field',
					message: `${f.field} is not an output field of ${opKey(def)} (${def.output_fields.join(', ')}).`,
				});
			if (!transformAllowed(f.transform))
				errors.push({
					path: `${at}.field_map[${j}].transform`,
					code: 'transform_not_allowed',
					message: `“${f.transform}” is not in the closed transform list (${TRANSFORMS.join(', ')}, kv_line:<KEY>).`,
				});
			if (!f.path.startsWith('$'))
				errors.push({
					path: `${at}.field_map[${j}]`,
					code: 'invalid_path',
					message: 'A response path starts with $.',
				});
		});
		if (needsKey(def.side_effect) && op.idempotency.kind === 'none')
			errors.push({
				path: `${at}.idempotency`,
				code: 'idempotency_required',
				message: `${opKey(def)} is ${def.side_effect}: declare an Idempotency header or a dup_search; none is refused.`,
			});
	});
	errors.push(...validateAuth(t));
	return errors;
}

/** The secret credential fields an `mtls` template must declare (`ca_pem` is optional). */
export const MTLS_FIELDS = ['client_cert_pem', 'client_key_pem'] as const;
/** `auth_params` keys that would turn TLS certificate verification off: always refused (there is no such option). */
export const TLS_VERIFY_OFF = [
	'danger_accept_invalid_certs',
	'insecure_skip_verify',
	'tls_insecure',
	'tls_verify',
	'verify_tls',
] as const;
/** RFC 9110 `token`: an HTTP header name. */
const HEADER_NAME = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

/**
 * Auth rules of a template: `mtls` declares its two secret credential fields; certificate verification can never be
 * turned off; a CSRF prefetch (`auth_params.csrf_fetch_path` / `csrf_header`) is a path relative to the instance
 * base URL (one leading `/`, no whitespace, `?`, `#` or `\`) and a header name.
 */
function validateAuth(t: Pick<TenantConnectorTemplate, 'auth_kind' | 'auth_params' | 'credential_schema'>) {
	const errors: ValidationError[] = [];
	const params = t.auth_params ?? {};
	const text = (k: string) => (typeof params[k] === 'string' ? params[k] : undefined);
	if (t.auth_kind === 'mtls')
		for (const name of MTLS_FIELDS)
			if (!t.credential_schema.some((f) => f.secret && f.name === name))
				errors.push({
					path: 'credential_schema',
					code: 'mtls_credential_missing',
					message: `mtls needs the secret credential field ${name}.`,
				});
	for (const key of TLS_VERIFY_OFF)
		if (text(key) !== undefined)
			errors.push({
				path: `auth_params.${key}`,
				code: 'tls_verification_required',
				message:
					'Certificate verification cannot be turned off. Trust a private CA with the ca_pem credential field instead.',
			});
	const path = text('csrf_fetch_path')?.trim();
	if (path && !(path.startsWith('/') && !path.startsWith('//') && !/[\s?#\\]/.test(path)))
		errors.push({
			path: 'auth_params.csrf_fetch_path',
			code: 'invalid_csrf',
			message:
				'The CSRF fetch path is relative to the instance base URL and starts with a single / (no query, fragment or spaces).',
		});
	const header = text('csrf_header')?.trim();
	if (header && !HEADER_NAME.test(header))
		errors.push({
			path: 'auth_params.csrf_header',
			code: 'invalid_csrf',
			message: 'The CSRF header must be an HTTP header name such as x-csrf-token.',
		});
	return errors;
}

export function idempotencyLabel(i: Idempotency): string {
	switch (i.kind) {
		case 'nat':
			return 'natural (read)';
		case 'header':
			return `header ${i.header}`;
		case 'dup_search':
			return `dup_search on ${i.on}`;
		case 'none':
			return 'none';
	}
}

/** Stable 8-hex fingerprint of a credential value (display only; the backend uses a keyed hash). */
export function fingerprintOf(value: string): string {
	let h = 0x811c9dc5;
	for (let i = 0; i < value.length; i++) {
		h ^= value.charCodeAt(i);
		h = Math.imul(h, 0x01000193) >>> 0;
	}
	return h.toString(16).padStart(8, '0');
}

/** `a91c…` as the UI shows a fingerprint. */
export const shortFp = (fp: string | null) => (fp ? `${fp.slice(0, 4)}…` : '—');
