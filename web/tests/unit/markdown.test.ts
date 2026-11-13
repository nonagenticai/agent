// The transcript markdown subset ($lib/markdown + Markdown.svelte): a node tree rendered without {@html}, so HTML in
// a message stays text and only https links become anchors.
import { cleanup, render } from '@testing-library/svelte';
import { afterEach, describe, expect, it } from 'vitest';
import { parseInline, parseMarkdown, safeHref } from '../../src/lib/markdown';
import Markdown from '../../src/lib/ui/Markdown.svelte';

afterEach(() => cleanup());

describe('markdown subset', () => {
	it('parses bold, italic, code and links into nodes', () => {
		expect(parseInline('**Order** ships *soon*, run `track`')).toEqual([
			{ t: 'strong', c: [{ t: 'text', v: 'Order' }] },
			{ t: 'text', v: ' ships ' },
			{ t: 'em', c: [{ t: 'text', v: 'soon' }] },
			{ t: 'text', v: ', run ' },
			{ t: 'code', v: 'track' },
		]);
		expect(parseInline('see https://x.example/a.')).toEqual([
			{ t: 'text', v: 'see ' },
			{ t: 'link', href: 'https://x.example/a', c: [{ t: 'text', v: 'https://x.example/a' }] },
			{ t: 'text', v: '.' },
		]);
		expect(parseInline('[a](javascript:alert(1)) [b](http://x.example)')).toEqual([
			{ t: 'text', v: '[a](javascript:alert(1)) [b](http://x.example)' },
		]);
		expect(safeHref('data:text/html,x')).toBeNull();
	});

	it('groups paragraphs, line breaks and lists', () => {
		const b = parseMarkdown('Hi\nthere\n\n- one\n* two\n\n3. three\n4. four\n## Next');
		expect(b.map((x) => x.t)).toEqual(['p', 'ul', 'ol', 'p']);
		expect(b[0]).toMatchObject({ t: 'p', lines: [[{ v: 'Hi' }], [{ v: 'there' }]] });
		expect(b[2]).toMatchObject({ t: 'ol', start: 3 });
		expect(b[3]).toEqual({ t: 'p', lines: [[{ t: 'strong', c: [{ t: 'text', v: 'Next' }] }]] });
	});

	it('markdown_renders_html_as_text', () => {
		const { container } = render(Markdown, {
			text: '**Bold** <script>window.__md=1</script>\n- [Track](https://shop.example.com/t)\n- <img src=x onerror="window.__md=1">',
		});
		expect(container.querySelector('strong')?.textContent).toBe('Bold');
		expect(container.querySelector('script, img')).toBeNull();
		expect(container.textContent).toContain('<script>window.__md=1</script>');
		expect(container.textContent).toContain('<img src=x onerror="window.__md=1">');
		const a = container.querySelector('a')!;
		expect(a.getAttribute('href')).toBe('https://shop.example.com/t');
		expect(a.getAttribute('rel')).toBe('noopener noreferrer');
		expect(a.getAttribute('target')).toBe('_blank');
		expect((window as unknown as { __md?: number }).__md).toBeUndefined();
	});
});
