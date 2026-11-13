// L4 Talk to a job: the page's pure helpers — error wording, recording format, base64 audio, and the "Use the API"
// snippets for the selected job: curl + OpenAI SDK, email, async signed webhook (+ receivers) and MCP.
import { ApiError } from '$lib/api/errors';

/** What a failed turn tells the user, by the API's error code (never by message text alone). */
export function explainError(e: unknown): string {
	if (!(e instanceof ApiError)) return e instanceof Error ? e.message : String(e);
	switch (e.code) {
		case 'interaction_backend_unavailable':
			return 'The job’s agent backend is unavailable right now. Try again in a moment.';
		case 'job_not_live':
			return 'This job has no live release yet. Customers (API keys, connectors) reach it once a release is live; this page’s staff playground still works.';
		case 'voice_unavailable':
			return 'Voice is unavailable right now (speech recognition or synthesis is down). Typing still works.';
		case 'no_speech':
			return 'No speech was heard in that recording. Hold the button while you speak, a little closer to the microphone.';
		case 'unavailable':
			return `The service is unavailable: ${e.message}`;
		case 'network_error':
		case 'stream_incomplete':
			return e.message;
	}
	if (e.status === 401) return 'Your session has expired. Reload the page to sign in again.';
	if (e.status === 403) return `Not allowed: ${e.message}`;
	if (e.status === 413) return 'That recording is too long. Keep a turn under a minute.';
	return `${e.message} (${e.status ? `${e.status} ` : ''}${e.code})`;
}

/** Recording formats in order of preference (the API takes webm/opus; Safari records mp4). */
export const RECORDING_TYPES = ['audio/webm;codecs=opus', 'audio/webm', 'audio/ogg;codecs=opus', 'audio/mp4'];

/** The first format the browser can record, or '' to let it choose. */
export function pickRecordingMime(supported: (t: string) => boolean): string {
	for (const t of RECORDING_TYPES) {
		try {
			if (supported(t)) return t;
		} catch {
			/* a browser that throws on the probe: try the next */
		}
	}
	return '';
}

/** Decodes base64 audio into a playable Blob. */
export function base64ToBlob(b64: string, type: string): Blob {
	const bin = atob(b64);
	const bytes = new Uint8Array(bin.length);
	for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
	return new Blob([bytes], { type });
}

export interface Snippet {
	id: string;
	title: string;
	code: string;
}

const SAMPLE = 'Where is my order?';

/**
 * Copyable calls for one job: base URL `${origin}/api/jobs/<job>`, bearer `$AGENT_TOKEN`, and the super_admin's
 * `X-Active-Tenant-Id` when one is acting in a tenant. The OpenAI SDKs take the job key as `model`.
 */
