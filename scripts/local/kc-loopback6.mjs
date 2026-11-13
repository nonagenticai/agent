#!/usr/bin/env node
// B4 local acceptance: `localhost` resolves to ::1 first and agent-api's connector client pins the FIRST address
// (netguard.rs pinned_client), but demo-backends binds 127.0.0.1 only. This relays [::1]:<port> -> 127.0.0.1:<port>
// so the allow-listed private target `localhost:8097` is reachable. Env: KC_RELAY_PORT (required).
import { createServer, connect } from 'node:net';
const PORT = Number(process.env.KC_RELAY_PORT ?? 0);
if (!PORT) throw new Error('KC_RELAY_PORT is required');
createServer((c) => {
	const u = connect(PORT, '127.0.0.1');
	c.pipe(u).pipe(c);
	const end = () => { c.destroy(); u.destroy(); };
	c.on('error', end); u.on('error', end);
}).listen(PORT, '::1', () => console.log(`kc-loopback6 [::1]:${PORT} -> 127.0.0.1:${PORT}`));
