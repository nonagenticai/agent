// Attachments in the staff workspace (SW-9): what the composer may offer, what a refusal means in plain words,
// and how a file is named for a person. Each constant mirrors the server (crates/agent-api/src/attachments).
import type { AttachmentRef } from '$lib/api/types/support';

/** At most this many files on one reply (server: `attachments` ≤ 5). */
export const MAX_FILES = 5;
/** The file picker's filter; the server sniffs the bytes and decides. */
export const ACCEPT = 'image/jpeg,image/png,image/webp,application/pdf';
/** Channels whose customers can receive a photo or a PDF. Other channels cannot (yet). */
const CHANNELS = ['telegram', 'web_chat'];

export const channelTakesFiles = (channel: string) => CHANNELS.includes(channel);

/**
 * Why the paperclip is off, in plain words (null = it is on). `staffAllowed` is the admin's setting for this
 * conversation's channel binding (`conversation.attachments.staff`); absent = on, as the server defaults.
 */
export function noFilesReason(channel: string, staffAllowed?: boolean): string | null {
	if (!channelTakesFiles(channel)) return 'This channel cannot receive photos or PDFs.';
	if (staffAllowed === false)
		return 'Sending photos and PDFs is turned off for this channel in its settings.';
	return null;
}

const REASONS: Record<string, string> = {
	unsupported_type: 'Only JPEG, PNG, WebP photos and PDF files can be sent.',
	too_large: 'That file is too big. The limit is 10 MB.',
	too_many_pixels: 'That photo has too many pixels. Use one up to 4096 by 4096.',
	animated: 'Animated images cannot be sent. Use a still photo.',
	malformed_image: 'That image is damaged and could not be read.',
	malformed_pdf: 'That PDF is damaged and could not be read.',
	active_pdf: 'That PDF contains scripts or embedded files, so it cannot be sent.',
	encrypted_pdf: 'That PDF is password protected, so it cannot be sent.',
	malware: 'This file was blocked by the virus check.',
	scan_unavailable: "The virus check is unavailable, so files can't be added right now.",
	storage_unavailable: 'Attachment storage could not be reached, so the file was not saved. Try again.',
	attachments_on_note: 'Notes are text only. Send the files in a reply.',
	too_many_attachments: 'A reply can carry at most 5 files.',
	attachments_not_supported: 'Photos and PDFs cannot be sent on this channel.',
	attachments_off: 'Photos and PDFs are turned off for this channel.',
	payload_too_large: 'That file is too big. The limit is 10 MB.',
	quota: 'This workspace has used all its attachment storage. Ask an admin to free some.',
	attachment_quota: 'This workspace has used all its attachment storage. Ask an admin to free some.',
	channel_unsupported: 'This channel cannot receive photos or PDFs.',
};

/** The words for a refusal, from the error the API client threw (`details.reason`, or the code itself). */
export function refusalText(e: unknown): string {
	const x = e as { code?: string; details?: { reason?: string } };
	const key = x?.details?.reason ?? x?.code ?? '';
	return REASONS[key] ?? 'That file could not be attached. Try another one.';
}

export const isImage = (a: Pick<AttachmentRef, 'media_type'>) => a.media_type.startsWith('image/');

/** 12345 → "12 KB", 2500000 → "2.4 MB". */
export function sizeLabel(bytes: number): string {
	if (bytes < 1024) return `${bytes} B`;
	if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
	if (bytes >= 1024 ** 3) return `${(bytes / 1024 ** 3).toFixed(1)} GB`;
	return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}
