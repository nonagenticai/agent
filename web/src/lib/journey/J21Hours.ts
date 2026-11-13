// F-14 (B4 T16a): business hours (the `hours` of a job's handover policy) and the tenant holiday calendar — pure
// checks shared by the fixture API and the pages. Each mirrors agent-api (`handover::hours::validate`,
// `handover::calendar::validate`) message for message, so the page refuses what the API would refuse (422
// `invalid_policy` with `details.field = "hours"` / 422 `invalid_calendar`) before a save is sent.
import { WEEKDAYS, type HandoverHours, type HoursWindow, type Weekday } from '$lib/api/types/journey';
import type { TenantCalendar, TenantCalendarInput, TenantHoliday } from '$lib/api/types/tenant';

/** The most windows one day holds (`hours::MAX_WINDOWS`). */
export const MAX_WINDOWS = 6;
/** The most holidays one calendar holds (`calendar::MAX_HOLIDAYS`). */
export const MAX_HOLIDAYS = 366;
/** The longest holiday label (`calendar::MAX_LABEL`). */
export const MAX_LABEL = 80;
/** The longest timezone name (`calendar::MAX_TIMEZONE`). */
export const MAX_TIMEZONE = 64;

export const WEEKDAY_LABEL: Record<Weekday, string> = {
	mon: 'Monday',
	tue: 'Tuesday',
	wed: 'Wednesday',
	thu: 'Thursday',
	fri: 'Friday',
	sat: 'Saturday',
	sun: 'Sunday',
};

export const OUT_OF_HOURS_LABEL: Record<HandoverHours['out_of_hours'], string> = {
	callback: 'Offer a call-back after opening',
	queue: 'Reply after opening',
};

/** `HH:MM` from 00:00 to 24:00 as minutes from midnight, else null (`hours::parse_hhmm`). */
export function minutesOf(s: string): number | null {
	if (!/^\d\d:\d\d$/.test(s)) return null;
	const h = Number(s.slice(0, 2));
	const m = Number(s.slice(3));
	const total = h * 60 + m;
	return m < 60 && total <= 1440 ? total : null;
}

/** A timezone name the browser knows (the API checks it against `pg_timezone_names`). */
export function knownTimezone(tz: string): boolean {
	try {
		new Intl.DateTimeFormat('en', { timeZone: tz });
		return true;
	} catch {
		return false;
	}
}

/** Office hours Monday to Friday, 09:00–17:00, replies after opening. */
export function defaultHours(timezone = 'Europe/Amsterdam'): HandoverHours {
	const day = (): HoursWindow[] => [{ from: '09:00', to: '17:00' }];
	return {
		timezone,
		weekly: { mon: day(), tue: day(), wed: day(), thu: day(), fri: day(), sat: [], sun: [] },
		use_tenant_calendar: true,
		out_of_hours: 'queue',
	};
}

/**
 * Why `h` cannot be saved as a policy's hours (each message prefixed `hours:`, as the API's), else null. The
 * timezone's existence is checked separately ({@link knownTimezone}), as the API does it against the database.
 */
export function hoursError(h: HandoverHours): string | null {
	const tz = (h.timezone ?? '').trim();
	if (!tz || [...tz].length > MAX_TIMEZONE) return 'hours: timezone is required';
	const weekly = (h.weekly ?? {}) as Record<string, HoursWindow[] | undefined>;
	const unknown = Object.keys(weekly).find((k) => !(WEEKDAYS as readonly string[]).includes(k));
	if (unknown) return `hours: unknown day ${unknown}`;
	let open = 0;
	for (const d of WEEKDAYS) {
		const ws = weekly[d] ?? [];
		if (ws.length > MAX_WINDOWS) return `hours: ${d} has more than ${MAX_WINDOWS} windows`;
		const mins: [number, number][] = [];
		for (const w of ws) {
			const from = minutesOf(w.from);
			const to = minutesOf(w.to);
			if (from === null || to === null || from >= to || from >= 1440)
				return `hours: ${d} windows need from before to, HH:MM between 00:00 and 24:00`;
			mins.push([from, to]);
		}
		mins.sort((a, b) => a[0] - b[0]);
		for (let i = 1; i < mins.length; i++)
			if (mins[i]![0] < mins[i - 1]![1]) return `hours: ${d} windows overlap`;
		open += mins.length;
	}
	if (!open) return 'hours: no open window in the week';
	if (h.out_of_hours !== 'callback' && h.out_of_hours !== 'queue')
		return 'hours: out_of_hours must be callback or queue';
	return null;
}

