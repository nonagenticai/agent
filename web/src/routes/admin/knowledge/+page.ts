import { redirect } from '@sveltejs/kit';

// There is no index page: go to the first real one.
export function load(): never {
	redirect(307, '/admin/knowledge/stores');
}
