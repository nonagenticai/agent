// Per-channel message length (staff workspace). Each value mirrors the server constant named next to it.
/** crates/agent-api/src/conversations/store.rs:25 `MAX_TEXT` */
export const MAX_TEXT = 8000;

const LIMITS: Record<string, number> = {
	/** crates/agent-api/src/channels/telegram.rs:57 `MAX_TEXT_CHARS` */
	telegram: 4000,
};

/** The longest message (in characters) staff can send on `channel`. */
export function limitFor(channel: string): number {
	return LIMITS[channel] ?? MAX_TEXT;
}