export function apiSnippets(opts: SnippetOpts): {
	base: string;
	items: Snippet[];
} {
	const base = `${opts.origin}/api/jobs/${encodeURIComponent(opts.job)}`;
	const model = opts.job;
	const tenantH = opts.tenant ? ` \\\n  -H "X-Active-Tenant-Id: ${opts.tenant}"` : '';
	const auth = `-H "Authorization: Bearer ${opts.apiKey ?? '$AGENT_TOKEN'}"${tenantH}`;
	const json = `${auth} \\\n  -H "Content-Type: application/json"`;
	const msgs = JSON.stringify([{ role: 'user', content: SAMPLE }]);
	const pyHeaders = opts.tenant ? `,\n    default_headers={"X-Active-Tenant-Id": "${opts.tenant}"},` : ',';
	const jsHeaders = opts.tenant ? `,\n  defaultHeaders: { 'X-Active-Tenant-Id': '${opts.tenant}' },` : ',';
	return {
		base,
		items: [
			{
				id: 'curl-interaction',
				title: 'curl · one turn (interaction)',
				code: `BASE="${base}"

curl -sS "$BASE/interactions" \\
  ${json} \\
  -d '${JSON.stringify({ input: SAMPLE })}'`,
			},
			{
				id: 'curl-stream',
				title: 'curl · streamed turn with history (server-sent events)',
				code: `curl -N "$BASE/interactions" \\
  ${json} \\
  -d '${JSON.stringify({
		input: [
			{ role: 'user', content: SAMPLE },
			{ role: 'assistant', content: 'Your ceramic lamp from 14 October, or the doormat?' },
			{ role: 'user', content: 'The lamp, please.' },
		],
		stream: true,
	})}'
# event: interaction.created … event: output_text.delta (data {"delta":…}) … event: interaction.completed`,
			},
			{
				id: 'curl-voice',
				title: 'curl · voice turn (audio in, transcript + spoken reply out)',
				code: `curl -sS "$BASE/voice/turns" \\
  ${auth} \\
  -F "audio=@question.webm" \\
  -F 'history=[]' \\
  -F "language=en" > turn.json
jq -r .reply_text turn.json
jq -r .audio_b64 turn.json | base64 -d > reply.wav`,
			},
			{
				id: 'curl-openai',
				title: 'curl · OpenAI-compatible (Responses, Chat Completions)',
				code: `curl -sS "$BASE/responses" \\
  ${json} \\
  -d '${JSON.stringify({ model, input: SAMPLE })}'

curl -sS "$BASE/chat/completions" \\
  ${json} \\
  -d '${JSON.stringify({ model, messages: JSON.parse(msgs) })}'`,
			},
			{
				id: 'python',
				title: 'Python · OpenAI SDK',
				code: `import os
from openai import OpenAI

client = OpenAI(
    base_url="${base}",
    api_key=${opts.apiKey ? `"${opts.apiKey}"` : 'os.environ["AGENT_TOKEN"]'}${pyHeaders}
)

r = client.responses.create(model="${model}", input="${SAMPLE}")
print(r.output_text)

c = client.chat.completions.create(
    model="${model}",
    messages=[{"role": "user", "content": "${SAMPLE}"}],
)
print(c.choices[0].message.content)`,
			},
			{
				id: 'js',
				title: 'JavaScript · OpenAI SDK',
				code: `import OpenAI from 'openai';

const client = new OpenAI({
  baseURL: '${base}',
  apiKey: ${opts.apiKey ? `'${opts.apiKey}'` : 'process.env.AGENT_TOKEN'}${jsHeaders}
});

const r = await client.responses.create({ model: '${model}', input: '${SAMPLE}' });
console.log(r.output_text);

const c = await client.chat.completions.create({
  model: '${model}',
  messages: [{ role: 'user', content: '${SAMPLE}' }],
});
console.log(c.choices[0].message.content);`,
			},
		],
	};
}

/** One "Use the API" section: a connector and its copyable snippets. */
export interface SnippetGroup {
	id: string;
	title: string;
	/** One line on what the connector is for. */
	note: string;
	items: Snippet[];
}

/**
 * `apiKey`: a job API key (`ak_…`, B13) the caller just created — the snippets then carry it as the bearer instead of
 * `$AGENT_TOKEN`. It is never stored; the page drops it when the reader dismisses the secret.
 */
type SnippetOpts = { origin: string; job: string; tenant?: string | null; apiKey?: string | null };

function baseOf(opts: SnippetOpts): string {
	return `${opts.origin}/api/jobs/${encodeURIComponent(opts.job)}`;
}
/** curl's bearer (`$AGENT_TOKEN`, or the just-created job API key) and, when acting in a tenant, its header. */
function curlAuth(tenant?: string | null, apiKey?: string | null): string {
	const tenantH = tenant ? ` \\\n  -H "X-Active-Tenant-Id: ${tenant}"` : '';
	return `-H "Authorization: Bearer ${apiKey ?? '$AGENT_TOKEN'}"${tenantH}`;
}

/** Where the samples deliver async results (a placeholder the reader replaces). */
export const SAMPLE_CALLBACK = 'https://example.com/hooks/agent';

