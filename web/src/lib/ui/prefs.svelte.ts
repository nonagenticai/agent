// Per-viewer display preferences (localStorage, try/catch: storage may be blocked).
// Build milestones are OFF by default: the app is a design of the product, so no build wording shows unless a
// member of the build team turns on "Show build milestones" (stored per browser).
// Technical details (slugs, ids, hashes, error codes) are likewise OFF by default ("Show technical details").
const KEY = 'agent-show-milestones';
const TECH_KEY = 'agent-show-tech';

function read(key: string): boolean {
	try {
		return localStorage.getItem(key) === 'on';
	} catch {
		return false;
	}
}

function write(key: string, on: boolean) {
	try {
		localStorage.setItem(key, on ? 'on' : 'off');
	} catch {
		/* storage blocked: lasts for this page only */
	}
}

export const prefs = $state<{ showMilestones: boolean; showTech: boolean }>({
	showMilestones: false,
	showTech: false,
});

/** Reads the stored preferences (call once in the browser, from the layout). */
export function loadPrefs() {
	prefs.showMilestones = read(KEY);
	prefs.showTech = read(TECH_KEY);
}

export function setShowMilestones(on: boolean) {
	prefs.showMilestones = on;
	write(KEY, on);
}

export function setShowTech(on: boolean) {
	prefs.showTech = on;
	write(TECH_KEY, on);
}
