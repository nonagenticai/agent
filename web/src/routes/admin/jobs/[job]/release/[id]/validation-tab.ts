// Release detail › Validation tab (B8 T12a, F-06): the grid run that a person's approve queues, as the tab says it.
import type { ValidationGridRun } from '$lib/api/types/journey';

/** UI poll interval while a grid run is queued or running (a page poll, not a server schedule). Tests set it to 0. */
export const releaseValidationUi = { pollMs: 5000 };

/** A run that is still going: poll until it is done or failed. */
export const gridActive = (g: ValidationGridRun | null): boolean =>
	g?.status === 'queued' || g?.status === 'running';

/** How the run started: an approve queues one; a person can also run the cut. */
export const TRIGGER_LABEL: Record<string, string> = {
	approve: 'queued by the approve',
	manual: 'run by a person',
};
export const triggerLabel = (t: string): string => TRIGGER_LABEL[t] ?? t.replaceAll('_', ' ');

/** The one-line status: "Queued on approve", "Running · 12 of 40", "Finished · 40 of 40", "Failed (code)". */
export function gridStatusLine(g: ValidationGridRun | null): string {
	if (!g) return 'No validation run yet';
	switch (g.status) {
		case 'queued':
			return g.trigger === 'approve' ? 'Queued on approve' : 'Queued';
		case 'running':
			return `Running · ${g.done} of ${g.total} questions`;
		case 'done':
			return `Finished · ${g.done} of ${g.total} questions`;
		case 'failed':
			return `Failed${g.error_code ? ` (${g.error_code})` : ''} · ${g.done} of ${g.total} questions`;
	}
}

/** Progress 0..1 (0 when the run has no questions). */
export const gridProgress = (g: ValidationGridRun | null): number =>
	g && g.total > 0 ? Math.min(1, g.done / g.total) : g?.status === 'done' ? 1 : 0;
