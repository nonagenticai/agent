import adapter from '@sveltejs/adapter-node';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';

export default {
	preprocess: vitePreprocess(),
	kit: {
		adapter: adapter({ out: 'build' }),
		// svelte-check type-checks web/scripts too.
		typescript: {
			config: (config) => {
				config.include.push('../scripts/**/*.ts');
			},
		},
	},
};
