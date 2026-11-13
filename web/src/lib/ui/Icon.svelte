<script lang="ts" module>
	// Path data from design/admin/ui.js `P` (plus), and demo, console and chat (not in ui.js).
	export const PATHS: Record<string, string> = {
		dashboard: 'M4 4h7v7H4zM13 4h7v4h-7zM13 10h7v10h-7zM4 13h7v7H4z',
		analytics: 'M4 20V10M10 20V4M16 20v-7M22 20H2',
		agents: 'M8 8h8v8H8zM12 2v4M12 18v4M2 12h4M18 12h4',
		buses: 'M3 6h18M3 12h18M3 18h18M7 4v4M15 10v4M11 16v4',
		cases: 'M4 4h16v12H4zM4 16l3 4h10l3-4M9 10h6',
		ontology: 'M12 5a2 2 0 1 0 0-.01M5 19a2 2 0 1 0 0-.01M19 19a2 2 0 1 0 0-.01M12 7v4M12 11l-6 6M12 11l6 6',
		procedures: 'M5 4h5v5H5zM14 15h5v5h-5zM7.5 9v4.5h9V15',
		knowledge: 'M4 5a2 2 0 0 1 2-2h13v16H6a2 2 0 0 0-2 2zM4 21V5M8 7h7',
		stores: 'M12 3 3 7.5l9 4.5 9-4.5zM3 12l9 4.5 9-4.5M3 16.5 12 21l9-4.5',
		jobs: 'M4 5h16v4H4zM4 11h10v4H4zM4 17h6v4H4z',
		tools: 'M14.7 6.3a4 4 0 0 0-5.4 5.4L3 18l3 3 6.3-6.3a4 4 0 0 0 5.4-5.4l-2.5 2.5-2.4-.6-.6-2.4z',
		backends:
			'M4 6c0-1.7 3.6-3 8-3s8 1.3 8 3-3.6 3-8 3-8-1.3-8-3zM4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3',
		models: 'M12 3l2.4 5.6L20 11l-5.6 2.4L12 19l-2.4-5.6L4 11l5.6-2.4z',
		audit: 'M9 4h6l1 2h3v15H5V6h3zM9 12l2 2 4-4',
		demo: 'M4 4h16v16H4zM4 9h16M9 9v11',
		console: 'M4 5h16v11H8l-4 4z',
		chat: 'M3 6h12v8H7l-4 3zM15 9h6v8l-3-2.5h-6V14',
		search: 'M11 4a7 7 0 1 0 0 14 7 7 0 0 0 0-14zM21 21l-4.3-4.3',
		sun: 'M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8zM12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4',
		moon: 'M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z',
		menu: 'M3 6h18M3 12h18M3 18h18',
		x: 'M6 6l12 12M18 6L6 18',
		paperclip: 'M20 11l-8.5 8.5a5 5 0 0 1-7-7L13 4a3.5 3.5 0 0 1 5 5l-8.5 8.5a2 2 0 0 1-3-3L14 7',
		file: 'M6 3h8l5 5v13H6zM14 3v5h5M9 13h6M9 17h6',
		download: 'M12 4v11M7 11l5 5 5-5M5 20h14',
		plus: 'M12 5v14M5 12h14',
		arrow: 'M5 12h14M13 6l6 6-6 6',
		info: 'M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM12 11v5M12 7.5v.5',
		warn: 'M12 3 2 20h20zM12 10v4M12 17v.5',
		err: 'M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM9 9l6 6M15 9l-6 6',
		// Added for the Agent concept UI (platform, tenant and M4 pages).
		layers: 'M12 3 3 8l9 5 9-5zM3 13l9 5 9-5',
		tenant: 'M4 21V7l8-4 8 4v14M9 21v-6h6v6M8 10h.01M12 10h.01M16 10h.01',
		users:
			'M9 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM2 21v-1a7 7 0 0 1 14 0v1M17 11a3 3 0 1 0 0-6M22 21v-1a5 5 0 0 0-4-4.9',
		signup: 'M15 19v-1a5 5 0 0 0-10 0v1M10 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM19 8v6M16 11h6',
		key: 'M7 14a4 4 0 1 0 0-.01M10 11l10-10M16 5l3 3M14 7l2 2',
		compile: 'M8 6l-5 6 5 6M16 6l5 6-5 6M14 4l-4 16',
		axes: 'M4 20h16M4 20V4M4 14l5-4 4 3 7-7',
		template: 'M4 4h16v5H4zM4 12h7v8H4zM14 12h6v8h-6z',
		journey: 'M5 5a2 2 0 1 0 0-.01M19 19a2 2 0 1 0 0-.01M7 5h7a3 3 0 0 1 0 6h-4a3 3 0 0 0 0 6h7',
		object: 'M12 3l8 4.5v9L12 21l-8-4.5v-9zM12 12l8-4.5M12 12v9M12 12L4 7.5',
		operation: 'M13 3 5 14h6l-1 7 8-11h-6z',
		gate: 'M4 21V5l8-2 8 2v16M4 11h16M9 21v-6h6v6',
		generator:
			'M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M5.6 18.4l2.1-2.1M16.3 7.7l2.1-2.1M12 9a3 3 0 1 0 0 6 3 3 0 0 0 0-6z',
		check: 'M4 12l5 5L20 6',
		rule: 'M5 4h14v16H5zM9 9h6M9 13h6M9 17h3',
		lock: 'M6 11h12v9H6zM8.5 11V8a3.5 3.5 0 0 1 7 0v3',
		bot: 'M5 8h14v10H5zM12 3v5M9 13h.01M15 13h.01M9 16h6M2 12v3M22 12v3',
		release: 'M12 3v12M7 10l5 5 5-5M5 21h14',
	};
</script>

<script lang="ts">
	let { name, size = 18 }: { name: string; size?: number } = $props();
</script>

<svg
	class="ic"
	width={size}
	height={size}
	viewBox="0 0 24 24"
	fill="none"
	stroke="currentColor"
	stroke-width="1.7"
	stroke-linecap="round"
	stroke-linejoin="round"
	aria-hidden="true"><path d={PATHS[name] ?? PATHS.dashboard} /></svg
>
