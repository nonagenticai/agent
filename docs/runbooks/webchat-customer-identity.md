# Web chat: telling the platform who the logged-in customer is

For the tenant's web developers. Audience: the team that runs the site where the chat widget is embedded.

**The rule this exists for (owner ruling 2026-10-26).** The customer is KYC'd and known before the conversation
starts. The platform NEVER asks a customer for an order number, order id, customer id, postcode, email address or any
other lookup key. "Where is my parcel?" is answered from the customer's own records. If a customer has two candidate
orders, the agent names them in plain words ("the rug from 2 Oct or the mugs from 16 Oct?"). Your site proves who the
customer is once, silently, when the chat opens; the customer types nothing for it.

An unidentified visitor gets general help only and is offered a "Sign in" button (below). They are still never asked
for an id.

## How it works

1. The customer logs in on your site (your own login, your own KYC).
2. When the chat opens, the widget asks your page for a **customer assertion**: a short-lived signed JWT that says
   "this is customer `sub`".
3. Your server mints it (it holds the key). The browser only carries it.
4. The widget sends it once to open a session. The platform verifies it and loads that customer's orders, payments,
   deliveries and cases. The assertion is never stored by the widget (not in `sessionStorage`, not in `localStorage`).

A valid assertion is the platform's only evidence of KYC. Turning identity on for a binding is your statement that
every `sub` you sign passed KYC.

## One-time setup (tenant admin, per web chat binding)

Binding settings `identity` (set through the binding settings PUT in the console):

| setting | meaning |
|---|---|
| `mode` | `off` (default), `hs256` or `es256`. Pinned per binding; the token header is never trusted to pick it. |
| `issuer` | required; your site's origin or name. Must equal the `iss` claim exactly. |
| `session_hours` | 1..24, default 8: how long an identified chat session lives. |
| `sign_in_url` | optional; an `https` URL on an allowed origin of the binding, or a path starting with `/`. Drives the "Sign in" button. |
| `jwks` | ES256 only: 1 or 2 public P-256 JWKs, each with a unique `kid`. Public keys only (a `d` member is refused). |

### HS256 (shared secret, simplest)

`POST /api/jobs/{job}/channel-bindings/{id}/identity-secret` generates a secret and returns `{kid, secret}`
**once**. Store it in your server-side secret manager at once; the platform keeps it sealed and can never show it
again. The secret looks like `ids_<base64url>`; the HMAC key is that whole string as UTF-8 bytes (including the
`ids_` prefix). Put `kid` in the JWT header.

**Rotation is two-slot.** Calling the route again creates `k<N+1>` and keeps the old secret as `k<N>` for at most 24
hours (`prev_valid_until`); after that it no longer verifies anything and is purged on the binding's next credential
write. So: rotate, deploy the new `kid` + secret to your site, done; no
downtime. `?revoke_sessions=true` additionally ends every open identified session of the binding. Every change is
audited (never the value). Never share one secret between bindings.

### ES256 (you keep the private key)

Generate a P-256 key pair, keep the private key, and put the public JWK (with a `kid`) in `identity.jwks`. Two keys
may be listed during a rotation. The platform holds no secret to leak. Sign with ES256 and the same `kid`.

## The claims

JWT, compact serialization, at most 2048 bytes. Header: `alg` = `HS256` or `ES256` (must match the binding's mode;
`none` and any other value are refused) and `kid` (required).

| claim | rule |
|---|---|
| `iss` | required; equal to the binding's `identity.issuer`. |
| `aud` | required; the **binding id** (uuid string), exactly. A token for binding A is dead on binding B. |
| `sub` | required; the customer id **as the connector knows it**, i.e. the same id the order/payment system uses for this customer. 1..200 chars of `[A-Za-z0-9._:@-]`. |
| `iat` | required; not more than 30 s in the future. |
| `exp` | required; `exp - iat <= 300` seconds (30 s clock leeway). Mint one per chat open; do not cache. |
| `jti` | required; 16..64 printable characters, **single use** per binding. A random UUID/hex is right. |
| `email` | optional (<= 320). Becomes the case's verified email, so tickets carry the KYC'd address and never a model-supplied one. |
| `name` | optional (<= 200). Greeting only. |