/** Async `POST …/interactions` with `callback_url`, the signing secret, and receivers that verify the signature. */
export function webhookSnippets(opts: SnippetOpts): SnippetGroup {
	const auth = curlAuth(opts.tenant, opts.apiKey);
	return {
		id: 'webhook',
		title: 'Async + signed webhook',
		note: 'Queue a turn and get the answer POSTed to your HTTPS endpoint, signed per the Standard Webhooks spec (webhook-id, webhook-timestamp, webhook-signature).',
		items: [
			{
				id: 'curl-webhook',
				title: 'curl · queue a turn with a callback_url',
				code: `BASE="${baseOf(opts)}"

curl -sS "$BASE/interactions" \\
  ${auth} \\
  -H "Content-Type: application/json" \\
  -d '${JSON.stringify({ input: SAMPLE, callback_url: SAMPLE_CALLBACK })}'
# → 202 {"id":"int_…","object":"interaction","status":"queued",…}
# The result is POSTed to callback_url with webhook-id, webhook-timestamp and webhook-signature: v1,<base64 HMAC-SHA256>

# The job's signing secret (integrations manager role):
curl -sS "$BASE/interactions/webhook-secret" \\
  ${auth}
# → {"secret":"whsec_…","scheme":"standard-webhooks-v1","signature_header":"webhook-signature"}`,
			},
			{
				id: 'node-webhook',
				title: 'Node · verify the webhook (standardwebhooks)',
				code: `// npm install standardwebhooks express
import express from 'express';
import { Webhook } from 'standardwebhooks';

const wh = new Webhook(process.env.AGENT_WEBHOOK_SECRET); // whsec_…
const app = express();

// Verify the raw body: parsing JSON first changes the bytes that were signed.
app.post('/hooks/agent', express.raw({ type: 'application/json' }), (req, res) => {
  let result;
  try {
    result = wh.verify(req.body.toString('utf8'), req.headers);
  } catch {
    return res.status(400).end(); // bad signature or stale timestamp
  }
  console.log(result.status, result.output_text);
  res.status(204).end();
});

app.listen(3000);`,
			},
			{
				id: 'python-webhook',
				title: 'Python · verify the webhook (standardwebhooks)',
				code: `# pip install standardwebhooks flask
import os
from flask import Flask, abort, request
from standardwebhooks.webhooks import Webhook

wh = Webhook(os.environ["AGENT_WEBHOOK_SECRET"])  # whsec_…
app = Flask(__name__)

@app.post("/hooks/agent")
def agent_hook():
    try:
        result = wh.verify(request.get_data(), dict(request.headers))
    except Exception:
        abort(400)  # bad signature or stale timestamp
    print(result["status"], result.get("output_text"))
    return "", 204`,
			},
		],
	};
}

/** An MCP server name for the job: `agent-<job>`, letters, digits, `-` and `_` only. */
export function mcpServerName(job: string): string {
	return `agent-${job.replace(/[^A-Za-z0-9_-]+/g, '-')}`;
}

/** `POST /api/jobs/{job}/mcp`: the stateless MCP server (one tool, `ask`) for Claude Code and other MCP clients. */
export function mcpSnippets(opts: SnippetOpts): SnippetGroup {
	const url = `${baseOf(opts)}/mcp`;
	const name = mcpServerName(opts.job);
	const tenantFlag = opts.tenant ? ` \\\n  --header "X-Active-Tenant-Id: ${opts.tenant}"` : '';
	const headers: Record<string, string> = { Authorization: `Bearer ${opts.apiKey ?? '${AGENT_TOKEN}'}` };
	if (opts.tenant) headers['X-Active-Tenant-Id'] = opts.tenant;
	const config = { mcpServers: { [name]: { type: 'http', url, headers } } };
	return {
		id: 'mcp',
		title: 'MCP',
		note: 'The job as an MCP server (Streamable HTTP, stateless) with one tool, ask {input, instructions?}. In the JSON config, ${AGENT_TOKEN} is read from the environment by clients that expand it (Claude Code does); others need the token pasted in.',
		items: [
			{
				id: 'mcp-claude',
				title: 'Claude Code · add the server',
				code: `claude mcp add --transport http ${name} ${url} \\
  --header "Authorization: Bearer ${opts.apiKey ?? '$AGENT_TOKEN'}"${tenantFlag}`,
			},
			{
				id: 'mcp-json',
				title: 'JSON config (.mcp.json and other MCP clients)',
				code: JSON.stringify(config, null, 2),
			},
		],
	};
}

/** Explains why the signing secret cannot be shown, by status and code. */
export function explainSecretError(e: unknown): string {
	if (e instanceof ApiError) {
		if (e.status === 409 || e.code === 'webhooks_not_configured')
			return 'Webhook signing is not configured on this platform';
		if (e.status === 403) return 'Only an integrations manager can view the signing secret';
	}
	return explainError(e);
}

/** `whsec_` + dots + the last 4 characters: enough to recognise, not to use. */
export function maskSecret(secret: string): string {
	const prefix = secret.startsWith('whsec_') ? 'whsec_' : '';
	const tail = secret.length - prefix.length > 8 ? secret.slice(-4) : '';
	return `${prefix}${'•'.repeat(16)}${tail}`;
}
