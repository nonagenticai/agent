// Keyboard bindings for the staff workspace. `map` keys are a lower-case single key (`j`, `t`, `/`) or `Escape` /
// `Mod+Enter` (Ctrl or Cmd + Enter). Single keys are ignored while focus is in an input, textarea, select or
// contenteditable, and when Ctrl/Cmd/Alt is held (browser shortcuts); `Mod+Enter` and `Escape` always fire.

const typing = (t: EventTarget | null): boolean => {
	const el = t as HTMLElement | null;
	if (!el || !el.tagName) return false;
	const tag = el.tagName;
	return (
		tag === 'INPUT' ||
		tag === 'TEXTAREA' ||
		tag === 'SELECT' ||
		el.isContentEditable === true ||
		el.getAttribute?.('contenteditable') === 'true' ||
		el.getAttribute?.('contenteditable') === ''
	);
};

/** Binds `map` on `el`; returns the function that unbinds it. */
export function bindKeys(el: HTMLElement | Window, map: Record<string, () => void>): () => void {
	const onKey = (ev: Event) => {
		const e = ev as KeyboardEvent;
		let id: string | null = null;
		if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) id = 'Mod+Enter';
		else if (e.key === 'Escape') id = 'Escape';
		else if (!e.ctrlKey && !e.metaKey && !e.altKey && !typing(e.target)) id = e.key.toLowerCase();
		const fn = id ? map[id] : undefined;
		if (!fn) return;
		e.preventDefault();
		fn();
	};
	el.addEventListener('keydown', onKey);
	return () => el.removeEventListener('keydown', onKey);
}