Neither `email` nor `name` is logged. Do not put anything else in the token.

### Failure answers

The session-create call answers `401 {"error":"assertion_invalid"}` for any bad token (signature, algorithm, issuer,
audience, expiry, lifetime, claim shape, unknown `kid`; deliberately one answer, no hints) and
`401 {"error":"assertion_replayed"}` when the `jti` was already used. 20 refused assertions per client IP per 10
minutes, then `429`. A refused attempt for another reason (rate limit, origin) does not burn the `jti`.

## Minting the assertion

All samples: the endpoint lives on YOUR server, behind YOUR login, and returns `{"assertion": "<jwt>"}` for the
logged-in customer. Return `{"assertion": null}` (or 401) when nobody is logged in. Never mint for an id taken from
the request; only from your session.

### Node (`jose`)

```js
import { SignJWT } from 'jose';
import { randomUUID } from 'node:crypto';

const BINDING_ID = process.env.CHAT_BINDING_ID;             // the aud
const KID = process.env.CHAT_IDENTITY_KID;                  // e.g. "k3"
const KEY = new TextEncoder().encode(process.env.CHAT_IDENTITY_SECRET); // the full "ids_..." string

app.get('/chat/assertion', requireLogin, async (req, res) => {
  const c = req.session.customer;                           // from YOUR login
  const now = Math.floor(Date.now() / 1000);
  const assertion = await new SignJWT({ email: c.email, name: c.name })
    .setProtectedHeader({ alg: 'HS256', kid: KID })
    .setIssuer('https://shop.example')                      // = identity.issuer
    .setAudience(BINDING_ID)
    .setSubject(c.id)                                       // the connector's customer id
    .setIssuedAt(now)
    .setExpirationTime(now + 120)
    .setJti(randomUUID().replaceAll('-', ''))
    .sign(KEY);
  res.set('Cache-Control', 'no-store').json({ assertion });
});
```

For ES256 use `alg: 'ES256'` and a key from `importPKCS8(privatePem, 'ES256')`.

### Python (`PyJWT`)

```python
import os, time, uuid
import jwt  # PyJWT

BINDING_ID = os.environ["CHAT_BINDING_ID"]
KID = os.environ["CHAT_IDENTITY_KID"]
SECRET = os.environ["CHAT_IDENTITY_SECRET"]  # the full "ids_..." string

def mint(customer) -> str:
    now = int(time.time())
    return jwt.encode(
        {
            "iss": "https://shop.example",       # = identity.issuer
            "aud": BINDING_ID,
            "sub": customer.id,                  # the connector's customer id
            "iat": now,
            "exp": now + 120,
            "jti": uuid.uuid4().hex,
            "email": customer.email,
            "name": customer.name,
        },
        SECRET,
        algorithm="HS256",
        headers={"kid": KID},
    )
```

For ES256: `algorithm="ES256"` and the private key PEM as the key.

### PHP (`firebase/php-jwt`)

```php
use Firebase\JWT\JWT;

function mintAssertion(array $customer): string {
    $now = time();
    return JWT::encode(
        [
            'iss'   => 'https://shop.example',          // = identity.issuer
            'aud'   => getenv('CHAT_BINDING_ID'),
            'sub'   => $customer['id'],                 // the connector's customer id
            'iat'   => $now,
            'exp'   => $now + 120,
            'jti'   => bin2hex(random_bytes(16)),
            'email' => $customer['email'],
            'name'  => $customer['name'],
        ],
        getenv('CHAT_IDENTITY_SECRET'),                 // the full "ids_..." string
        'HS256',
        getenv('CHAT_IDENTITY_KID')                     // sets the kid header
    );
}
```

For ES256 pass the private key PEM and `'ES256'`.

## The widget hook

Either of these, set **before** `widget.js` loads:

