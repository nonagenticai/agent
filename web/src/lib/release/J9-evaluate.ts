// J9 — the built-in `rule.evaluate` (K49) for the fixture rules, as a pure function: facts + the tenant's values →
// outcome. Used by the fixture `evaluateRule` (dry run) and by the J9 change preview. A rule with an unset value
// cannot be evaluated: the agent escalates instead (10 §5.2 step 7, "placeholder").
import type { RuleEvaluation } from '$lib/api/types/release';

type Values = Record<string, string | null>;
type Facts = Record<string, string>;

const DAY = 86_400_000;

function daysBetween(fromIso: string, toIso: string): number | null {
	const a = Date.parse(fromIso.slice(0, 10));
	const b = Date.parse(toIso.slice(0, 10));
	if (Number.isNaN(a) || Number.isNaN(b)) return null;
	return Math.floor((b - a) / DAY);
}

function monthsBetween(fromIso: string, toIso: string): number | null {
	const a = new Date(fromIso.slice(0, 10));
	const b = new Date(toIso.slice(0, 10));
	if (Number.isNaN(a.getTime()) || Number.isNaN(b.getTime())) return null;
	let m = (b.getUTCFullYear() - a.getUTCFullYear()) * 12 + (b.getUTCMonth() - a.getUTCMonth());
	if (b.getUTCDate() < a.getUTCDate()) m -= 1;
	return m;
}

const list = (v: string | null | undefined) =>
	(v ?? '')
		.split(',')
		.map((x) => x.trim().toLowerCase())
		.filter(Boolean);

/** Evaluate rule `id` with `facts` against `values` on day `today` (YYYY-MM-DD…). */
export function evaluateJ9(id: string, facts: Facts, values: Values, today: string): RuleEvaluation {
	const base = { rule: id, values_used: { ...values } };
	const unset = Object.entries(values)
		.filter(([, v]) => v === null || v.trim() === '')
		.map(([k]) => k);
	if (unset.length)
		return {
			...base,
			outcome: 'placeholder',
			code: 'placeholder',
			reason: `${id} has no value for ${unset.join(', ')}: the agent cannot decide and hands the case to a person.`,
			trace: [`value ${unset.join(', ')} = not set → cannot evaluate`],
		};
	const no = (reason: string, trace: string[]): RuleEvaluation => ({
		...base,
		outcome: 'ineligible',
		code: `ineligible:${id}`,
		reason,
		trace,
	});
	const yes = (reason: string, trace: string[]): RuleEvaluation => ({
		...base,
		outcome: 'eligible',
		code: 'eligible',
		reason,
		trace,
	});
	const escalate = (reason: string, trace: string[]): RuleEvaluation => ({
		...base,
		outcome: 'escalate',
		code: 'escalate',
		reason,
		trace,
	});
	const missing = (k: string) => escalate(`Fact ${k} is missing: the agent reads it first.`, [`${k} = ?`]);

	switch (id) {
		case 'RET-01': {
			const delivered = facts['order.delivered_at'];
			if (!delivered) return missing('order.delivered_at');
			const window = Number(values.window_days);
			const days = daysBetween(delivered, today);
			if (days === null) return missing('order.delivered_at');
			const category = (facts['offering.category'] ?? '').trim();
			const excluded = list(values.excluded_categories);
			const trace = [`category “${category || '—'}” ∉ excluded [${excluded.join(', ')}]`];
			if (category && excluded.includes(category.toLowerCase()))
				return no(`${category} cannot be returned.`, [
					`category “${category}” ∈ excluded [${excluded.join(', ')}]`,
				]);
			trace.push(`${days} days since delivery ≤ ${window}`);
			if (days > window)
				return no(`The return window closed ${days - window} days ago (day ${days} of ${window}).`, [
					trace[0]!,
					`${days} days since delivery > ${window}`,
				]);
			return yes(`Day ${days} of ${window}: the return can be opened.`, trace);
		}
		case 'WAR-01': {
			const delivered = facts['order.delivered_at'];
			if (!delivered) return missing('order.delivered_at');
			const period = Number(values.period_months);
			const months = monthsBetween(delivered, today);
			if (months === null) return missing('order.delivered_at');
			if (months >= period)
				return no(`Out of warranty: delivered ${months} months ago, the guarantee is ${period} months.`, [
					`${months} months since delivery ≥ ${period}`,
				]);
			return yes(`Month ${months + 1} of ${period}: a warranty claim can be opened.`, [
				`${months} months since delivery < ${period}`,
			]);
		}
		case 'PAY-04': {
			const status = (facts['payment.status'] ?? '').trim();
			if (!status) return missing('payment.status');
			if (status !== 'pending')
				return yes(`The charge is ${status}, not pending: PAY-04 does not block a refund.`, [
					`payment.status = ${status} ≠ pending`,
				]);
			const at = facts['payment.at'];
			const age = at ? daysBetween(at, today) : null;
			const limit = Number(values.drop_off_days);
			if (age !== null && age > limit)
				return escalate(
					`The reservation has been pending ${age} days, longer than ${limit}: a person checks it with the bank.`,
					['payment.status = pending', `${age} days pending > ${limit}`],
				);
			return no(`It is a reservation, not a payment. Customer is told: “${values.customer_text}”`, [
				'payment.status = pending',
				`${age ?? '?'} days pending ≤ ${limit}`,
			]);
		}
		case 'DIS-02': {
			if ((facts['party.vulnerable'] ?? '') === 'yes')
				return no('Vulnerable customer: no disconnection notice is sent; a person takes over.', [
					'party.vulnerable = yes',
				]);
			return yes(`A notice of ${values.notice_days} days is required before disconnection.`, [
				'party.vulnerable = no',
			]);
		}
		case 'ARR-01': {
			const amount = Number(facts['arrangement.instalment_eur']);
			const min = Number(values.min_instalment_eur);
			if (!Number.isFinite(amount)) return missing('arrangement.instalment_eur');
			if (amount < min)
				return no(`€${amount} a month is below the €${min} minimum.`, [`€${amount} < €${min}`]);
			return yes(`€${amount} a month meets the €${min} minimum.`, [`€${amount} ≥ €${min}`]);
		}
		default:
			return escalate(`Rule ${id} has no evaluation definition: a person must decide.`, []);
	}
}
