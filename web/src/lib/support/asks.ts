// Canned-reply guard (owner ruling 2026-10-26, "I know my customer that I serve"): customers are KYC'd, so a staff
// reply never asks them for a lookup key. A hint, never a block.

/** "You already know this customer — their orders are in the panel on the right." */
export const KYC_HINT = 'You already know this customer — their orders are in the panel on the right.';

const KEY =
	/\b(?:order\s*(?:number|no\.?|nr\.?|id|#)|ordernummer|ordernr\.?|bestelnummer|order\s*id|post\s?code|postal\s*code|zip\s*code|e-?mail\s*address|e-?mailadres|e-?mail\s*adres|customer\s*(?:number|id|no\.?)|klantnummer|klant\s*id)(?![\w])/i;
// A request cue: an imperative or a question opener (English + Dutch). "?" counts as a cue on its own.
const ASK =
	/\?|\b(?:what(?:'s|\s+is)|which|can\s+you|could\s+you|would\s+you|will\s+you|please|send|share|provide|give|tell|confirm|enter|type|wat\s+is|welk|kun\s+je|kunt\s+u|kan\s+je|kan\s+u|wilt\s+u|wil\s+je|geef|stuur|deel|graag|doorgeven|noem|vermeld|laat)\b/i;

/** True when a sentence of `text` asks for an order number, postcode, e-mail address or customer number. */
export function asksForIdentifier(text: string): boolean {
	return text.split(/(?<=[.!?])\s+|\n+/).some((s) => KEY.test(s) && ASK.test(s));
}
