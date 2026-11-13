<script lang="ts">
	import { ICON } from '../icons';
	import { PENDING_LABEL, formatTime, initials, type LineStatus } from '../looks';
	import { enterSends, stick, type ShellProps } from './types';

	let {
		lines,
		typing,
		agentName,
		customerName,
		text = $bindable(),
		locked,
		onsend,
		notice,
		answerFoot,
	}: ShellProps = $props();
	const last = $derived(lines.filter((l) => l.role !== 'system').at(-1));
	/** Read ticks: one grey tick while sending, two grey once stored, two blue once the agent has answered. */
	const TICK: Record<LineStatus, { icon: string; label: string }> = {
		read: { icon: ICON.checks, label: 'Read' },
		sent: { icon: ICON.checks, label: 'Delivered' },
		sending: { icon: ICON.check, label: 'Sending' },
		queued: { icon: ICON.clock, label: 'Queued' },
		failed: { icon: ICON.alert, label: 'Not sent' },
		unknown: { icon: ICON.alert, label: 'Outcome unknown' },
	};
</script>

<div class="tg-host" data-look="telegram">
	<div class="tg">
		<aside class="tg-list" aria-label="Conversations">
			<header class="tg-list-head">
				<div class="tg-me" aria-hidden="true">{initials(customerName)}</div>
				<svg class="tg-ic" viewBox="0 0 24 24" aria-hidden="true"><path d={ICON.more} /></svg>
			</header>
			<div class="tg-search" aria-hidden="true">
				<svg viewBox="0 0 24 24"><path d={ICON.search} /></svg><span>Search</span>
			</div>
			<div class="tg-row on" aria-current="true">
				<div class="tg-ava" aria-hidden="true">{initials(agentName)}</div>
				<div class="tg-row-body">
					<div class="tg-row-top">
						<span class="tg-row-name">{agentName}</span><span class="tg-row-time"
							>{formatTime(last?.at ?? null)}</span
						>
					</div>
					<div class="tg-row-prev">{typing ? 'typing…' : (last?.text ?? 'Start the conversation')}</div>
				</div>
			</div>
		</aside>

		<section class="tg-conv" aria-label="Conversation with {agentName}">
			<header class="tg-head">
				<div class="tg-ava sm" aria-hidden="true">{initials(agentName)}</div>
				<div class="tg-head-meta">
					<span class="tg-head-name">{agentName}</span>
					<span class="tg-head-sub">{typing ? 'typing…' : 'bot'}</span>
				</div>
				<svg class="tg-ic" viewBox="0 0 24 24" aria-hidden="true"><path d={ICON.search} /></svg>
				<svg class="tg-ic" viewBox="0 0 24 24" aria-hidden="true"><path d={ICON.more} /></svg>
			</header>
			<!-- svelte-ignore a11y_no_noninteractive_tabindex -->
			<div
				class="tg-log"
				role="log"
				aria-live="polite"
				aria-relevant="additions"
				tabindex="0"
				use:stick={lines.length + (typing ? 0.5 : 0)}
			>
				<div class="tg-banner">
					<svg viewBox="0 0 24 24" aria-hidden="true"><path d={ICON.lock} /></svg>
					Customers reach this bot on Telegram, the only customer channel in v1. Staff test it here.
				</div>
				{#each lines as m (m.key)}
					{#if m.role === 'system'}
						<div class="tg-sys" data-testid="chat-message" data-role="system">{m.text}</div>
					{:else}
						{@const you = m.role === 'customer'}
						<div
							class="tg-line"
							class:you
							class:failed={m.status === 'failed' || m.status === 'unknown'}
							data-testid="chat-message"
							data-role={m.role}
							data-status={m.status}
						>
							<div class="tg-bubble">
								<span class="tg-sr">{you ? customerName : agentName}:</span>
								<span class="tg-txt">{m.text}</span>
								<span class="tg-stamp">
									{#if m.at !== null}<time>{formatTime(m.at)}</time>{:else if m.status !== 'sending'}<span
											>{PENDING_LABEL[m.status as keyof typeof PENDING_LABEL]}</span
										>{/if}
									{#if you}
										<span
											class="tg-ticks"
											data-state={m.status}
											data-testid="chat-ticks"
											role="img"
											aria-label={TICK[m.status].label}
											><svg viewBox="0 0 24 24" aria-hidden="true"><path d={TICK[m.status].icon} /></svg
											></span
										>
									{/if}
								</span>
							</div>
						</div>
						{#if m.role === 'agent' && answerFoot}<div class="tg-foot">{@render answerFoot(m.key)}</div>{/if}
					{/if}
				{/each}
				{#if typing}
					<div class="tg-line" data-testid="chat-typing">
						<div class="tg-bubble tg-typing" aria-hidden="true"><i></i><i></i><i></i></div>
					</div>
				{/if}
			</div>
			<div class="tg-sr" role="status">{typing ? `${agentName} is typing…` : ''}</div>
			{#if notice}<div class="tg-notice">{@render notice()}</div>{/if}
			<form
				class="tg-compose"
				onsubmit={(e) => {
					e.preventDefault();
					onsend();
				}}
			>
				<textarea
					bind:value={text}
					rows="1"
					placeholder="Message"
					aria-label="Message"
					onkeydown={(e) => enterSends(e, onsend)}
				></textarea>
				<button class="tg-send" type="submit" disabled={locked || !text.trim()} aria-label="Send">
					<svg viewBox="0 0 24 24" aria-hidden="true"><path d={ICON.send} /></svg>
				</button>
			</form>
		</section>
	</div>
</div>

<style>
	/* Sources + feedback under an agent bubble, on the app's own panel so it reads on any wallpaper. */
	.tg-foot {
		align-self: flex-start;
		max-width: min(85%, 520px);
		min-width: 0;
		margin: -2px 0 4px 8px;
		padding: 4px 8px;
		border-radius: 8px;
		background: var(--panel);
		color: var(--fg);
	}
	.tg-host {
		/* Telegram-style palette (generic: no marks or names of the app): light, dark via color-scheme. */
		--tg-bg: light-dark(#ffffff, #0e1621);
		--tg-fg: light-dark(#000000, #f5f5f5);
		--tg-panel: light-dark(#ffffff, #17212b);
		--tg-bar: light-dark(#ffffff, #17212b);
		--tg-line: light-dark(#dfe1e5, #101921);
		--tg-icon: light-dark(#707579, #6c7883);
		--tg-muted: light-dark(#707579, #6c7883);
		--tg-search: light-dark(#f1f1f1, #242f3d);
		--tg-row-on: light-dark(#419fd9, #2b5278);
		--tg-brand: light-dark(#3390ec, #5eb5f7);
		--tg-ava: linear-gradient(135deg, #72d5fd, #2a9ef1);
		--tg-wall: light-dark(#a7c48a, #0e1621);
		--tg-wall-dot: light-dark(rgba(255, 255, 255, 0.18), rgba(255, 255, 255, 0.025));
		--tg-banner: light-dark(rgba(0, 0, 0, 0.22), rgba(0, 0, 0, 0.35));
		--tg-banner-fg: light-dark(#ffffff, #f5f5f5);
		--tg-in: light-dark(#ffffff, #182533);
		--tg-out: light-dark(#eeffde, #2b5278);
		--tg-input: light-dark(#ffffff, #17212b);
		--tg-tick: light-dark(#4fae4e, #6ab3f3);
		--tg-tick-read: light-dark(#4fae4e, #6ab3f3);
		--tg-typing: light-dark(#3390ec, #6ab3f3);
		--tg-err: light-dark(#b3261e, #f2b8b5);

		container: tg / inline-size;
		height: 100%;
	}
	.tg {
		display: grid;
		grid-template-columns: 340px minmax(0, 1fr);
		grid-template-rows: minmax(0, 1fr);
		height: 100%;
		background: var(--tg-bg);
		color: var(--tg-fg);
		font-family: -apple-system, 'Roboto', 'Helvetica Neue', system-ui, sans-serif;
		font-size: 14.5px;
	}
	.tg-ic {
		width: 20px;
		height: 20px;
		fill: var(--tg-icon);
		flex-shrink: 0;
	}
	.tg-list {
		background: var(--tg-panel);
		border-right: 1px solid var(--tg-line);
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.tg-list-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		background: var(--tg-bar);
		padding: 10px 16px;
	}
	.tg-me {
		width: 40px;
		height: 40px;
		border-radius: 50%;
		background: var(--tg-brand);
		color: #fff;
		display: flex;
		align-items: center;
		justify-content: center;
		font-weight: 700;
	}
	.tg-search {
		display: flex;
		align-items: center;
		gap: 10px;
		margin: 8px 12px;
		padding: 7px 12px;
		background: var(--tg-search);
		border-radius: 8px;
		color: var(--tg-muted);
		font-size: 14px;
	}
	.tg-search svg {
		width: 15px;
		height: 15px;
		fill: currentColor;
	}
	.tg-row {
		display: flex;
		gap: 13px;
		align-items: center;
		padding: 11px 16px;
		border-bottom: 1px solid var(--tg-line);
	}
	.tg-row.on {
		background: var(--tg-row-on);
		color: #fff;
	}
	.tg-row.on .tg-row-time,
	.tg-row.on .tg-row-prev {
		color: rgba(255, 255, 255, 0.85);
	}
	.tg-ava {
		width: 48px;
		height: 48px;
		border-radius: 50%;
		flex-shrink: 0;
		display: flex;
		align-items: center;
		justify-content: center;
		color: #fff;
		font-weight: 700;
		font-size: 15px;
		background: var(--tg-ava);
	}
	.tg-ava.sm {
		width: 40px;
		height: 40px;
		font-size: 13px;
	}
	.tg-row-body {
		flex: 1;
		min-width: 0;
	}
	.tg-row-top {
		display: flex;
		justify-content: space-between;
		align-items: baseline;
		gap: 8px;
	}
	.tg-row-name {
		font-size: 16px;
		font-weight: 500;
	}
	.tg-row-time,
	.tg-row-prev {
		font-size: 12px;
		color: var(--tg-muted);
	}
	.tg-row-prev {
		font-size: 14px;
		white-space: nowrap;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.tg-conv {
		display: flex;
		flex-direction: column;
		min-width: 0;
		min-height: 0;
	}
	.tg-head {
		display: flex;
		align-items: center;
		gap: 14px;
		background: var(--tg-bar);
		padding: 9px 18px;
	}
	.tg-head-meta {
		flex: 1;
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.tg-head-name {
		font-size: 16px;
		font-weight: 500;
	}
	.tg-head-sub {
		font-size: 12.5px;
		color: var(--tg-muted);
	}
	.tg-log {
		flex: 1;
		min-height: 0;
		overflow-y: auto;
		padding: 18px 7% 12px;
		display: flex;
		flex-direction: column;
		gap: 4px;
		background-color: var(--tg-wall);
		background-image: radial-gradient(circle at 20% 30%, var(--tg-wall-dot) 0 8px, transparent 9px),
			radial-gradient(circle at 70% 60%, var(--tg-wall-dot) 0 6px, transparent 7px),
			radial-gradient(circle at 45% 85%, var(--tg-wall-dot) 0 10px, transparent 11px),
			radial-gradient(circle at 90% 20%, var(--tg-wall-dot) 0 7px, transparent 8px);
		background-size: 280px 280px;
	}
	.tg-banner {
		align-self: center;
		display: flex;
		gap: 6px;
		align-items: flex-start;
		background: var(--tg-banner);
		color: var(--tg-banner-fg);
		font-size: 12.5px;
		text-align: center;
		padding: 6px 12px;
		border-radius: 8px;
		margin-bottom: 8px;
		max-width: 90%;
		box-shadow: 0 1px 0.5px rgba(0, 0, 0, 0.13);
	}
	.tg-banner svg {
		width: 12px;
		height: 12px;
		fill: currentColor;
		flex-shrink: 0;
		margin-top: 3px;
	}
	.tg-sys {
		align-self: center;
		background: var(--tg-in);
		color: var(--tg-muted);
		font-size: 12.5px;
		padding: 5px 12px;
		border-radius: 8px;
		margin: 4px 0;
	}
	.tg-line {
		display: flex;
	}
	.tg-line.you {
		justify-content: flex-end;
	}
	.tg-bubble {
		position: relative;
		max-width: 65%;
		padding: 6px 9px 8px 11px;
		border-radius: 14px;
		line-height: 1.32;
		box-shadow: 0 1px 0.5px rgba(0, 0, 0, 0.13);
		overflow-wrap: anywhere;
		background: var(--tg-in);
		border-top-left-radius: 0;
	}
	.tg-line.you .tg-bubble {
		background: var(--tg-out);
		border-top-left-radius: 8px;
		border-top-right-radius: 0;
	}
	.tg-bubble::before {
		content: '';
		position: absolute;
		top: 0;
		width: 8px;
		height: 13px;
		left: -8px;
		background: radial-gradient(circle at top left, transparent 8px, var(--tg-in) 8px) top left / 8px 13px
			no-repeat;
	}
	.tg-line.you .tg-bubble::before {
		left: auto;
		right: -8px;
		background: radial-gradient(circle at top right, transparent 8px, var(--tg-out) 8px) top right / 8px 13px
			no-repeat;
	}
	.tg-txt {
		white-space: pre-wrap;
	}
	.tg-stamp {
		float: right;
		margin: 6px 0 -3px 10px;
		font-size: 11px;
		color: var(--tg-muted);
		display: inline-flex;
		align-items: center;
		gap: 3px;
	}
	.tg-line.failed .tg-stamp {
		color: var(--tg-err);
	}
	.tg-ticks {
		display: inline-flex;
		color: var(--tg-tick);
	}
	.tg-ticks svg {
		width: 16px;
		height: 16px;
		fill: currentColor;
	}
	.tg-ticks[data-state='read'] {
		color: var(--tg-tick-read);
	}
	.tg-ticks[data-state='failed'],
	.tg-ticks[data-state='unknown'] {
		color: var(--tg-err);
	}
	.tg-typing {
		display: flex;
		gap: 4px;
		padding: 11px 14px;
	}
	.tg-typing i {
		width: 7px;
		height: 7px;
		border-radius: 50%;
		background: var(--tg-typing);
		animation: tg-blink 1.3s infinite ease-in-out;
	}
	.tg-typing i:nth-child(2) {
		animation-delay: 0.18s;
	}
	.tg-typing i:nth-child(3) {
		animation-delay: 0.36s;
	}
	@keyframes tg-blink {
		0%,
		60%,
		100% {
			opacity: 0.4;
		}
		30% {
			opacity: 1;
		}
	}
	.tg-notice {
		padding: 8px 16px 0;
		background: var(--tg-bar);
	}
	.tg-compose {
		display: flex;
		align-items: flex-end;
		gap: 10px;
		background: var(--tg-bar);
		padding: 8px 16px;
	}
	.tg-compose textarea {
		flex: 1;
		min-width: 0;
		border: 0;
		outline: none;
		background: var(--tg-input);
		border-radius: 8px;
		padding: 11px 14px;
		font-size: 15px;
		color: var(--tg-fg);
		resize: none;
		line-height: 1.4;
		max-height: 140px;
		field-sizing: content;
	}
	.tg-compose textarea:focus-visible {
		outline: 2px solid var(--tg-brand);
		outline-offset: 0;
	}
	.tg-send {
		width: 42px;
		height: 42px;
		border-radius: 50%;
		border: 0;
		cursor: pointer;
		background: var(--tg-brand);
		color: #fff;
		display: flex;
		align-items: center;
		justify-content: center;
		flex-shrink: 0;
	}
	.tg-send svg {
		width: 20px;
		height: 20px;
		fill: currentColor;
	}
	.tg-send:disabled {
		background: none;
		color: var(--tg-icon);
		opacity: 0.6;
		cursor: default;
	}
	.tg-sr {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip-path: inset(50%);
		white-space: nowrap;
	}
	@container tg (max-width: 720px) {
		.tg {
			grid-template-columns: minmax(0, 1fr);
		}
		.tg-list {
			display: none;
		}
		.tg-head {
			padding: 9px 12px;
			gap: 10px;
		}
		.tg-log {
			padding-inline: 12px;
		}
		.tg-bubble {
			max-width: 85%;
		}
		.tg-compose,
		.tg-notice {
			padding-inline: 10px;
		}
	}
</style>