```html
<script>
  window.nonagenticChat = {
    // called once per session open; return the assertion, or null when nobody is logged in
    identity: () => fetch('/chat/assertion', { credentials: 'include' })
      .then((r) => (r.ok ? r.json() : { assertion: null }))
      .then((j) => j.assertion),
  };
</script>
<script src="https://chat.example/webchat/widget.js" data-widget="wk_..." async></script>
```

or, with no script of your own, `data-identity-url="/chat/assertion"` on the widget tag: the widget does a `GET` with
credentials and reads `{assertion}`.

**On logout** call `window.nonagenticChat.reset()`: the widget drops its session and conversation and, if the chat
is open, starts an anonymous one at once (your hook is not asked). **On login, or when the logged-in customer
switches**, call `window.nonagenticChat.reset({ identify: true })`: the same, but the new session asks your hook for
an assertion, so the chat continues as the new customer. With the chat closed either call only drops the session; the
next open asks your hook.

## Lifetime and failure behaviour

- **Assertion**: at most 5 minutes, usable once, only to open a session. A stolen assertion is worth one use.
- **Session**: an identified session ends `session_hours` (default 8, max 24) after it was identified, in addition to
  the normal 24 h sliding / 7 d cap. At expiry the widget asks your hook again and opens a new session (a new
  conversation, as for any session).
- **401 `assertion_*`**: the widget calls your hook once more and retries once. If there is no assertion or the retry
  fails, it opens an anonymous session (lowest privilege) and, when `sign_in_url` is set, shows a **Sign in** button.
  A bad token never produces a customer session.
- **Anonymous visitors**: general help (knowledge, FAQ) only. For anything about their own order the agent says, in
  words, that they need to sign in on your site and open the chat again, and the widget shows the Sign in button
  (the agent signals it with the `sign_in` event). It offers a colleague through the normal handover. It does not ask
  for ids, a postcode or an email, and no case is created for that request.
- Turnstile (if on) is skipped for a valid assertion: your site vouches for the visitor.

## What the platform will never do

- Ask the customer for an order number, order id, customer id, postcode, email address or any lookup key, in chat or
  in any demo or acceptance script.
- Accept an identity from the page itself. Only a verified assertion makes a customer known.
- Show or return the identity secret after the one-time reveal.

If you see the agent ask a customer for an identifier, that is a platform bug: report it with the conversation id.

## Testing without a real site (admin test session)

A tenant admin tries a channel as a known customer from the console, before publishing it. On the job's Channels page,
open the web chat panel of a binding that is still in **Test** with identity mode **HS256** and its secret made, type
the **Customer reference from your system** (the same value your site would put in `sub`; this is the admin naming a
customer, not a customer being asked for anything) and press **Try as this customer**. The test page opens signed in as
that customer. Without HS256 and a secret the same button is an anonymous visitor's test page, and the panel says so.

Under the hood: `POST /api/jobs/{job}/channel-bindings/{id}/test-session` with `{customer_ref}` (tenant admin or
integrations manager). Core signs an assertion exactly as your site would, with the binding's own identity secret
(read the way the session verifier reads it, never returned or logged): `aud` = the binding id, a fresh single-use
`jti`, 120 seconds, `iss` = the configured issuer, `sub` = the customer reference. It answers `{url}`, the edge's hosted
test page `GET /webchat/demo?key=wk_...#assertion=<jwt>`. The assertion is in the URL **fragment**, so it is never sent
to a server or logged; the page's same-origin script (`/webchat/demo.js`) hands it to the widget's identify hook once
and removes it from the address bar. Refusals: `409 binding_live` (only a Test binding can be tried: unpublish first),
`409 identity_not_configured` (identity off, or no secret made), `409 identity_mode_not_hs256` (core holds no private
ES256 key), `422 invalid_customer_ref`. Each use is audited as `channel_binding.test_session` (binding id and actor
only). The acceptance walk of this journey is `docs/runbooks/acceptance-tenant-admin-channels.md`.
