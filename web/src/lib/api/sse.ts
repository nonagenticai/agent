// Server-sent events over a streamed response body. The browser's built-in SSE client cannot send the bearer
// header, so `http-client.ts` reads the body itself and feeds the decoded text here. Pure: no network, no timers.
// Follows the WHATWG event-stream format: lines end in LF, CRLF or CR; a blank line dispatches the event; `data:`
// lines join with LF; `event:` names it (default `message`); `id:` is kept; `retry:` and `:` comments are ignored;
// one space after the colon is dropped.

export interface SseEvent {
	/** The `event:` field, `message` when none was given. */
	event: string;
	/** The `data:` lines joined with `\n`. */
	data: string;
	/** The last `id:` seen (empty when none). */
	id: string;
}

export interface SseParser {
	/** Feed the next decoded chunk (any split: mid-line, mid-CRLF, several events at once). */
	push(chunk: string): void;
	/**
	 * The stream ended: the last line is processed, and an event still waiting for its blank line is dispatched (a
	 * server that closes right after the final `data:` line still delivers it).
	 */
	end(): void;
}

export function createSseParser(onEvent: (e: SseEvent) => void): SseParser {
	let buf = '';
	let event = '';
	let data: string[] = [];
	let hasData = false;
	let lastId = '';
	/** The previous chunk ended in CR: a LF opening the next chunk belongs to that line ending. */
	let skipLf = false;

	function dispatch() {
		if (hasData) onEvent({ event: event || 'message', data: data.join('\n'), id: lastId });
		event = '';
		data = [];
		hasData = false;
	}

	function line(l: string) {
		if (l === '') return dispatch();
		if (l.startsWith(':')) return;
		const colon = l.indexOf(':');
		const field = colon === -1 ? l : l.slice(0, colon);
		let value = colon === -1 ? '' : l.slice(colon + 1);
		if (value.startsWith(' ')) value = value.slice(1);
		if (field === 'event') event = value;
		else if (field === 'data') {
			data.push(value);
			hasData = true;
		} else if (field === 'id' && !value.includes('\0')) lastId = value;
	}

	return {
		push(chunk) {
			if (!chunk) return;
			if (skipLf && chunk.startsWith('\n')) chunk = chunk.slice(1);
			skipLf = false;
			buf += chunk;
			let start = 0;
			for (let i = 0; i < buf.length; i++) {
				const c = buf[i];
				if (c !== '\n' && c !== '\r') continue;
				line(buf.slice(start, i));
				if (c === '\r') {
					if (i + 1 < buf.length) {
						if (buf[i + 1] === '\n') i++;
					} else skipLf = true;
				}
				start = i + 1;
			}
			buf = buf.slice(start);
		},
		end() {
			if (buf) line(buf);
			buf = '';
			dispatch();
		},
	};
}
