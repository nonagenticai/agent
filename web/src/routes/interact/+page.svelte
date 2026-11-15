<script lang="ts">
	// L4 Talk to a job — the interaction API demo. Pick one of the tenant's jobs and talk to it in text (the reply
	// streams in) or by voice (push to talk: the recording is transcribed, answered and spoken back). The API is
	// stateless: this page keeps the conversation and sends the history with every turn. "Use the API" shows the same
	// calls as curl and OpenAI SDK snippets for the selected job, plus the async signed-webhook and MCP
	// connectors (with the job's webhook signing secret for integrations managers). B13: the job's API keys (create,
	// revoke); right after a create the snippets carry the new `ak_…` key as the bearer until it is dismissed.
	import { onDestroy, onMount } from 'svelte';
	import { api, app, selectJob } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { ApiError } from '$lib/api/errors';
	import { LANGUAGE_LABEL, type JobSummary, type Paged } from '$lib/api/types/core';
	import type { InteractionMessage } from '$lib/api/types/interaction';
	import type { Snippet as ApiSnippet } from '$lib/interact/interact';
	import {
		apiSnippets,
		base64ToBlob,
		explainError,
		explainSecretError,
		maskSecret,
		mcpSnippets,
		pickRecordingMime,
		webhookSnippets,
	} from '$lib/interact/interact';
	import PageHead from '$lib/ui/PageHead.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import ApiKeys from '$lib/interact/ApiKeys.svelte';

	type Msg = {
		id: number;
		role: 'user' | 'assistant';
		text: string;
		via: 'text' | 'voice';
		state: 'pending' | 'streaming' | 'done' | 'stopped' | 'error';
		error?: string;
		/** The user text to send again (on a failed reply). */
		retry?: string;
		audioUrl?: string;
	};

	/** A press shorter than this is a tap: recording keeps going until the next press (toggle). */
	const HOLD_MS = 350;
	const MIN_MS = 400;
	const MAX_S = 60;
	const AUTOPLAY_KEY = 'agent-interact-autoplay';

	const jobs = new Resource<Paged<JobSummary>>(() => api().jobs());
	const jobList = $derived(jobs.value.state === 'ok' ? jobs.value.data.items : []);
	const jobKey = $derived(jobList.some((j) => j.key === app.job) ? app.job : (jobList[0]?.key ?? ''));
	const jobLabel = $derived(jobList.find((j) => j.key === jobKey)?.label ?? jobKey);

	let msgs = $state<Msg[]>([]);
	let text = $state('');
	let streamOn = $state(true);
	let autoplay = $state(true);
	let language = $state('');
	let busy = $state(false);
	/** A text reply is in flight (Stop aborts it). */
	let stoppable = $state(false);
	let controller: AbortController | null = null;
	let seq = 0;

	let rec = $state<'idle' | 'starting' | 'recording'>('idle');
	let level = $state(0);
	let elapsed = $state(0);
	let micError = $state('');
	let playNote = $state('');
	let playingId = $state<number | null>(null);
	let origin = $state('');
	let copied = $state('');

	let recorder: MediaRecorder | null = null;
	let media: MediaStream | null = null;
	let chunks: Blob[] = [];
	let audioCtx: AudioContext | null = null;
	let raf = 0;
	let startedAt = 0;
	let pressAt = 0;
	let holding = false;
	let player: HTMLAudioElement | null = null;
	const urls: string[] = [];
	let logEl: HTMLElement | undefined = $state();

	/** A job API key created on this page, shown once: the snippets use it until it is dismissed. */
	let apiKey = $state<string | null>(null);
	const snippetOpts = $derived({
		origin: origin || 'https://<host>',
		job: jobKey || '<job>',
		tenant: app.activeTenant,
		apiKey,
	});
	const snippets = $derived(apiSnippets(snippetOpts));
	const groups = $derived([webhookSnippets(snippetOpts), mcpSnippets(snippetOpts)]);
	const recSeconds = $derived(Math.floor(elapsed / 1000));

	/** The job's webhook signing secret, fetched on demand and masked until revealed. */
	let secret = $state<
		| { state: 'idle' }
		| { state: 'loading' }
		| { state: 'ok'; value: string; shown: boolean }
		| { state: 'error'; message: string }
	>({ state: 'idle' });
	async function loadSecret() {
		if (!jobKey || secret.state === 'loading') return;
		secret = { state: 'loading' };
		try {
			const r = await api().webhookSecret(jobKey);
			secret = { state: 'ok', value: r.secret, shown: false };
		} catch (e) {
			secret = { state: 'error', message: explainSecretError(e) };
		}
	}

	onMount(() => {
		origin = location.origin;
		try {
			autoplay = localStorage.getItem(AUTOPLAY_KEY) !== 'off';
		} catch {
			/* storage blocked: default on */
		}
		void jobs.refresh();
	});
	onDestroy(() => {
		controller?.abort();
		if (recorder && recorder.state !== 'inactive') {
			recorder.onstop = null;
			recorder.stop();
		}
		stopInput();
		player?.pause();
		for (const u of urls) URL.revokeObjectURL(u);
	});

	function setAutoplay(on: boolean) {
		autoplay = on;
		try {
			localStorage.setItem(AUTOPLAY_KEY, on ? 'on' : 'off');
		} catch {
			/* this page only */
		}
	}

	function chooseJob(key: string) {
		if (key === jobKey) return;
		// The page remounts for the new job (the layout keys pages on the job): a fresh conversation.
		selectJob(key);
	}

	/** Adds a message and returns the reactive row (mutate it to update the bubble). */
	function push(m: Omit<Msg, 'id'>): Msg {
		msgs.push({ ...m, id: ++seq });
		scrollDown();
		return msgs[msgs.length - 1]!;
	}
	function scrollDown() {
		queueMicrotask(() => logEl?.scrollTo?.({ top: logEl.scrollHeight }));
	}
	/** What the API needs to continue: every finished turn, oldest first. */
	function history(): InteractionMessage[] {
		return msgs.filter((m) => m.state === 'done' && m.text).map((m) => ({ role: m.role, content: m.text }));
	}

	async function sendText(raw = text) {
		const content = raw.trim();
		if (!content || busy || !jobKey) return;
		const input: InteractionMessage[] = [...history(), { role: 'user', content }];
		text = '';
		push({ role: 'user', text: content, via: 'text', state: 'done' });
		const a = push({ role: 'assistant', text: '', via: 'text', state: streamOn ? 'streaming' : 'pending' });
		busy = true;
		stoppable = true;
		const ctl = new AbortController();
		controller = ctl;
		try {
			const res = await api().interact(jobKey, {
				input,
				stream: streamOn,
				signal: ctl.signal,
				onDelta: (d) => {
					a.text += d;
					scrollDown();
				},
			});
			a.text = res.output_text || a.text;
			a.state = 'done';
		} catch (e) {
			if (ctl.signal.aborted) {
				a.state = 'stopped';
			} else {
				a.state = 'error';
				a.error = explainError(e);
				a.retry = content;
			}
		} finally {
			busy = false;
			stoppable = false;
			controller = null;
			scrollDown();
		}
	}
	function stop() {
		controller?.abort();
	}
	function retry(m: Msg) {
		const content = m.retry;
		if (!content) return;
		const i = msgs.findIndex((x) => x.id === m.id);
		// Drop the failed reply and the message it answered; send that message again.
		const userAt = msgs.slice(0, i).findLastIndex((x) => x.role === 'user');
		msgs.splice(i, 1);
		if (userAt >= 0) msgs.splice(userAt, 1);
		void sendText(content);
	}
	function clearChat() {
		stop();
		player?.pause();
		playingId = null;
		msgs = [];
	}

	// ---- voice -------------------------------------------------------------------------------------------------

	function micMessage(e: unknown): string {
		const name = (e as { name?: string } | null)?.name;
		if (name === 'NotAllowedError' || name === 'SecurityError')
			return 'Microphone access was denied. Allow the microphone for this site in the browser’s address bar, then press the button again.';
		if (name === 'NotFoundError' || name === 'OverconstrainedError')
			return 'No microphone was found. Connect one and try again.';
		if (name === 'NotReadableError') return 'The microphone is in use by another application.';
		return `The microphone could not be started: ${e instanceof Error ? e.message : String(e)}`;
	}

	async function startRec() {
		micError = '';
		playNote = '';
		if (!navigator.mediaDevices?.getUserMedia || typeof MediaRecorder === 'undefined') {
			micError =
				'This browser cannot record here. Voice needs a secure page (HTTPS or localhost) and MediaRecorder support.';
			return;
		}
		player?.pause();
		rec = 'starting';
		try {
			media = await navigator.mediaDevices.getUserMedia({
				audio: { echoCancellation: true, noiseSuppression: true },
			});
		} catch (e) {
			rec = 'idle';
			micError = micMessage(e);
			return;
		}
		if (rec !== 'starting') return stopInput();
		const mimeType = pickRecordingMime((t) => MediaRecorder.isTypeSupported(t));
		try {
			recorder = new MediaRecorder(media, mimeType ? { mimeType } : undefined);
		} catch (e) {
			rec = 'idle';
			stopInput();
			micError = `Recording is not supported here: ${e instanceof Error ? e.message : String(e)}`;
			return;
		}
		chunks = [];
		recorder.ondataavailable = (ev) => {
			if (ev.data.size) chunks.push(ev.data);
		};
		recorder.onstop = () => void finishRec();
		recorder.start(250);
		startedAt = performance.now();
		elapsed = 0;
		rec = 'recording';
		meter(media);
	}

	/** Input level (RMS of the waveform) and the timer, every frame while recording. */
	function meter(stream: MediaStream) {
		let analyser: AnalyserNode | null = null;
		let buf: Uint8Array<ArrayBuffer> | null = null;
		try {
			audioCtx = new AudioContext();
			analyser = audioCtx.createAnalyser();
			analyser.fftSize = 512;
			audioCtx.createMediaStreamSource(stream).connect(analyser);
			buf = new Uint8Array(analyser.fftSize);
		} catch {
			analyser = null;
		}
		const frame = () => {
			elapsed = performance.now() - startedAt;
			if (analyser && buf) {
				analyser.getByteTimeDomainData(buf);
				let sum = 0;
				for (const v of buf) sum += ((v - 128) / 128) ** 2;
				level = Math.min(1, Math.sqrt(sum / buf.length) * 4);
			}
			if (elapsed >= MAX_S * 1000) return stopRec();
			raf = requestAnimationFrame(frame);
		};
		raf = requestAnimationFrame(frame);
	}

	function stopInput() {
		cancelAnimationFrame(raf);
		level = 0;
		void audioCtx?.close().catch(() => {});
		audioCtx = null;
		for (const t of media?.getTracks() ?? []) t.stop();
		media = null;
	}

	function stopRec() {
		if (rec === 'starting') {
			rec = 'idle';
			return;
		}
		if (recorder && recorder.state !== 'inactive') recorder.stop();
	}

	async function finishRec() {
		const took = performance.now() - startedAt;
		const type = recorder?.mimeType || chunks[0]?.type || 'audio/webm';
		recorder = null;
		stopInput();
		rec = 'idle';
		const blob = new Blob(chunks, { type });
		chunks = [];
		if (took < MIN_MS || blob.size === 0) {
			micError =
				'That was too short. Hold the button while you speak, or tap once to start and again to stop.';
			return;
		}
		await sendVoice(blob);
	}

	async function sendVoice(audio: Blob) {
		if (!jobKey) return;
		const h = history();
		const u = push({ role: 'user', text: '', via: 'voice', state: 'pending' });
		const a = push({ role: 'assistant', text: '', via: 'voice', state: 'pending' });
		busy = true;
		try {
			const r = await api().voiceTurn(jobKey, { audio, history: h, language: language || undefined });
			u.text = r.transcript;
			u.state = 'done';
			a.text = r.reply_text;
			a.state = 'done';
			if (r.audio_b64) {
				const url = URL.createObjectURL(base64ToBlob(r.audio_b64, r.audio_mime || 'audio/wav'));
				urls.push(url);
				a.audioUrl = url;
				if (autoplay) play(a);
			}
		} catch (e) {
			msgs = msgs.filter((m) => m.id !== u.id);
			a.state = 'error';
			a.error = explainError(e);
		} finally {
			busy = false;
			scrollDown();
		}
	}

	function play(m: Msg) {
		if (!m.audioUrl) return;
		player?.pause();
		playNote = '';
		const el = new Audio(m.audioUrl);
		player = el;
		playingId = m.id;
		el.onended = () => {
			if (player === el) playingId = null;
		};
		el.play().catch((e: unknown) => {
			if (player === el) playingId = null;
			if ((e as { name?: string } | null)?.name === 'NotAllowedError')
				playNote = 'The browser blocked autoplay. Press “Play reply” to hear it.';
		});
	}
	function stopPlaying() {
		player?.pause();
		playingId = null;
	}

	/** Pointer: hold to talk (released after HOLD_MS stops), or a quick tap to start and another to stop. */
	function press(e: PointerEvent) {
		if (e.button !== 0 || busy || !jobKey) return;
		e.preventDefault();
		if (rec === 'recording') return stopRec();
		if (rec !== 'idle') return;
		pressAt = performance.now();
		holding = true;
		try {
			(e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
		} catch {
			/* capture is a nicety */
		}
		void startRec();
	}
	function release() {
		if (!holding) return;
		holding = false;
		if (rec === 'recording' && performance.now() - pressAt > HOLD_MS) stopRec();
	}
	/** Keyboard and assistive tech (a click with no pointer behind it): toggle. */
	function keyClick(e: MouseEvent) {
		if (e.detail !== 0 || busy || !jobKey) return;
		if (rec === 'recording' || rec === 'starting') stopRec();
		else void startRec();
	}

	async function copy(id: string, value: string) {
		try {
			await navigator.clipboard.writeText(value);
			copied = id;
			setTimeout(() => {
				if (copied === id) copied = '';
			}, 1500);
		} catch {
			copied = '';
		}
	}

	const micLabel = $derived(
		rec === 'recording'
			? `Recording ${recSeconds}s. Release, or press again, to send`
			: rec === 'starting'
				? 'Starting the microphone…'
				: 'Hold to talk (or tap to start, tap again to send)',
	);
	const failed = $derived(jobs.value.state === 'unavailable' ? jobs.value : null);
</script>

{#snippet snip(s: ApiSnippet)}
	<div class="ia-snip">
		<div class="ia-snip-head">
			<h4>{s.title}</h4>
			<button class="btn sm" type="button" onclick={() => copy(s.id, s.code)}
				>{copied === s.id ? 'Copied' : 'Copy'}</button
			>
		</div>
		<pre><code>{s.code}</code></pre>
	</div>
{/snippet}

<PageHead
	id="L4"
	sub="Talk to one of this tenant’s jobs over the interaction API: type, or hold the mic button and speak. The API keeps no conversation: this page sends the history with every turn."
/>

{#if jobs.value.state === 'loading'}
	<p class="muted" role="status">Loading jobs…</p>
{:else if failed}
	<Unavailable reason={failed.reason} retry={jobs.retry} />
{:else if jobs.value.state === 'empty'}
	<div class="empty">This tenant has no jobs yet. Create one on the Jobs page first.</div>
{:else}
	<div class="ia-bar">
		<div class="field ia-job">
			<label for="ia-job">Job</label>
			<select
				id="ia-job"
				class="input"
				value={jobKey}
				onchange={(e) => chooseJob((e.currentTarget as HTMLSelectElement).value)}
				disabled={busy || rec !== 'idle'}
			>
				{#each jobList as j (j.key)}
					<option value={j.key}>{j.label} ({j.key})</option>
				{/each}
			</select>
		</div>
		<div class="field ia-lang">
			<label for="ia-lang">Voice language</label>
			<select id="ia-lang" class="input" bind:value={language}>
				<option value="">Detect</option>
				{#each Object.entries(LANGUAGE_LABEL) as [tag, name] (tag)}
					<option value={tag}>{name}</option>
				{/each}
			</select>
		</div>
		<label class="ia-check"><input type="checkbox" bind:checked={streamOn} /> Stream replies</label>
		<label class="ia-check"
			><input type="checkbox" checked={autoplay} onchange={(e) => setAutoplay(e.currentTarget.checked)} /> Play
			voice replies</label
		>
		<button class="btn sm" type="button" onclick={clearChat} disabled={!msgs.length}>New conversation</button>
	</div>

	{#if micError}
		<Callout kind="err" title="Microphone">{micError}</Callout>
	{/if}
	{#if playNote}
		<Callout kind="info" title="Audio">{playNote}</Callout>
	{/if}

	<section class="ia-chat card" aria-label="Conversation with {jobLabel}">
		<div class="ia-log" bind:this={logEl} role="log" aria-live="polite" aria-relevant="additions text">
			{#if !msgs.length}
				<p class="muted ia-hint">
					Say or type something to {jobLabel}. Each turn calls
					<code>POST /api/jobs/{jobKey}/interactions</code>
					(text) or
					<code>…/voice/turns</code> (voice).
				</p>
			{/if}
			{#each msgs as m (m.id)}
				<div class="ia-msg {m.role}" class:err={m.state === 'error'}>
					<div class="ia-who">
						{m.role === 'user' ? 'You' : jobLabel}
						{#if m.via === 'voice'}<span class="chip">voice</span>{/if}
						{#if m.state === 'stopped'}<span class="chip warn">stopped</span>{/if}
					</div>
					{#if m.state === 'error'}
						<p class="ia-text" role="alert">{m.error}</p>
						{#if m.retry}
							<button class="btn sm" type="button" onclick={() => retry(m)} disabled={busy}>Try again</button>
						{/if}
					{:else if m.state === 'pending' && !m.text}
						<p class="ia-text muted">
							{m.via === 'voice' ? (m.role === 'user' ? 'Transcribing…' : 'Thinking…') : 'Thinking…'}
						</p>
					{:else}
						<p class="ia-text">
							{m.text}{#if m.state === 'streaming'}<span class="ia-caret" aria-hidden="true"></span>{/if}
						</p>
					{/if}
					{#if m.audioUrl}
						{#if playingId === m.id}
							<button class="btn sm" type="button" onclick={stopPlaying}>Stop audio</button>
						{:else}
							<button class="btn sm" type="button" onclick={() => play(m)}>Play reply</button>
						{/if}
					{/if}
				</div>
			{/each}
		</div>

		<form
			class="ia-compose"
			onsubmit={(e) => {
				e.preventDefault();
				void sendText();
			}}
		>
			<label class="sr-only" for="ia-text">Message</label>
			<textarea
				id="ia-text"
				class="input"
				rows="2"
				maxlength="4000"
				placeholder="Type a message… (Enter sends, Shift+Enter for a new line)"
				bind:value={text}
				disabled={rec !== 'idle'}
				onkeydown={(e) => {
					if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
						e.preventDefault();
						void sendText();
					}
				}}></textarea>
			<div class="ia-actions">
				{#if stoppable}
					<button class="btn" type="button" onclick={stop}>Stop</button>
				{:else}
					<button class="btn primary" type="submit" disabled={busy || !text.trim() || rec !== 'idle'}
						>Send</button
					>
				{/if}
				<button
					class="btn ia-mic"
					class:on={rec === 'recording'}
					type="button"
					aria-pressed={rec === 'recording'}
					aria-label={micLabel}
					title={micLabel}
					disabled={busy}
					onpointerdown={press}
					onpointerup={release}
					onpointercancel={release}
					onclick={keyClick}
					oncontextmenu={(e) => e.preventDefault()}
				>
					<svg viewBox="0 0 24 24" width="18" height="18" aria-hidden="true"
						><path
							d="M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3zM5 11a7 7 0 0 0 14 0M12 18v3"
							fill="none"
							stroke="currentColor"
							stroke-width="2"
							stroke-linecap="round"
						/></svg
					>
					{#if rec === 'recording'}
						<span class="ia-level" aria-hidden="true"
							><span style:width="{Math.round(level * 100)}%"></span></span
						>
						<span class="mono">{recSeconds}s</span>
					{:else if rec === 'starting'}
						<span>Mic…</span>
					{:else}
						<span>Hold to talk</span>
					{/if}
				</button>
			</div>
		</form>
		<p class="muted ia-foot" role="status">
			{#if rec === 'recording'}
				Listening… release (or press again) to send. Stops by itself after {MAX_S}s.
			{:else if busy}
				Waiting for {jobLabel}…
			{:else}
				Hold the mic button while you speak, or tap it once to start and again to send.
			{/if}
		</p>
	</section>

	<details class="card ia-api">
		<summary>Use the API</summary>
		<div class="ia-api-body">
			<p class="muted">
				Base URL for <strong>{jobLabel}</strong>: <code>{snippets.base}</code>. Authenticate with a bearer
				access token for your account (<code>$AGENT_TOKEN</code>), or with a job API key (<code>ak_…</code>,
				below) from a server. The API is stateless: send the whole conversation each turn.
				<code>/responses</code> and <code>/chat/completions</code> speak the OpenAI shapes, so the OpenAI SDKs
				work with this base URL.
			</p>
			{#if jobKey}
				{#key jobKey}
					<ApiKeys job={jobKey} onsecret={(k) => (apiKey = k)} />
				{/key}
			{/if}
			{#each snippets.items as s (s.id)}
				{@render snip(s)}
			{/each}
			{#each groups as g (g.id)}
				<section class="ia-group" aria-labelledby="ia-group-{g.id}">
					<h3 id="ia-group-{g.id}">{g.title}</h3>
					<p class="muted">{g.note}</p>
					{#each g.items as s (s.id)}
						{@render snip(s)}
						{#if s.id === 'curl-webhook'}
							<div class="ia-secret">
								{#if secret.state === 'ok'}
									<code class="mono" aria-label="Webhook signing secret"
										>{secret.shown ? secret.value : maskSecret(secret.value)}</code
									>
									<button
										class="btn sm"
										type="button"
										onclick={() => {
											if (secret.state === 'ok') secret.shown = !secret.shown;
										}}>{secret.shown ? 'Hide' : 'Reveal'}</button
									>
									<button
										class="btn sm"
										type="button"
										onclick={() => secret.state === 'ok' && copy('webhook-secret', secret.value)}
										>{copied === 'webhook-secret' ? 'Copied' : 'Copy secret'}</button
									>
								{:else}
									<button
										class="btn sm"
										type="button"
										onclick={loadSecret}
										disabled={secret.state === 'loading' || !jobKey}
										>{secret.state === 'loading' ? 'Loading…' : 'Show signing secret'}</button
									>
									{#if secret.state === 'error'}
										<span class="ia-secret-err" role="alert">{secret.message}</span>
									{/if}
								{/if}
							</div>
						{/if}
					{/each}
				</section>
			{/each}
		</div>
	</details>
{/if}

<style>
	.ia-bar {
		display: flex;
		flex-wrap: wrap;
		gap: 12px;
		align-items: flex-end;
		margin-bottom: 12px;
	}
	.ia-job {
		min-width: 220px;
		flex: 1 1 260px;
		max-width: 420px;
	}
	.ia-lang {
		width: 150px;
	}
	.ia-check {
		display: inline-flex;
		gap: 6px;
		align-items: center;
		font-size: 13px;
		min-height: 34px;
	}
	.ia-chat {
		display: grid;
		grid-template-rows: minmax(0, 1fr) auto auto;
		height: min(64vh, 640px);
		min-height: 360px;
		padding: 0;
		overflow: hidden;
		margin-top: 12px;
	}
	.ia-log {
		overflow-y: auto;
		padding: 14px;
		display: flex;
		flex-direction: column;
		gap: 10px;
		background: var(--bg);
	}
	.ia-hint {
		margin: auto;
		text-align: center;
		max-width: 46ch;
	}
	.ia-msg {
		max-width: min(78%, 640px);
		padding: 8px 12px;
		border-radius: 10px;
		border: 1px solid var(--line);
		background: var(--panel);
		display: grid;
		gap: 4px;
		justify-items: start;
	}
	.ia-msg.user {
		align-self: flex-end;
		background: color-mix(in srgb, var(--accent) 12%, var(--panel));
		border-color: color-mix(in srgb, var(--accent) 30%, var(--line));
	}
	.ia-msg.assistant {
		align-self: flex-start;
	}
	.ia-msg.err {
		border-color: color-mix(in srgb, var(--err) 45%, var(--line));
	}
	.ia-msg.err .ia-text {
		color: var(--err);
	}
	.ia-who {
		font-size: 11.5px;
		color: var(--muted);
		display: flex;
		gap: 6px;
		align-items: center;
	}
	.ia-text {
		margin: 0;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.ia-caret {
		display: inline-block;
		width: 7px;
		height: 1em;
		margin-left: 2px;
		vertical-align: text-bottom;
		background: var(--accent);
		animation: ia-blink 1s steps(2) infinite;
	}
	@keyframes ia-blink {
		50% {
			opacity: 0;
		}
	}
	@media (prefers-reduced-motion: reduce) {
		.ia-caret {
			animation: none;
		}
	}
	.ia-compose {
		display: flex;
		gap: 10px;
		align-items: flex-end;
		padding: 10px 12px;
		border-top: 1px solid var(--line);
		background: var(--panel);
	}
	.ia-compose textarea {
		flex: 1;
		resize: vertical;
		min-height: 42px;
		max-height: 200px;
	}
	.ia-actions {
		display: flex;
		gap: 8px;
	}
	.ia-mic {
		min-width: 132px;
		touch-action: none;
		user-select: none;
		-webkit-user-select: none;
	}
	.ia-mic.on {
		border-color: var(--err);
		color: var(--err);
		background: color-mix(in srgb, var(--err) 10%, var(--panel));
	}
	.ia-level {
		width: 40px;
		height: 6px;
		border-radius: 3px;
		background: var(--sunk);
		overflow: hidden;
	}
	.ia-level span {
		display: block;
		height: 100%;
		background: var(--err);
	}
	.ia-foot {
		margin: 0;
		padding: 0 12px 10px;
		font-size: 12px;
		background: var(--panel);
	}
	.ia-api {
		margin-top: 16px;
		padding: 0;
	}
	.ia-api summary {
		cursor: pointer;
		padding: 12px 16px;
		font-weight: 600;
	}
	/* One shrinkable track: a grid item's min-width is its content by default, so a long snippet line or base URL
	   would widen the whole card past a phone's edge (the snippets scroll inside their own pre instead). */
	.ia-api-body {
		padding: 0 16px 16px;
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 14px;
	}
	.ia-api-body p {
		margin: 0;
	}
	.ia-api-body p code {
		overflow-wrap: anywhere;
	}
	.ia-snip {
		min-width: 0;
	}
	.ia-snip-head {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 8px;
	}
	.ia-snip h4 {
		font-size: 13px;
		margin: 0 0 6px;
	}
	.ia-group {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		min-width: 0;
		gap: 10px;
		padding-top: 12px;
		border-top: 1px solid var(--line);
	}
	.ia-group h3 {
		font-size: 14px;
		margin: 0;
	}
	.ia-secret {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
	}
	.ia-secret code {
		padding: 4px 8px;
		background: var(--sunk);
		border: 1px solid var(--line);
		border-radius: var(--r);
		font-size: 12px;
		overflow-wrap: anywhere;
	}
	.ia-secret-err {
		color: var(--err);
		font-size: 13px;
	}
	.ia-snip pre {
		margin: 0;
		padding: 10px 12px;
		background: var(--sunk);
		border: 1px solid var(--line);
		border-radius: var(--r);
		overflow-x: auto;
		font: 12px/1.5 var(--mono);
	}
	.sr-only {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
		white-space: nowrap;
	}
	@media (max-width: 640px) {
		.ia-msg {
			max-width: 92%;
		}
		.ia-compose {
			flex-direction: column;
			align-items: stretch;
		}
		.ia-actions {
			justify-content: space-between;
		}
		.ia-mic {
			flex: 1;
		}
		.ia-chat {
			height: 70vh;
		}
	}
</style>
