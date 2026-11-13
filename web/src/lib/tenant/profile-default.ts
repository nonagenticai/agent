// D132.1: which business profile a new job (or a selection with no profile yet) starts on. The default is the only
// profile whose industry is the job's industry, else the only industry-less profile; none or several = ambiguous
// (the server answers 422 `profile_required`), so the user must choose.
export interface ProfileOption {
	key: string;
	label: string;
	industry?: string | null;
}

/** The profiles that fit an industry: its own, else the industry-less ones. */
export function fittingProfiles<T extends ProfileOption>(profiles: T[], industry: string): T[] {
	const own = profiles.filter((p) => p.industry === industry);
	return own.length ? own : profiles.filter((p) => !p.industry);
}

/** The D132.1 default profile key, or null when there is none or several (the user chooses). */
export function defaultProfile(
	profiles: Pick<ProfileOption, 'key' | 'industry'>[],
	industry: string,
): string | null {
	const own = profiles.filter((p) => p.industry === industry);
	if (own.length === 1) return own[0]!.key;
	if (own.length > 1) return null;
	const free = profiles.filter((p) => !p.industry);
	return free.length === 1 ? free[0]!.key : null;
}

/** Profiles offered for an industry: those of the industry plus the industry-less ones. */
export function profileChoices<T extends Pick<ProfileOption, 'industry'>>(
	profiles: T[],
	industry: string,
): T[] {
	return profiles.filter((p) => !p.industry || p.industry === industry);
}
