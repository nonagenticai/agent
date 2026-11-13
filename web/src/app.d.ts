// See https://svelte.dev/docs/kit/types#app.d.ts
declare global {
	namespace App {
		interface PageState {
			/** The staff workspace's customer sheet is open on a phone (a history entry, so Back closes it). */
			customer?: boolean;
		}
	}
}

export {};