/** The hours as the API stores them: trimmed timezone, every day present, each day's windows sorted. */
export function normalizeHours(h: HandoverHours): HandoverHours {
	const weekly = {} as Record<Weekday, HoursWindow[]>;
	for (const d of WEEKDAYS)
		weekly[d] = [...(h.weekly?.[d] ?? [])]
			.map((w) => ({ from: w.from, to: w.to }))
			.sort((a, b) => (minutesOf(a.from) ?? 0) - (minutesOf(b.from) ?? 0));
	return {
		timezone: h.timezone.trim(),
		weekly,
		use_tenant_calendar: !!h.use_tenant_calendar,
		out_of_hours: h.out_of_hours,
	};
}

/** One line: `Europe/Amsterdam · Mon–Fri 09:00–17:00 · call-back`, or `always staffed`. */
export function hoursText(h: HandoverHours | null | undefined): string {
	if (!h) return 'always staffed';
	const days = WEEKDAYS.filter((d) => (h.weekly[d] ?? []).length);
	const ws = (d: Weekday) => h.weekly[d].map((w) => `${w.from}–${w.to}`).join(', ');
	const same = days.length > 0 && days.every((d) => ws(d) === ws(days[0]!));
	const label = (d: Weekday) => WEEKDAY_LABEL[d].slice(0, 3);
	const run =
		same && days.length > 1 && WEEKDAYS.indexOf(days.at(-1)!) - WEEKDAYS.indexOf(days[0]!) === days.length - 1
			? `${label(days[0]!)}–${label(days.at(-1)!)} ${ws(days[0]!)}`
			: days.map((d) => `${label(d)} ${ws(d)}`).join(' · ');
	return [
		h.timezone,
		run || 'closed all week',
		h.out_of_hours === 'callback' ? 'call-back' : 'reply after opening',
	]
		.concat(h.use_tenant_calendar ? ['tenant holidays'] : [])
		.join(' · ');
}

// ---- tenant calendar ----------------------------------------------------------------------------------------------

/** A `YYYY-MM-DD` calendar date (exactly that form, a real day). */
export function validDate(s: string): boolean {
	if (!/^\d{4}-\d{2}-\d{2}$/.test(s)) return false;
	const d = new Date(`${s}T00:00:00Z`);
	return !Number.isNaN(d.getTime()) && d.toISOString().slice(0, 10) === s;
}

/** Why the calendar cannot be saved (the API's `invalid_calendar` messages), else null. */
export function calendarError(c: TenantCalendarInput): string | null {
	const tz = (c.timezone ?? '').trim();
	if (!tz || [...tz].length > MAX_TIMEZONE) return 'timezone is required';
	const rows = c.holidays ?? [];
	if (rows.length > MAX_HOLIDAYS) return `at most ${MAX_HOLIDAYS} holidays`;
	const seen = new Set<string>();
	for (const [i, h] of rows.entries()) {
		const n = i + 1;
		if (!validDate(h.date)) return `holiday ${n}: date must be YYYY-MM-DD`;
		if ([...(h.label ?? '').trim()].length > MAX_LABEL)
			return `holiday ${n}: label is longer than ${MAX_LABEL}`;
		if (h.closed !== undefined && h.closed !== true) return `holiday ${n}: closed must be true`;
	}
	for (const d of rows.map((h) => h.date).sort()) {
		if (seen.has(d)) return `${d} is listed twice`;
		seen.add(d);
	}
	return null;
}

/** The calendar as the API stores it: trimmed, sorted by date, every holiday `closed: true`. */
export function normalizeCalendar(c: TenantCalendarInput): TenantCalendar {
	const holidays: TenantHoliday[] = (c.holidays ?? [])
		.map((h) => ({ date: h.date, label: (h.label ?? '').trim(), closed: true as const }))
		.sort((a, b) => a.date.localeCompare(b.date));
	return { timezone: c.timezone.trim(), holidays };
}

/** One line: `Europe/Amsterdam · 3 holidays`, or `no calendar` (`calendar::calendar_text`). */
export function calendarText(c: TenantCalendar | null | undefined): string {
	if (!c) return 'no calendar';
	const n = c.holidays.length;
	return `${c.timezone} · ${n} holiday${n === 1 ? '' : 's'}`;
}
