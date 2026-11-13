// A safe, minimal markdown subset for chat transcripts (model answers carry `**bold**`, lists, links). The text is
// parsed into a plain node tree that Markdown.svelte renders with ordinary elements and text interpolation — never
// {@html} — so a `<script>` in a message stays text. Subset: paragraphs and line breaks, `**bold**`, `*italic*`,
// `` `code` ``, `-`/`*`/`•` and `1.` lists, `# heading` lines (shown bold), `[label](https://…)` and bare https
// links. Any other scheme (http:, javascript:, data:) stays text.

export type MdInline =
	| { t: 'text'; v: string }
	| { t: 'strong'; c: MdInline[] }
	| { t: 'em'; c: MdInline[] }
	| { t: 'code'; v: string }
	| { t: 'link'; href: string; c: MdInline[] };

/** A paragraph's lines are joined with line breaks; a list item is one line. */
export type MdBlock =
	| { t: 'p'; lines: MdInline[][] }
	| { t: 'ul'; items: MdInline[][] }
	| { t: 'ol'; start: number; items: MdInline[][] };

const INLINE =
	/\*\*([^\s*](?:[^*]*[^\s*])?)\*\*|`([^`\n]+)`|\[([^\]\n]+)\]\((https:\/\/[^\s()]+)\)|(https:\/\/[^\s<>()]*[^\s<>().,;:!?'"])|\*([^\s*](?:[^*]*[^\s*])?)\*/g;

/** The URL when it is a well-formed https URL, else null. */
export function safeHref(raw: string): string | null {
	try {
		const u = new URL(raw);
		return u.protocol === 'https:' ? u.href : null;
	} catch {
		return null;
	}
}

export function parseInline(s: string): MdInline[] {
	const out: MdInline[] = [];
	const text = (v: string) => {
		if (!v) return;
		const last = out[out.length - 1];
		if (last?.t === 'text') last.v += v;
		else out.push({ t: 'text', v });
	};
	let at = 0;
	for (const m of s.matchAll(INLINE)) {
		const i = m.index ?? 0;
		text(s.slice(at, i));
		at = i + m[0].length;
		if (m[1] !== undefined) out.push({ t: 'strong', c: parseInline(m[1]) });
		else if (m[2] !== undefined) out.push({ t: 'code', v: m[2] });
		else if (m[3] !== undefined) {
			const href = safeHref(m[4]!);
			if (href) out.push({ t: 'link', href, c: parseInline(m[3]) });
			else text(m[0]);
		} else if (m[5] !== undefined) {
			const href = safeHref(m[5]);
			if (href) out.push({ t: 'link', href, c: [{ t: 'text', v: m[5] }] });
			else text(m[0]);
		} else if (m[6] !== undefined) out.push({ t: 'em', c: parseInline(m[6]) });
	}
	text(s.slice(at));
	return out;
}

const UL = /^\s*[-*•]\s+(.*)$/;
const OL = /^\s*(\d{1,4})[.)]\s+(.*)$/;
const HEADING = /^\s*#{1,6}\s+(.*)$/;

export function parseMarkdown(src: string): MdBlock[] {
	const blocks: MdBlock[] = [];
	let cur: MdBlock | null = null;
	for (const line of src.replace(/\r\n?/g, '\n').split('\n')) {
		if (!line.trim()) {
			cur = null;
			continue;
		}
		const ul = UL.exec(line);
		const ol = ul ? null : OL.exec(line);
		if (ul) {
			if (cur?.t !== 'ul') blocks.push((cur = { t: 'ul', items: [] }));
			cur.items.push(parseInline(ul[1]!));
		} else if (ol) {
			if (cur?.t !== 'ol') blocks.push((cur = { t: 'ol', start: Number(ol[1]), items: [] }));
			cur.items.push(parseInline(ol[2]!));
		} else {
			const h = HEADING.exec(line);
			const inl: MdInline[] = h ? [{ t: 'strong', c: parseInline(h[1]!) }] : parseInline(line);
			if (cur?.t !== 'p') blocks.push((cur = { t: 'p', lines: [] }));
			cur.lines.push(inl);
		}
	}
	return blocks;
}
