// Regenerates the B8 acceptance fixtures next to this file: `node tests/e2e/fixtures/b8/make-fixtures.mjs`.
// Deterministic (no randomness), so a diff means the generator changed. No fixture message contains an order
// number, customer id, postcode, email or tracking number (owner KYC ruling 2026-10-26): customers never type ids.
import { crc32, deflateRawSync } from 'node:zlib';
import { writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const csvCell = (s) => (/[",\n]/.test(s) ? `"${s.replaceAll('"', '""')}"` : s);
const csv = (rows) => rows.map((r) => r.map(csvCell).join(',')).join('\n') + '\n';

// ---- 200 tickets: 160 the retail job covers (track / return / cancel), 40 it does not (garden and lawn) ----------
const openers = ['Hi', 'Hello', 'Good morning', 'Hey there', 'Dear shop'];
const asks = {
	track: [
		'where is my parcel?',
		'my delivery is late, when will it arrive?',
		'can you tell me where my package is right now?',
		'the courier has not shown up yet, what is going on with my delivery?',
	],
	return: [
		'I want to send this back, how do I return it?',
		'the item does not fit, can I get a refund?',
		'I would like to return the jacket I received last week.',
		'how do I return something that arrived damaged?',
	],
	cancel: [
		'please cancel my last order, I changed my mind.',
		'can I still cancel what I ordered this morning?',
		'I ordered the wrong size, cancel it please.',
	],
	lawn: [
		'do you sell lawn sprinklers for a small garden?',
		'which garden hose works best with your sprinkler set?',
		'I need advice on lawn care for the summer.',
		'is the garden furniture waterproof?',
	],
};
const plan = [
	['track', 70],
	['return', 60],
	['cancel', 30],
	['lawn', 40],
];
const channels = ['email', 'chat', 'web_form'];
const tickets = [['ticket_ref', 'created_at', 'channel', 'subject', 'message']];
let n = 0;
for (const [kind, count] of plan) {
	for (let i = 0; i < count; i++) {
		const text = `${openers[i % openers.length]}, ${asks[kind][i % asks[kind].length]}${i >= asks[kind].length * openers.length ? ` Thanks, ${'ab'.repeat(1 + (i % 3))}.` : ''}`;
		n++;
		tickets.push([
			`T-${String(n).padStart(4, '0')}`,
			`2026-10-${String(1 + (n % 25)).padStart(2, '0')}T09:${String(n % 60).padStart(2, '0')}:00Z`,
			channels[n % 3],
			kind === 'lawn' ? 'Garden question' : 'Order question',
			text,
		]);
	}
}
// Make every message unique (the importer dedupes by text) without adding anything an agent could ask for.
const seen = new Set();
for (const row of tickets.slice(1)) {
	let t = row[4];
	let k = 2;
	while (seen.has(t)) t = `${row[4].replace(/[.?]$/, '')} (message ${k++})`;
	seen.add(t);
	row[4] = t;
}
writeFileSync(join(here, 'tickets-200.csv'), csv(tickets));

// ---- validation questions: 20 and 501 rows (501 is one over the limit and must be refused with 413) ------------------
const qrows = (count) => [
	['question', 'expected', 'reference', 'activity'],
	...Array.from({ length: count }, (_, i) => [
		`Acceptance question ${i + 1}: how long do I have to return an item?`,
		'Items can be returned within 30 days of delivery.',
		'policy.returns §1',
		'support:return_item',
	]),
];
writeFileSync(join(here, 'questions-20.csv'), csv(qrows(20)));
writeFileSync(join(here, 'questions-501.csv'), csv(qrows(501)));

// ---- the same tickets as XLSX with a chosen text column (`message`; `subject` is the decoy) -------------------------
function xlsx(rows) {
	const strings = [];
	const idx = new Map();
	const sid = (s) => {
		if (!idx.has(s)) idx.set(s, strings.push(s) - 1);
		return idx.get(s);
	};
	const col = (i) => String.fromCharCode(65 + i);
	const esc = (s) => s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
	const sheet = rows
		.map(
			(r, ri) =>
				`<row r="${ri + 1}">${r.map((c, ci) => `<c r="${col(ci)}${ri + 1}" t="s"><v>${sid(c)}</v></c>`).join('')}</row>`,
		)
		.join('');
	const files = {
		'[Content_Types].xml':
			'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/></Types>',
		'_rels/.rels':
			'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>',
		'xl/workbook.xml':
			'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Tickets" sheetId="1" r:id="rId1"/></sheets></workbook>',
		'xl/_rels/workbook.xml.rels':
			'<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/></Relationships>',
		'xl/worksheets/sheet1.xml': `<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>${sheet}</sheetData></worksheet>`,
		'xl/sharedStrings.xml': `<?xml version="1.0" encoding="UTF-8" standalone="yes"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="${strings.length}" uniqueCount="${strings.length}">${strings.map((s) => `<si><t>${esc(s)}</t></si>`).join('')}</sst>`,
	};
	// A minimal zip writer (deflate, fixed timestamp) so the fixture needs no dependency.
	const local = [];
	const central = [];
	let offset = 0;
	for (const [name, text] of Object.entries(files)) {
		const nameB = Buffer.from(name);
		const raw = Buffer.from(text);
		const data = deflateRawSync(raw);
		const crc = crc32(raw);
		const lh = Buffer.alloc(30);
		lh.writeUInt32LE(0x04034b50, 0);
		lh.writeUInt16LE(20, 4);
		lh.writeUInt16LE(0, 6);
		lh.writeUInt16LE(8, 8);
		lh.writeUInt16LE(0, 10);
		lh.writeUInt16LE(0x21, 12);
		lh.writeUInt32LE(crc, 14);
		lh.writeUInt32LE(data.length, 18);
		lh.writeUInt32LE(raw.length, 22);
		lh.writeUInt16LE(nameB.length, 26);
		local.push(lh, nameB, data);
		const ch = Buffer.alloc(46);
		ch.writeUInt32LE(0x02014b50, 0);
		ch.writeUInt16LE(20, 4);
		ch.writeUInt16LE(20, 6);
		ch.writeUInt16LE(8, 10);
		ch.writeUInt16LE(0x21, 14);
		ch.writeUInt32LE(crc, 16);
		ch.writeUInt32LE(data.length, 20);
		ch.writeUInt32LE(raw.length, 24);
		ch.writeUInt16LE(nameB.length, 28);
		ch.writeUInt32LE(offset, 42);
		central.push(ch, nameB);
		offset += lh.length + nameB.length + data.length;
	}
	const cd = Buffer.concat(central);
	const end = Buffer.alloc(22);
	end.writeUInt32LE(0x06054b50, 0);
	end.writeUInt16LE(Object.keys(files).length, 8);
	end.writeUInt16LE(Object.keys(files).length, 10);
	end.writeUInt32LE(cd.length, 12);
	end.writeUInt32LE(offset, 16);
	return Buffer.concat([...local, cd, end]);
}
writeFileSync(join(here, 'tickets-40.xlsx'), xlsx(tickets.slice(0, 41)));
console.log('fixtures written to', here);
