/* nonagentic web chat widget. No dependencies.
 * Embed: <script src="https://chat.nonagentic.ai/webchat/widget.js" data-widget="wk_..." async></script>
 * (`data-widget-key` and `data-key` are read too.)
 * The conversation lives in this tab's sessionStorage; each message sends the recent turns.
 * B13: before the first message the widget opens a session (POST /api/webchat/sessions) and sends its token as a
 * Bearer; the answer also carries the widget's display (title, greeting, colour). A message refused with 503 or lost
 * to the network is resent with the SAME message_id, so the edge and agent-api dedupe it.
 * B14: a binding with Turnstile answers the session POST with 428 {site_key}; the widget then loads Cloudflare's
 * api.js once, renders the challenge (action `webchat`) in the panel and retries with the token.
 * With a stored conversation (B12: the edge answered JSON with conversation_id + token), later messages (a person's
 * replies, notices, the CSAT question) arrive over an EventSource on /api/webchat/conversations/{id}/events.
 * B7 known customer: a tenant's logged-in site hands the widget a short-lived signed assertion for the signed-in
 * customer, either from `window.nonagenticChat.identity()` (a function returning a promise of the assertion string or
 * null, set BEFORE this script) or from `data-identity-url` (GET with cookies, answers {assertion}). It is asked for
 * once per session open and sent in the session POST; it is never stored (not in sessionStorage, not in the state).
 * A refused assertion (401 assertion_*) is asked for once more; a second refusal opens an anonymous session and shows
 * a "Sign in" button (the binding's sign_in_url). `window.nonagenticChat.reset({identify})` drops the session and the
 * conversation and opens a fresh anonymous one (`identify: true` asks the host again); a `sign_in` event from the
 * agent shows the same button.
 * Message text is only ever set with textContent: answers render a small markdown subset (paragraphs, lists, **bold**,
 * *italic*, `code`, https links) built from DOM nodes, never parsed as HTML.
 * SW-9 attachments: as soon as a session exists a paperclip attaches up to 3 photos or PDFs. Before the conversation
 * exists (the customer's first message creates it) a picked file waits as a chip; the first message goes out as typed,
 * then the waiting files upload into the new conversation and follow as a message of their own (a photo sent first
 * thing needs no extra step). Once the conversation exists they attach to the next message. If the admin turns customer
 * files off, the paperclip disappears at the first refusal (422 attachments_off) and, after a reload, from the history
 * read. A HEIC or large photo is converted to JPEG (canvas, 4096 px, quality 0.85) when the browser can decode it, else sent as
 * is (the server re-encodes and refuses what it cannot). Files go to POST /api/webchat/attachments with the session as
 * a Bearer (progress, cancel), the message carries their ids. Received and sent files are fetched with the same proof
 * into blob: URLs (no token ever sits in an <img src>): images show as thumbnails that open the full image, a PDF as a
 * chip that downloads. Bytes are never stored by the widget. */
(function () {
  "use strict";

  var DEFAULT_BASE = "__WEBCHAT_PUBLIC_URL__";
  var SEND_LAST = 20;
  var KEEP = 40;
  var MAX_CHARS = 2000;

  var script = document.currentScript || document.querySelector('script[src*="/webchat/widget.js"]');
  if (!script) return;
  var key =
    script.getAttribute("data-widget") || script.getAttribute("data-widget-key") || script.getAttribute("data-key") || "";
  if (!/^wk_[a-z2-7]{24}$/.test(key)) {
    if (window.console) console.warn("web chat: missing or malformed data-widget");
    return;
  }
  if (document.querySelector('[data-nonagentic-webchat="' + key + '"]')) return;

  var base = DEFAULT_BASE;
  try {
    if (script.src) base = new URL(script.src, window.location.href).origin;
  } catch (e) {
    base = DEFAULT_BASE;
  }
  var API = base + "/api/webchat/messages";
  var SESSIONS = base + "/api/webchat/sessions";
  var TURNSTILE_JS = "https://challenges.cloudflare.com/turnstile/v0/api.js?render=explicit";
  var STORE = "nonagentic-webchat:" + key;
  var IDENTITY_URL = script.getAttribute("data-identity-url") || "";
  var MAX_ASSERTION = 2048;
  var SIGN_IN_TEXT = "Please sign in to continue.";
  var ATTACH_URL = base + "/api/webchat/attachments";
  var ACCEPT = "image/jpeg,image/png,image/webp,application/pdf,image/heic,image/heif";
  var MAX_FILES = 3;
  var MAX_FILE_BYTES = 10 * 1024 * 1024;
  var CONVERT_ABOVE = 2 * 1024 * 1024;
  var MAX_SIDE = 4096;
  var JPEG_QUALITY = 0.85;
  var SHOWN_TYPES = { "image/jpeg": 1, "image/png": 1, "image/webp": 1, "application/pdf": 1 };
  var REFUSALS = {
    unsupported_type: "That file type is not supported. Please send a photo (JPEG, PNG or WebP) or a PDF.",
    too_large: "That file is too big. The limit is 10 MB.",
    payload_too_large: "That file is too big. The limit is 10 MB.",
    too_many_pixels: "That image is too large. Please send a smaller photo.",
    animated: "Animated images are not supported. Please send a still photo.",
    malformed_image: "We could not read that image. Please try another one.",
    malformed_pdf: "We could not read that PDF. Please try another one.",
    active_pdf: "That PDF cannot be accepted because it contains active content. Please send a plain PDF or a photo.",
    encrypted_pdf: "That PDF is password protected. Please remove the password and try again.",
    attachments_off: "Photos and PDFs are turned off for this chat. Please type your message instead.",
    quota: "We cannot take more files right now. Please try again later.",
    attachment_quota: "We cannot take more files right now. Please try again later.",
    malware: "This file can't be accepted.",
    scan_unavailable: "We can't accept files right now, please try again later.",
    storage_unavailable: "We can't accept files right now, please try again later.",
    rate_limited: "You have sent a lot of files. Please wait a while and try again."
  };

  // ---- state ----------------------------------------------------------------------------------------------------
  function randomId() {
    var c = window.crypto;
    if (c && typeof c.randomUUID === "function") return c.randomUUID().replace(/-/g, "");
    var bytes = new Uint8Array(16);
    if (c && c.getRandomValues) {
      c.getRandomValues(bytes);
    } else {
      for (var i = 0; i < 16; i++) bytes[i] = Math.floor(Math.random() * 256);
    }
    var out = "";
    for (var j = 0; j < bytes.length; j++) out += (bytes[j] + 256).toString(16).slice(1);
    return out;
  }

  function load() {
    try {
      var raw = window.sessionStorage.getItem(STORE);
      if (raw) {
        var s = JSON.parse(raw);
        if (s && typeof s.visitor === "string" && /^[A-Za-z0-9_-]{8,64}$/.test(s.visitor) && Array.isArray(s.messages)) {
          var msgs = s.messages.filter(function (m) {
            return m && (m.role === "user" || m.role === "assistant") && typeof m.text === "string";
          });
          var conv = typeof s.conv === "string" && /^wc_[0-9a-f]{32}$/.test(s.conv) ? s.conv : null;
          var token = conv && typeof s.token === "string" ? s.token : null;
          var session = typeof s.session === "string" && s.session.length <= 256 ? s.session : null;
          var pending =
            s.pending && typeof s.pending.text === "string" && typeof s.pending.id === "string" ? s.pending : null;
          return {
            visitor: s.visitor,
            messages: msgs.slice(-KEEP),
            open: !!s.open,
            conv: conv,
            token: token,
            session: session,
            display: cleanDisplay(s.display),
            csat: s.csat !== false,
            attach: s.attach !== false,
            pending: pending,
            identified: s.identified === true,
            name: null,
            signIn: typeof s.signIn === "string" && s.signIn.length <= 2048 ? s.signIn : null,
            needSignIn: s.needSignIn === true
          };
        }
      }
    } catch (e) {
      /* storage blocked or corrupt: start fresh */
    }
    return {
      visitor: randomId(),
      messages: [],
      open: false,
      conv: null,
      token: null,
      session: null,
      display: {},
      csat: true,
      attach: true,
      pending: null,
      identified: false,
      name: null,
      signIn: null,
      needSignIn: false
    };
  }

  /* The customer's first name for the greeting: text only, <= 40 chars. */
  function cleanName(n) {
    if (typeof n !== "string") return null;
    var first = n.trim().split(/\s+/)[0] || "";
    return first ? first.slice(0, 40) : null;
  }

  /* The sign-in link: https (or http on localhost), a relative URL resolved against the HOST page (the sign-in is the
     host shop's, not the edge's); else null. */
  function signInHref(raw) {
    if (typeof raw !== "string" || !raw || raw.length > 2048) return null;
    try {
      var u = new URL(raw, window.location.href);
      if (u.protocol === "https:") return u.href;
      if (u.protocol === "http:" && (u.hostname === "localhost" || u.hostname === "127.0.0.1")) return u.href;
    } catch (e) {
      /* malformed */
    }
    return null;
  }

  /* Only known fields of the expected shape: title <= 60, greeting <= 280, colour #rrggbb. */
  function cleanDisplay(d) {
    var out = {};
    if (!d || typeof d !== "object") return out;
    if (typeof d.title === "string" && d.title.trim()) out.title = d.title.trim().slice(0, 60);
    if (typeof d.greeting === "string" && d.greeting.trim()) out.greeting = d.greeting.trim().slice(0, 280);
    var c = typeof d.colour === "string" ? d.colour : d.color;
    if (typeof c === "string" && /^#[0-9a-fA-F]{6}$/.test(c)) out.colour = c;
    return out;
  }

  var state = load();

  function save() {
    state.messages = state.messages.slice(-KEEP);
    try {
      /* the customer's name is kept in memory only, never in sessionStorage */
      var stored = {};
      for (var k in state) {
        if (Object.prototype.hasOwnProperty.call(state, k) && k !== "name") stored[k] = state[k];
      }
      window.sessionStorage.setItem(STORE, JSON.stringify(stored));
    } catch (e) {
      /* storage unavailable: the conversation lives in memory only */
    }
  }

  // ---- DOM ------------------------------------------------------------------------------------------------------
  var CSS =
    ":host{all:initial}" +
    "*{box-sizing:border-box;font-family:system-ui,-apple-system,'Segoe UI',Roboto,sans-serif}" +
    ".root{--bg:#fff;--fg:#1d2330;--muted:#5b6475;--line:#dfe3ea;--accent:#2f5bea;--accent-fg:#fff;--me:#2f5bea;--me-fg:#fff;--them:#eef1f6;--err:#b42318}" +
    "@media (prefers-color-scheme:dark){.root{--bg:#1a1e25;--fg:#e6e8ec;--muted:#9aa3b2;--line:#2c323c;--accent:#6b8cff;--accent-fg:#0d1020;--me:#6b8cff;--me-fg:#0d1020;--them:#262b34;--err:#ff8a80}}" +
    ".launcher{position:fixed;right:20px;bottom:20px;z-index:2147483646;width:56px;height:56px;border-radius:50%;border:0;cursor:pointer;background:var(--accent);color:var(--accent-fg);box-shadow:0 4px 16px rgba(0,0,0,.25);display:flex;align-items:center;justify-content:center}" +
    ".launcher:focus-visible,button:focus-visible,textarea:focus-visible{outline:3px solid var(--accent);outline-offset:2px}" +
    ".launcher svg{width:26px;height:26px;fill:currentColor}" +
    ".panel{position:fixed;right:20px;bottom:88px;z-index:2147483647;width:min(380px,calc(100vw - 32px));height:min(560px,calc(100vh - 120px));display:flex;flex-direction:column;background:var(--bg);color:var(--fg);border:1px solid var(--line);border-radius:14px;box-shadow:0 12px 40px rgba(0,0,0,.25);overflow:hidden}" +
    ".panel[hidden]{display:none}" +
    "@media (max-width:480px){.panel{right:16px;left:16px;width:auto;bottom:84px}}" +
    ".head{display:flex;align-items:center;justify-content:space-between;padding:12px 14px;border-bottom:1px solid var(--line);font-weight:600;font-size:15px}" +
    ".close{border:0;background:transparent;color:var(--muted);cursor:pointer;font-size:22px;line-height:1;padding:2px 6px;border-radius:6px}" +
    ".list{flex:1;overflow-y:auto;padding:14px;display:flex;flex-direction:column;gap:8px;font-size:14px;line-height:1.45}" +
    ".msg{max-width:85%;padding:8px 11px;border-radius:12px;white-space:pre-wrap;overflow-wrap:anywhere}" +
    ".md{white-space:normal}.md p,.md ul,.md ol{margin:0}.md *+p,.md *+ul,.md *+ol{margin-top:6px}.md ul,.md ol{padding-left:18px}" +
    ".md code{font-family:ui-monospace,monospace;font-size:.92em;padding:0 3px;border-radius:4px;background:rgba(127,127,127,.18)}.md a{color:inherit}" +
    ".user{align-self:flex-end;background:var(--me);color:var(--me-fg);border-bottom-right-radius:4px}" +
    ".assistant{align-self:flex-start;background:var(--them);color:var(--fg);border-bottom-left-radius:4px}" +
    ".who{align-self:flex-start;margin:4px 0 -4px 4px;color:var(--muted);font-size:12px}" +
    ".empty{color:var(--muted);font-size:13px;text-align:center;margin:auto 0}" +
    ".typing{align-self:flex-start;display:flex;gap:4px;padding:10px 12px;background:var(--them);border-radius:12px}" +
    ".typing[hidden]{display:none}" +
    ".typing span{width:6px;height:6px;border-radius:50%;background:var(--muted);animation:b 1.2s infinite ease-in-out}" +
    ".typing span:nth-child(2){animation-delay:.15s}.typing span:nth-child(3){animation-delay:.3s}" +
    "@keyframes b{0%,80%,100%{opacity:.3;transform:translateY(0)}40%{opacity:1;transform:translateY(-3px)}}" +
    "@media (prefers-reduced-motion:reduce){.typing span{animation:none}}" +
    ".status{min-height:0;padding:0 14px;font-size:12.5px;color:var(--err)}" +
    ".status:not(:empty){padding:6px 14px}" +
    ".ts{display:flex;justify-content:center;font-size:12.5px;color:var(--muted)}.ts:not(:empty){padding:6px 14px}" +
    ".form{display:flex;gap:8px;padding:10px;border-top:1px solid var(--line)}" +
    "textarea{flex:1;resize:none;min-height:40px;max-height:120px;padding:9px 10px;border:1px solid var(--line);border-radius:10px;background:var(--bg);color:var(--fg);font-size:14px;line-height:1.4}" +
    ".send{border:0;border-radius:10px;padding:0 14px;background:var(--accent);color:var(--accent-fg);font-weight:600;font-size:14px;cursor:pointer}" +
    ".send:disabled{opacity:.5;cursor:default}" +
    ".csat{align-self:flex-start;display:flex;gap:6px}" +
    ".csat button{min-width:34px;height:34px;border:1px solid var(--line);border-radius:8px;background:var(--bg);color:var(--fg);cursor:pointer;font-size:14px}" +
    ".csat button:focus-visible{outline:3px solid var(--accent);outline-offset:2px}" +
    ".signin{display:flex;align-items:center;gap:10px;flex-wrap:wrap;padding:8px 14px;border-top:1px solid var(--line);font-size:13px;color:var(--fg)}" +
    ".signin[hidden]{display:none}" +
    ".signin a{display:inline-block;padding:6px 12px;border-radius:8px;background:var(--accent);color:var(--accent-fg);font-weight:600;text-decoration:none}" +
    ".attach{flex:none;width:40px;border:1px solid var(--line);border-radius:10px;background:var(--bg);color:var(--muted);cursor:pointer;display:flex;align-items:center;justify-content:center}" +
    ".attach[hidden]{display:none}" +
    ".attach:disabled{opacity:.45;cursor:default}.attach svg{width:20px;height:20px;fill:none;stroke:currentColor;stroke-width:2;stroke-linecap:round;stroke-linejoin:round}" +
    ".chips{list-style:none;margin:0;padding:6px 10px 0;display:flex;flex-wrap:wrap;gap:6px;border-top:1px solid var(--line)}.chips[hidden]{display:none}" +
    ".chip{display:flex;align-items:center;gap:6px;max-width:100%;padding:4px 6px 4px 10px;border:1px solid var(--line);border-radius:999px;font-size:12.5px;background:var(--them)}" +
    ".chip .name{overflow:hidden;text-overflow:ellipsis;white-space:nowrap;max-width:140px}.chip progress{width:48px;height:6px}" +
    ".chip button{border:0;background:transparent;color:var(--muted);cursor:pointer;font-size:16px;line-height:1;padding:2px 6px;border-radius:6px}" +
    ".files{display:flex;flex-wrap:wrap;gap:6px;margin-top:6px}" +
    ".thumb{padding:0;border:1px solid var(--line);border-radius:8px;background:var(--bg);cursor:pointer;overflow:hidden;line-height:0}.thumb img{display:block;max-width:160px;max-height:160px;object-fit:cover}" +
    ".thumb.wait{min-width:96px;min-height:64px;line-height:1.2;font-size:12px;color:var(--muted);padding:8px}" +
    ".filechip{display:inline-flex;align-items:center;gap:6px;border:1px solid var(--line);border-radius:8px;background:var(--bg);color:var(--fg);padding:6px 10px;font-size:13px;cursor:pointer}" +
    ".sr{position:absolute;width:1px;height:1px;margin:-1px;padding:0;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap;border:0}";

  function el(tag, cls, attrs) {
    var n = document.createElement(tag);
    if (cls) n.className = cls;
    if (attrs) {
      for (var k in attrs) {
        if (Object.prototype.hasOwnProperty.call(attrs, k)) n.setAttribute(k, attrs[k]);
      }
    }
    return n;
  }

  var host = el("div", null, { "data-nonagentic-webchat": key });
  var shadow = host.attachShadow ? host.attachShadow({ mode: "open" }) : host;
  var style = document.createElement("style");
  style.textContent = CSS;
  var root = el("div", "root");

  var panelId = "nwc-panel";
  var launcher = el("button", "launcher", {
    type: "button",
    "aria-label": "Open chat",
    "aria-expanded": "false",
    "aria-controls": panelId
  });
  var svgNs = "http://www.w3.org/2000/svg";
  var icon = document.createElementNS(svgNs, "svg");
  icon.setAttribute("viewBox", "0 0 24 24");
  icon.setAttribute("aria-hidden", "true");
  var path = document.createElementNS(svgNs, "path");
  path.setAttribute("d", "M4 4h16a2 2 0 0 1 2 2v10a2 2 0 0 1-2 2H9l-5 4v-4H4a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2z");
  icon.appendChild(path);
  launcher.appendChild(icon);

  var panel = el("div", "panel", { id: panelId, role: "dialog", "aria-label": "Chat with us", "aria-modal": "false" });
  panel.hidden = true;
  var head = el("div", "head");
  var title = el("span");
  title.textContent = "Chat with us";
  var closeBtn = el("button", "close", { type: "button", "aria-label": "Close chat" });
  closeBtn.textContent = "×";
  head.appendChild(title);
  head.appendChild(closeBtn);

  /* The list is rebuilt on every render, so it is not live itself; `live` announces each new message once. */
  var list = el("div", "list", { role: "log", "aria-live": "off", "aria-label": "Messages", tabindex: "0" });
  var live = el("div", "sr", { "aria-live": "polite", "aria-atomic": "true", "data-live": "" });
  /* Visually hidden even when the host page blocks the shadow <style> (CSSOM is not subject to style-src). */
  live.style.cssText =
    "position:absolute;width:1px;height:1px;margin:-1px;padding:0;overflow:hidden;clip:rect(0 0 0 0);white-space:nowrap;border:0";
  var typing = el("div", "typing", { role: "status", "aria-label": "The assistant is typing" });
  typing.appendChild(el("span"));
  typing.appendChild(el("span"));
  typing.appendChild(el("span"));
  typing.hidden = true;

  /* B7: the sign-in prompt, shown by a refused assertion or the agent's `sign_in` event. */
  var signBox = el("div", "signin");
  signBox.hidden = true;
  var signText = el("span");
  var signLink = el("a", null, { rel: "noopener" });
  signLink.textContent = "Sign in";
  signLink.hidden = true;
  signBox.appendChild(signText);
  signBox.appendChild(signLink);

  var status = el("div", "status", { role: "alert" });
  /* B14: the Turnstile challenge, when the binding asks for one. */
  var tsBox = el("div", "ts");
  var form = el("form", "form");
  var input = el("textarea", null, {
    rows: "1",
    "aria-label": "Message",
    placeholder: "Type a message",
    maxlength: String(MAX_CHARS)
  });
  var sendBtn = el("button", "send", { type: "submit", "aria-label": "Send message" });
  sendBtn.textContent = "Send";
  var attachBtn = el("button", "attach", { type: "button", "aria-label": "Attach a photo or PDF" });
  var clip = document.createElementNS(svgNs, "svg");
  clip.setAttribute("viewBox", "0 0 24 24");
  clip.setAttribute("aria-hidden", "true");
  var clipPath = document.createElementNS(svgNs, "path");
  clipPath.setAttribute(
    "d",
    "M21 11.5l-8.6 8.6a5.5 5.5 0 0 1-7.8-7.8l8.6-8.6a3.7 3.7 0 0 1 5.2 5.2l-8.6 8.6a1.8 1.8 0 0 1-2.6-2.6l7.9-7.9"
  );
  clip.appendChild(clipPath);
  attachBtn.appendChild(clip);
  var fileInput = el("input", null, { type: "file", accept: ACCEPT, multiple: "", tabindex: "-1", "aria-hidden": "true" });
  fileInput.hidden = true;
  var chipsBox = el("ul", "chips", { "aria-label": "Files to send" });
  chipsBox.hidden = true;
  form.appendChild(attachBtn);
  form.appendChild(fileInput);
  form.appendChild(input);
  form.appendChild(sendBtn);

  panel.appendChild(head);
  panel.appendChild(list);
  panel.appendChild(signBox);
  panel.appendChild(tsBox);
  panel.appendChild(status);
  panel.appendChild(chipsBox);
  panel.appendChild(form);
  panel.appendChild(live);
  root.appendChild(launcher);
  root.appendChild(panel);
  shadow.appendChild(style);
  shadow.appendChild(root);

  // ---- rendering ------------------------------------------------------------------------------------------------
  /* Markdown subset → DOM. Every piece of message text goes in through createTextNode; links are https only. */
  var INLINE =
    /\*\*([^\s*](?:[^*]*[^\s*])?)\*\*|`([^`\n]+)`|\[([^\]\n]+)\]\((https:\/\/[^\s()]+)\)|(https:\/\/[^\s<>()]*[^\s<>().,;:!?'"])|\*([^\s*](?:[^*]*[^\s*])?)\*/g;
  function safeHref(raw) {
    try {
      var u = new URL(raw);
      return u.protocol === "https:" ? u.href : null;
    } catch (e) {
      return null;
    }
  }
  function link(href, label, plain) {
    var a = el("a", null, { href: href, target: "_blank", rel: "noopener noreferrer" });
    if (plain) a.textContent = label;
    else inline(a, label);
    return a;
  }
  function inline(parent, s) {
    var re = new RegExp(INLINE.source, "g");
    var at = 0;
    var m, n, href;
    while ((m = re.exec(s))) {
      if (m.index > at) parent.appendChild(document.createTextNode(s.slice(at, m.index)));
      at = m.index + m[0].length;
      n = null;
      if (m[1] != null) {
        n = el("strong");
        inline(n, m[1]);
      } else if (m[2] != null) {
        n = el("code");
        n.textContent = m[2];
      } else if (m[3] != null) {
        href = safeHref(m[4]);
        if (href) n = link(href, m[3]);
      } else if (m[5] != null) {
        href = safeHref(m[5]);
        if (href) n = link(href, m[5], true);
      } else {
        n = el("em");
        inline(n, m[6]);
      }
      parent.appendChild(n || document.createTextNode(m[0]));
    }
    if (at < s.length) parent.appendChild(document.createTextNode(s.slice(at)));
  }
  function markdown(box, text) {
    while (box.firstChild) box.removeChild(box.firstChild);
    var lines = String(text).replace(/\r\n?/g, "\n").split("\n");
    var cur = null;
    var kind = "";
    for (var i = 0; i < lines.length; i++) {
      var line = lines[i];
      if (!line.trim()) {
        cur = null;
        continue;
      }
      var ul = /^\s*[-*\u2022]\s+(.*)$/.exec(line);
      var ol = ul ? null : /^\s*(\d{1,4})[.)]\s+(.*)$/.exec(line);
      if (ul || ol) {
        var k = ul ? "ul" : "ol";
        if (!cur || kind !== k) {
          cur = el(k);
          if (ol && ol[1] !== "1") cur.setAttribute("start", String(Number(ol[1])));
          box.appendChild(cur);
          kind = k;
        }
        var li = el("li");
        inline(li, ul ? ul[1] : ol[2]);
        cur.appendChild(li);
      } else {
        if (!cur || kind !== "p") {
          cur = el("p");
          box.appendChild(cur);
          kind = "p";
        } else {
          cur.appendChild(el("br"));
        }
        var h = /^\s*#{1,6}\s+(.*)$/.exec(line);
        if (h) {
          var st = el("strong");
          inline(st, h[1]);
          cur.appendChild(st);
        } else {
          inline(cur, line);
        }
      }
    }
  }

  function bubble(role, text) {
    var b = el("div", "msg " + (role === "user" ? "user" : "assistant md"));
    if (role === "user") b.textContent = text;
    else markdown(b, text);
    return b;
  }

  /* The CSAT answer: five buttons that send the digit as a message. */
  function csatButtons() {
    var row = el("div", "csat", { role: "group", "aria-label": "Rate this conversation from 1 to 5" });
    for (var n = 1; n <= 5; n++) {
      (function (digit) {
        var b = el("button", null, { type: "button", "aria-label": "Rate " + digit + " of 5", "data-csat": digit });
        b.textContent = digit;
        b.addEventListener("click", function () {
          if (busy) return;
          send(digit);
        });
        row.appendChild(b);
      })(String(n));
    }
    return row;
  }


  // ---- attachments (SW-9) ----------------------------------------------------------------------------------------
  /* Refs from the wire or the stored turns: only well-formed ones, only types the server stores. */
  function cleanRefs(raw) {
    var out = [];
    if (!Array.isArray(raw)) return out;
    for (var i = 0; i < raw.length && out.length < 5; i++) {
      var r = raw[i];
      if (!r || typeof r !== "object") continue;
      if (typeof r.id !== "string" || !/^[0-9a-f-]{36}$/.test(r.id)) continue;
      if (typeof r.media_type !== "string" || !SHOWN_TYPES[r.media_type]) continue;
      out.push({
        id: r.id,
        media_type: r.media_type,
        display_name: typeof r.display_name === "string" && r.display_name ? r.display_name.slice(0, 80) : "file",
        byte_size: typeof r.byte_size === "number" ? r.byte_size : 0
      });
    }
    return out;
  }

  /* Bytes are fetched with the session as a Bearer and shown from a blob: URL, so no token is ever in a src/href. */
  var blobs = {};
  function fileUrl(id) {
    return (
      base + "/api/webchat/conversations/" + state.conv + "/attachments/" + id + "?token=" + encodeURIComponent(state.token)
    );
  }
  function fetchBlob(ref) {
    if (blobs[ref.id]) return blobs[ref.id];
    if (!state.conv || !state.token || !state.session) return Promise.resolve(null);
    var p = fetch(fileUrl(ref.id), {
      mode: "cors",
      credentials: "omit",
      headers: { authorization: "Bearer " + state.session }
    })
      .then(function (res) {
        return res.ok ? res.blob() : null;
      })
      .then(function (blob) {
        if (!blob) {
          delete blobs[ref.id];
          return null;
        }
        return URL.createObjectURL(blob);
      })
      .catch(function () {
        delete blobs[ref.id];
        return null;
      });
    blobs[ref.id] = p;
    return p;
  }
  function dropBlobs() {
    var old = blobs;
    blobs = {};
    Object.keys(old).forEach(function (k) {
      old[k].then(function (u) {
        if (u) URL.revokeObjectURL(u);
      });
    });
  }

  function filesBox(refs, role) {
    var box = el("div", "files");
    refs.forEach(function (ref) {
      if (ref.media_type === "application/pdf") {
        var chip = el("button", "filechip", { type: "button", "aria-label": "Download " + ref.display_name });
        chip.textContent = "PDF " + ref.display_name;
        chip.addEventListener("click", function () {
          fetchBlob(ref).then(function (u) {
            if (!u) return setStatus("That file could not be opened. Please try again.");
            var a = el("a", null, { href: u, download: ref.display_name });
            shadow.appendChild(a);
            a.click();
            shadow.removeChild(a);
          });
        });
        box.appendChild(chip);
        return;
      }
      var thumb = el("button", "thumb wait", { type: "button", "aria-label": "Open image " + ref.display_name });
      thumb.textContent = "Loading image";
      var url = null;
      fetchBlob(ref).then(function (u) {
        if (!u) {
          thumb.textContent = "Image unavailable";
          return;
        }
        url = u;
        var img = el("img", null, { alt: ref.display_name });
        img.src = u;
        thumb.className = "thumb";
        thumb.textContent = "";
        thumb.appendChild(img);
      });
      thumb.addEventListener("click", function () {
        if (url) window.open(url, "_blank", "noopener");
      });
      box.appendChild(thumb);
    });
    return box;
  }

  /* The files waiting for the next message (memory only): {name, status: preparing|uploading|ready, progress, ref}. */
  var chips = [];
  function renderChips() {
    while (chipsBox.firstChild) chipsBox.removeChild(chipsBox.firstChild);
    chips.forEach(function (c) {
      var li = el("li", "chip");
      var name = el("span", "name");
      name.textContent = c.name;
      li.appendChild(name);
      if (c.status !== "ready" && c.status !== "queued") {
        var pr = el("progress", null, { max: "100", "aria-label": "Uploading " + c.name });
        if (c.status === "uploading") pr.value = c.progress;
        li.appendChild(pr);
      }
      var x = el("button", null, {
        type: "button",
        "aria-label": (c.status === "ready" ? "Remove " : "Cancel upload of ") + c.name
      });
      x.textContent = "×";
      x.addEventListener("click", function () {
        c.cancelled = true;
        if (c.xhr) c.xhr.abort();
        chips = chips.filter(function (o) {
          return o !== c;
        });
        renderChips();
      });
      li.appendChild(x);
      chipsBox.appendChild(li);
    });
    chipsBox.hidden = chips.length === 0;
    attachBtn.hidden = state.attach === false;
    /* enabled as soon as a session exists; a file picked before the conversation does is queued (addFiles) */
    attachBtn.disabled = !state.session;
    attachBtn.title = attachBtn.disabled ? "You can attach files in a moment" : "";
  }

  function refusalText(status, body) {
    var e = body && body.error;
    var key = e && (e.reason || e.code);
    if (key && REFUSALS[key]) return REFUSALS[key];
    if (status === 429) return REFUSALS.rate_limited;
    if (status === 413) return REFUSALS.too_large;
    if (status === 401 || status === 403) return "Your chat session has ended. Please send a new message to continue.";
    if (status === 0 || status === 503) return "We could not reach the chat. Please try again.";
    return "That file could not be sent. Please try again.";
  }

  function typeOk(f) {
    if (f.type) return ACCEPT.indexOf(f.type.toLowerCase()) >= 0;
    return /\.(jpe?g|png|webp|pdf|heic|heif)$/i.test(f.name || "");
  }

  /* HEIC and big photos become a JPEG of at most 4096 px when this browser can decode them; else the file as is. */
  async function prepare(file) {
    var t = (file.type || "").toLowerCase();
    var heic = t === "image/heic" || t === "image/heif" || /\.(heic|heif)$/i.test(file.name || "");
    if (t === "application/pdf" || (!heic && (t.indexOf("image/") !== 0 || file.size <= CONVERT_ABOVE))) return file;
    try {
      var bmp = null;
      if (typeof window.createImageBitmap === "function") bmp = await window.createImageBitmap(file);
      if (!bmp) return file;
      var scale = Math.min(1, MAX_SIDE / Math.max(bmp.width, bmp.height));
      var canvas = document.createElement("canvas");
      canvas.width = Math.max(1, Math.round(bmp.width * scale));
      canvas.height = Math.max(1, Math.round(bmp.height * scale));
      var ctx = canvas.getContext("2d");
      if (!ctx) return file;
      ctx.drawImage(bmp, 0, 0, canvas.width, canvas.height);
      if (typeof bmp.close === "function") bmp.close();
      var blob = await new Promise(function (resolve) {
        canvas.toBlob(resolve, "image/jpeg", JPEG_QUALITY);
      });
      if (!blob || (!heic && blob.size >= file.size)) return file;
      var base_ = (file.name || "photo").replace(/\.[^.]*$/, "") || "photo";
      return new File([blob], base_ + ".jpg", { type: "image/jpeg" });
    } catch (e) {
      return file;
    }
  }

  function uploadFile(chip, file) {
    return new Promise(function (resolve) {
      var xhr = new XMLHttpRequest();
      chip.xhr = xhr;
      var fd = new FormData();
      fd.append("conversation", state.conv);
      fd.append("token", state.token);
      fd.append("file", file, file.name || "file");
      xhr.open("POST", ATTACH_URL);
      xhr.setRequestHeader("authorization", "Bearer " + state.session);
      if (xhr.upload) {
        xhr.upload.onprogress = function (e) {
          if (e.lengthComputable) {
            chip.progress = Math.round((e.loaded * 100) / e.total);
            renderChips();
          }
        };
      }
      xhr.onload = function () {
        var body = null;
        try {
          body = JSON.parse(xhr.responseText);
        } catch (e) {
          body = null;
        }
        resolve({ status: xhr.status, body: body });
      };
      xhr.onerror = function () {
        resolve({ status: 0, body: null });
      };
      xhr.onabort = function () {
        resolve({ aborted: true });
      };
      xhr.send(fd);
    });
  }

  var QUEUED_TEXT = "Your chat was refreshed. Your file is kept and is sent right after your next message.";
  var PICKED_TEXT = "Your file is sent right after your message.";

  /* The admin turned customer files off (422 attachments_off, or the history read says so): hide the paperclip now,
     remember it for this tab's stored session, drop every waiting file and say why in plain words. */
  function filesTurnedOff(quiet) {
    state.attach = false;
    chips.forEach(function (c) {
      c.cancelled = true;
      if (c.xhr) c.xhr.abort();
    });
    chips = [];
    save();
    renderChips();
    if (!quiet) setStatus(REFUSALS.attachments_off);
  }

  /* A 401 is the session itself (ended, or its channel moved env on publish): drop it and its conversation; the next
     message opens both again. */
  function dropSession() {
    epoch += 1;
    opening = null;
    state.session = null;
    state.conv = null;
    state.token = null;
    closeEvents();
    save();
  }

  /* Prepare and upload one chip. Resolves "ok", "queued" (401: the file waits for a new conversation), or "gone". */
  async function uploadOne(chip, f) {
    chip.status = "preparing";
    renderChips();
    var prepared = await prepare(f);
    if (chip.cancelled) return "gone";
    if (prepared.size > MAX_FILE_BYTES) {
      chips = chips.filter(function (o) {
        return o !== chip;
      });
      renderChips();
      setStatus(REFUSALS.too_large);
      return "gone";
    }
    chip.name = (prepared.name || chip.name).slice(0, 80);
    chip.file = prepared;
    chip.status = "uploading";
    renderChips();
    var r = await uploadFile(chip, prepared);
    chip.xhr = null;
    if (r.aborted || chip.cancelled) return "gone";
    if (r.status === 401) {
      dropSession();
      chip.status = "queued";
      chip.progress = 0;
      renderChips();
      setStatus(QUEUED_TEXT);
      return "queued";
    }
    if (r.status === 422 && r.body && r.body.error && r.body.error.reason === "attachments_off") {
      filesTurnedOff();
      return "gone";
    }
    var refs = r.status === 201 ? cleanRefs([r.body]) : [];
    if (!refs.length) {
      chips = chips.filter(function (o) {
        return o !== chip;
      });
      renderChips();
      setStatus(refusalText(r.status, r.body));
      return "gone";
    }
    chip.ref = refs[0];
    chip.status = "ready";
    renderChips();
    live.textContent = "Attached " + chip.ref.display_name;
    return "ok";
  }

  async function addFiles(fileList) {
    setStatus("");
    if (state.attach === false) return;
    var files = Array.prototype.slice.call(fileList || []);
    /* no conversation yet: every file waits as a queued chip and uploads after the first message (drainQueued) */
    var early = !state.conv || !state.token || !state.session;
    var held = early;
    for (var i = 0; i < files.length; i++) {
      var f = files[i];
      if (state.attach === false) return;
      if (chips.length >= MAX_FILES) {
        setStatus("You can attach up to " + MAX_FILES + " files to one message.");
        return;
      }
      if (!typeOk(f)) {
        setStatus(REFUSALS.unsupported_type);
        continue;
      }
      var chip = { name: (f.name || "file").slice(0, 80), status: "preparing", progress: 0, ref: null, xhr: null, cancelled: false, file: f };
      chips.push(chip);
      if (held) {
        /* the session ended under an earlier file of this batch: the rest wait for a new conversation too */
        chip.status = "queued";
        renderChips();
        continue;
      }
      if ((await uploadOne(chip, f)) === "queued") held = true;
    }
    if (early && chips.some(function (c) { return c.status === "queued"; })) setStatus(PICKED_TEXT);
  }

  /* After a message opened a new conversation: upload the files that were waiting for one (once; a second 401 re-queues). */
  async function drainQueued() {
    if (!state.conv || !state.token || !state.session) return;
    var waiting = chips.filter(function (c) {
      return c.status === "queued";
    });
    for (var i = 0; i < waiting.length; i++) {
      if (waiting[i].cancelled) continue;
      if (state.attach === false) return;
      if ((await uploadOne(waiting[i], waiting[i].file)) === "queued") return;
    }
    if (waiting.length && !chips.some(function (c) { return c.status === "queued"; })) setStatus("");
    /* the files the customer picked first follow the message they were picked with, as a message of their own */
    var uploaded = waiting.filter(function (c) {
      return c.status === "ready" && !c.cancelled;
    });
    if (uploaded.length && !busy) send("", { files: true });
  }

  function scrollDown() {
    list.scrollTop = list.scrollHeight;
  }

  /* #rrggbb → the readable foreground on it (WCAG relative luminance). */
  function onColour(hex) {
    var lum = 0;
    var w = [0.2126, 0.7152, 0.0722];
    for (var i = 0; i < 3; i++) {
      var c = parseInt(hex.slice(1 + 2 * i, 3 + 2 * i), 16) / 255;
      lum += w[i] * (c <= 0.03928 ? c / 12.92 : Math.pow((c + 0.055) / 1.055, 2.4));
    }
    return lum > 0.4 ? "#0d1020" : "#ffffff";
  }

  function applyDisplay() {
    var d = state.display || {};
    title.textContent = d.title || "Chat with us";
    panel.setAttribute("aria-label", d.title || "Chat with us");
    if (d.colour) {
      var fg = onColour(d.colour);
      root.style.setProperty("--accent", d.colour);
      root.style.setProperty("--me", d.colour);
      root.style.setProperty("--accent-fg", fg);
      root.style.setProperty("--me-fg", fg);
    }
  }

  function applySignIn() {
    var href = state.needSignIn ? signInHref(state.signIn) : null;
    signBox.hidden = !state.needSignIn;
    signText.textContent = state.needSignIn ? SIGN_IN_TEXT : "";
    signLink.hidden = !href;
    if (href) signLink.setAttribute("href", href);
    else signLink.removeAttribute("href");
  }

  function render() {
    while (list.firstChild) list.removeChild(list.firstChild);
    var greeting = state.display && state.display.greeting;
    if (state.identified && state.name) list.appendChild(bubble("assistant", "Hi " + state.name + ", welcome back."));
    if (greeting) {
      list.appendChild(bubble("assistant", greeting));
    } else if (!state.messages.length && !(state.identified && state.name)) {
      var hint = el("div", "empty");
      hint.textContent = "Ask us anything.";
      list.appendChild(hint);
    }
    for (var i = 0; i < state.messages.length; i++) {
      var m = state.messages[i];
      /* A staff reply names its author (B13 display name; textContent only). */
      if (m.role === "assistant" && typeof m.name === "string" && m.name) {
        var who = el("div", "who");
        who.textContent = m.name;
        list.appendChild(who);
      }
      var refs = cleanRefs(m.attachments);
      var b = m.text || !refs.length ? bubble(m.role, m.text) : el("div", "msg " + (m.role === "user" ? "user" : "assistant"));
      list.appendChild(b);
      if (refs.length) b.appendChild(filesBox(refs, m.role));
    }
    var last = state.messages[state.messages.length - 1];
    if (last && last.csat && state.csat) list.appendChild(csatButtons());
    list.appendChild(typing);
    applySignIn();
    renderChips();
    scrollDown();
  }

  /* A new message from the other side: stored, rendered, announced once. */
  function receive(text, csat, name, files) {
    /* The same message can arrive twice (the POST answer and its SSE echo, or a replayed event): show it once. */
    var prev = state.messages[state.messages.length - 1];
    if (prev && prev.role === "assistant" && prev.text === text && !(files && files.length)) {
      if (csat && !prev.csat) {
        prev.csat = true;
        save();
        render();
      }
      return;
    }
    var m = { role: "assistant", text: text };
    if (csat) m.csat = true;
    if (typeof name === "string" && name) m.name = name.slice(0, 80);
    if (files && files.length) m.attachments = files;
    state.messages.push(m);
    save();
    render();
    var said = text || (files && files.length ? "Sent you " + files.map(function (f) { return f.display_name; }).join(", ") : "");
    live.textContent = m.name ? m.name + ": " + said : said;
  }

  function setStatus(text) {
    status.textContent = text || "";
  }

  var busy = false;
  function setBusy(b) {
    busy = b;
    sendBtn.disabled = b;
    typing.hidden = !b;
    if (b) scrollDown();
  }

  function setOpen(open, quiet) {
    state.open = open;
    panel.hidden = !open;
    launcher.setAttribute("aria-expanded", open ? "true" : "false");
    launcher.setAttribute("aria-label", open ? "Close chat" : "Open chat");
    save();
    if (open) {
      render();
      if (!quiet) input.focus();
      /* The session (and the display it carries) is opened when the chat is, not on every page view. */
      if (!state.session) ensureSession();
    } else if (!quiet) {
      launcher.focus();
    }
  }

  // ---- transport ------------------------------------------------------------------------------------------------
  function messageFor(code, httpStatus) {
    if (httpStatus === 429 || code === "rate_limited") return "Please wait a moment before sending another message.";
    if (code === "not_available") return "The assistant is not available right now.";
    if (code === "timeout") return "That took too long. Please try again.";
    if (code === "session_blocked") return "This chat is paused for a while. Please try again later.";
    if (code === "turnstile_failed" || code === "turnstile_required")
      return "We could not verify this browser. Send your message to try again.";
    if (httpStatus === 0 || httpStatus === 503) return "We could not reach the chat. Please send your message again.";
    return "Sorry, something went wrong. Please try again.";
  }

  /* Parse one SSE block ("event: x\ndata: y"); returns {event, data} or null for comments / empty blocks. */
  function parseBlock(block) {
    var ev = "message";
    var data = [];
    var lines = block.split(/\r?\n/);
    for (var i = 0; i < lines.length; i++) {
      var line = lines[i];
      if (!line || line.charAt(0) === ":") continue;
      var idx = line.indexOf(":");
      var field = idx === -1 ? line : line.slice(0, idx);
      var value = idx === -1 ? "" : line.slice(idx + 1);
      if (value.charAt(0) === " ") value = value.slice(1);
      if (field === "event") ev = value;
      else if (field === "data") data.push(value);
    }
    if (!data.length) return null;
    var parsed = null;
    try {
      parsed = JSON.parse(data.join("\n"));
    } catch (e) {
      parsed = null;
    }
    return { event: ev, data: parsed };
  }

  async function readStream(res, onDelta) {
    /* Resolves to {done: true} or {error: code}. */
    var handle = function (frame) {
      if (!frame) return null;
      if (frame.event === "delta" && frame.data && typeof frame.data.text === "string") {
        onDelta(frame.data.text);
        return null;
      }
      if (frame.event === "done") return { done: true };
      if (frame.event === "error") return { error: (frame.data && frame.data.code) || "error" };
      return null;
    };
    var buf = "";
    var outcome = null;
    var drain = function (final) {
      var parts = buf.split(/\r?\n\r?\n/);
      buf = final ? "" : parts.pop();
      for (var i = 0; i < parts.length && !outcome; i++) outcome = handle(parseBlock(parts[i]));
    };
    if (res.body && typeof res.body.getReader === "function" && typeof TextDecoder !== "undefined") {
      var reader = res.body.getReader();
      var decoder = new TextDecoder();
      while (!outcome) {
        var chunk = await reader.read();
        if (chunk.done) break;
        buf += decoder.decode(chunk.value, { stream: true });
        drain(false);
      }
      if (!outcome) {
        buf += decoder.decode();
        drain(true);
      }
      try {
        reader.cancel();
      } catch (e) {
        /* already closed */
      }
    } else {
      buf = await res.text();
      drain(true);
    }
    return outcome || { error: "upstream_error" };
  }

  // ---- later messages (B12) -------------------------------------------------------------------------------------
  var events = null;
  function convUrl(what) {
    /* the conversation token alone is not enough: the stream and the history also prove the session (an
       EventSource cannot set a header, so it rides in the query like the conversation token) */
    return (
      base +
      "/api/webchat/conversations/" +
      state.conv +
      "/" +
      what +
      "?token=" +
      encodeURIComponent(state.token) +
      (state.session ? "&session=" + encodeURIComponent(state.session) : "")
    );
  }

  function closeEvents() {
    if (events) {
      try {
        events.close();
      } catch (e) {
        /* already closed */
      }
      events = null;
    }
  }

  var seen = {};
  function openEvents() {
    if (events || !state.conv || !state.token || typeof window.EventSource !== "function") return;
    events = new window.EventSource(convUrl("events"));
    events.addEventListener("message", function (e) {
      var d = null;
      try {
        d = JSON.parse(e.data);
      } catch (err) {
        d = null;
      }
      var files = d ? cleanRefs(d.attachments) : [];
      if (!d || typeof d.text !== "string" || (!d.text && !files.length)) return;
      if (typeof d.id === "string") {
        if (seen[d.id]) return;
        seen[d.id] = true;
      }
      receive(d.text, d.kind === "csat_request", d.name, files);
    });
    events.addEventListener("sign_in", function (e) {
      var d = null;
      try {
        d = JSON.parse(e.data);
      } catch (err) {
        d = null;
      }
      state.needSignIn = true;
      if (d && typeof d.url === "string" && signInHref(d.url)) state.signIn = d.url;
      save();
      applySignIn();
      live.textContent = SIGN_IN_TEXT;
    });
    events.addEventListener("error", function () {
      /* A token the edge refuses (403) closes the stream for good; the browser retries anything else itself. */
      if (events && events.readyState === 2) {
        events = null;
      }
    });
  }

  /* After a reload: replace the local turns with the stored conversation, then listen. */
  async function resume() {
    if (!state.conv || !state.token) return;
    try {
      var res = await fetch(convUrl("messages"), { mode: "cors", credentials: "omit" });
      if (res.ok) {
        var j = await res.json();
        /* the admin's current customer-files switch (the edge adds it to every history read) */
        if (j && j.attachments === false && state.attach !== false) filesTurnedOff(true);
        else if (j && j.attachments === true && state.attach === false) {
          state.attach = true;
          save();
          renderChips();
        }
        if (j && Array.isArray(j.messages) && j.messages.length) {
          dropBlobs();
          state.messages = j.messages
            .filter(function (m) {
              return m && typeof m.text === "string" && (m.text || cleanRefs(m.attachments).length);
            })
            .map(function (m) {
              var out = { role: m.role === "customer" || m.role === "user" ? "user" : "assistant", text: m.text };
              var refs = cleanRefs(m.attachments);
              if (refs.length) out.attachments = refs;
              return out;
            })
            .slice(-KEEP);
          save();
          render();
        }
      } else if (res.status === 403 || res.status === 401) {
        /* a refused conversation token, or a session that ended or was blocked: the conversation is over */
        state.conv = null;
        state.token = null;
        /* a 401 is the session itself (ended, or its channel moved env on publish): the next message opens a new one */
        if (res.status === 401) state.session = null;
        save();
        return;
      }
    } catch (e) {
      /* history is best effort: keep the local turns */
    }
    openEvents();
  }

  function messageId() {
    return randomId().slice(0, 32);
  }

  async function failureOf(res) {
    var code = null;
    var siteKey = null;
    try {
      var j = await res.json();
      code = j && j.error && j.error.code;
      siteKey = j && j.error && typeof j.error.site_key === "string" ? j.error.site_key : null;
    } catch (e) {
      code = null;
    }
    return { code: code, status: res.status, siteKey: siteKey };
  }

  /* Open a session unless one is held; null on success, else the failure. Concurrent callers share one request. */
  var opening = null;
  var epoch = 0;
  function ensureSession() {
    if (state.session) return Promise.resolve(null);
    if (!opening) {
      var p = startSession().then(function (f) {
        if (opening === p) opening = null;
        return f;
      });
      opening = p;
    }
    return opening;
  }

  /* B7: the host's assertion for the signed-in customer, or null. Asked per session open, held in a local only. */
  async function getAssertion() {
    var a = null;
    try {
      var cfg = window.nonagenticChat;
      if (cfg && typeof cfg.identity === "function") {
        a = await cfg.identity();
      } else if (IDENTITY_URL) {
        var res = await fetch(IDENTITY_URL, {
          method: "GET",
          credentials: "include",
          headers: { accept: "application/json" }
        });
        if (res.ok) {
          var j = await res.json();
          a = j && j.assertion;
        }
      }
    } catch (e) {
      a = null;
    }
    return typeof a === "string" && a.length > 0 && a.length <= MAX_ASSERTION ? a : null;
  }

  function assertionRefused(f) {
    return !!f && f.status === 401 && typeof f.code === "string" && f.code.indexOf("assertion_") === 0;
  }

  /* Ask the host who this is, open the session with it; a refused assertion is asked for once more; a second refusal
   * opens an anonymous session that offers sign-in. */
  async function startSession() {
    var a = await getAssertion();
    var f = await openSession(undefined, { assertion: a });
    if (a && assertionRefused(f)) {
      a = await getAssertion();
      if (a) f = await openSession(undefined, { assertion: a });
      if (assertionRefused(f)) f = await openSession(undefined, { assertion: null, signIn: true });
    }
    return f;
  }

  /* Drop the session and the conversation and start over, anonymous (the host's provider is asked again only with
   * {identify: true}); a session is opened at once when the chat is open. */
  function reset(opts) {
    epoch += 1;
    opening = null;
    closeEvents();
    seen = {};
    state.session = null;
    state.conv = null;
    state.token = null;
    state.messages = [];
    state.pending = null;
    state.identified = false;
    state.name = null;
    state.signIn = null;
    state.needSignIn = false;
    chips.forEach(function (c) {
      c.cancelled = true;
      if (c.xhr) c.xhr.abort();
    });
    chips = [];
    dropBlobs();
    renderChips();
    setBusy(false);
    setStatus("");
    save();
    render();
    if (!panel.hidden) {
      if (opts && opts.identify) {
        ensureSession();
      } else {
        var p = openSession(undefined, { assertion: null }).then(function (f) {
          if (opening === p) opening = null;
          return f;
        });
        opening = p;
      }
    }
  }

  /* Cloudflare's api.js, loaded once (explicit render); resolves window.turnstile or null. */
  var tsLoading = null;
  function loadTurnstile() {
    if (window.turnstile) return Promise.resolve(window.turnstile);
    if (!tsLoading) {
      tsLoading = new Promise(function (resolve) {
        var s = document.createElement("script");
        s.src = TURNSTILE_JS;
        s.async = true;
        s.onload = function () {
          resolve(window.turnstile || null);
        };
        s.onerror = function () {
          tsLoading = null;
          resolve(null);
        };
        document.head.appendChild(s);
      });
    }
    return tsLoading;
  }

  /* Render the challenge for `siteKey` in the panel; resolves the token, or null when it errors or expires. */
  var tsWidget = null;
  async function challenge(siteKey) {
    var ts = await loadTurnstile();
    if (!ts) return null;
    return new Promise(function (resolve) {
      var done = false;
      function finish(token) {
        if (done) return;
        done = true;
        if (tsWidget != null) {
          try {
            ts.remove(tsWidget);
          } catch (e) {
            /* already gone */
          }
          tsWidget = null;
        }
        tsBox.textContent = "";
        resolve(token);
      }
      tsBox.textContent = "";
      live.textContent = "Please complete the verification to start the chat.";
      try {
        tsWidget = ts.render(tsBox, {
          sitekey: siteKey,
          action: "webchat",
          appearance: "interaction-only",
          callback: function (token) {
            finish(token);
          },
          "error-callback": function () {
            finish(null);
          },
          "expired-callback": function () {
            finish(null);
          },
          "timeout-callback": function () {
            finish(null);
          }
        });
      } catch (e) {
        finish(null);
      }
    });
  }

  async function openSession(token, ctx) {
    ctx = ctx || {};
    var ep = epoch;
    var res;
    try {
      res = await fetch(SESSIONS, {
        method: "POST",
        mode: "cors",
        credentials: "omit",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          widget_key: key,
          turnstile_token: token || undefined,
          customer_assertion: ctx.assertion || undefined
        })
      });
    } catch (e) {
      return { code: "network", status: 0 };
    }
    if (res.status === 428 && !token) {
      var f = await failureOf(res);
      var solved = f.siteKey ? await challenge(f.siteKey) : null;
      if (solved) {
        var opened = await openSession(solved, ctx);
        /* interaction-only Turnstile is invisible to a visitor who passes: say briefly that the check ran */
        if (!opened && !tsBox.firstChild) {
          tsBox.textContent = "Verified";
          setTimeout(function () {
            if (tsBox.textContent === "Verified") tsBox.textContent = "";
          }, 4000);
        }
        return opened;
      }
      f = { code: "turnstile_failed", status: 403 };
      setStatus(messageFor(f.code, f.status));
      return f;
    }
    if (!res.ok) return failureOf(res);
    var j = null;
    try {
      j = await res.json();
    } catch (e) {
      j = null;
    }
    if (!j || typeof j.session_token !== "string" || !j.session_token) return { code: "upstream_error", status: 502 };
    /* a reset while this was in flight: that session belongs to the dropped conversation */
    if (ep !== epoch) return { code: "superseded", status: 0 };
    state.session = j.session_token;
    state.display = cleanDisplay(j.display);
    state.csat = j.csat_enabled !== false;
    /* the admin's setting: customer files off hides the paperclip (an older edge that does not say means on) */
    state.attach = j.attachments !== false;
    state.identified = j.identified === true;
    state.name = state.identified ? cleanName(j.greeting_name) : null;
    state.signIn = typeof j.sign_in_url === "string" && signInHref(j.sign_in_url) ? j.sign_in_url : null;
    state.needSignIn = !!ctx.signIn && !!state.signIn;
    save();
    applyDisplay();
    render();
    return null;
  }

  /* One POST of the pending message; a refused session (401) is replaced once and the message resent. */
  async function post(text, id, retried, ids) {
    var f = await ensureSession();
    if (f) return { failure: f };
    var res;
    try {
      res = await fetch(API, {
        method: "POST",
        mode: "cors",
        credentials: "omit",
        headers: { "content-type": "application/json", authorization: "Bearer " + state.session },
        body: JSON.stringify({
          widget_key: key,
          visitor_id: state.visitor,
          messages: state.messages.slice(-SEND_LAST).map(function (m) {
            return { role: m.role, text: m.text };
          }),
          stream: true,
          conversation_id: state.conv || undefined,
          token: state.token || undefined,
          message_id: id,
          attachments: ids && ids.length ? ids : undefined
        })
      });
    } catch (e) {
      return { failure: { code: "network", status: 0 } };
    }
    if (res.status === 401 && !retried) {
      /* One session = one conversation: a new session starts a new one. */
      state.session = null;
      state.conv = null;
      state.token = null;
      closeEvents();
      save();
      return post(text, id, true, ids);
    }
    return { res: res };
  }

  /* Transient failures keep the pending message (and its id) for the resend. */
  function transient(f) {
    return f.status === 0 || f.status === 502 || f.status === 503 || f.status === 504;
  }

  /* `opts.files`: send the ready files as a message of their own (the follow-up of a first message). */
  async function send(preset, opts) {
    var typed = preset == null;
    var withFiles = typed || !!(opts && opts.files);
    var text = (typed ? input.value : preset).trim();
    var waiting = chips.some(function (c) {
      return c.status !== "ready" && c.status !== "queued";
    });
    var ready = chips.filter(function (c) {
      return c.status === "ready";
    });
    if (busy) return;
    if (typed && waiting) {
      setStatus("Please wait for your file to finish uploading.");
      return;
    }
    /* a message of only files says what it is, never an id or a file name */
    if (!text && withFiles && ready.length) {
      text = ready.every(function (c) {
        return c.ref.media_type.indexOf("image/") === 0;
      })
        ? "(photo)"
        : "(file)";
    }
    if (!text) {
      if (typed && chips.some(function (c) { return c.status === "queued"; }))
        setStatus("Write your message first. Your file is sent right after it.");
      return;
    }
    var sentRefs = withFiles ? ready.map(function (c) { return c.ref; }) : [];
    var sentIds = sentRefs.map(function (r) { return r.id; });
    if (text.length > MAX_CHARS) text = text.slice(0, MAX_CHARS);
    var id = state.pending && state.pending.text === text ? state.pending.id : messageId();
    state.pending = { text: text, id: id };
    setStatus("");
    var mine = { role: "user", text: text };
    if (sentRefs.length) mine.attachments = sentRefs;
    state.messages.push(mine);
    if (typed) input.value = "";
    save();
    render();
    setBusy(true);

    var answer = null;
    var answerText = "";
    var csat = false;
    var failure = null;
    var handled = false;
    var duplicate = false;
    var sent = await post(text, id, false, sentIds);
    try {
      var res = sent.res;
      if (sent.failure) {
        failure = sent.failure;
      } else if (!res.ok) {
        failure = await failureOf(res);
        if (res.status === 403 && failure.code === "invalid_token") {
          state.conv = null;
          state.token = null;
        }
      } else if ((res.headers.get("content-type") || "").indexOf("application/json") === 0) {
        /* B12: the core answer, {status, text, conversation_id, token}; later messages come over SSE. */
        var r = await res.json();
        if (r && r.conversation_id && r.token) {
          state.conv = r.conversation_id;
          state.token = r.token;
        }
        if (r && typeof r.text === "string" && r.text) answerText = r.text;
        csat = !!(r && r.kind === "csat_request");
        duplicate = !!(r && r.status === "duplicate");
        handled = true;
      } else {
        var outcome = await readStream(res, function (delta) {
          if (!answer) {
            answer = bubble("assistant", "");
            list.insertBefore(answer, typing);
          }
          answerText += delta;
          markdown(answer, answerText);
          scrollDown();
        });
        if (outcome.error) failure = { code: outcome.error, status: 200 };
      }
    } catch (e) {
      failure = { code: "network", status: 0 };
    }

    setBusy(false);
    if (!failure || !transient(failure)) state.pending = null;
    /* the files left with the message; a message taken back keeps them for the resend */
    if (sentRefs.length && (!failure || answerText || handled)) {
      chips = chips.filter(function (c) {
        return sentRefs.indexOf(c.ref) < 0;
      });
      renderChips();
    }
    if (failure) {
      if (!answerText && !handled) {
        /* Nothing answered: take the message back so it can be sent again (with the same id if transient). */
        state.messages.pop();
        if (typed && !input.value) input.value = text;
      }
      setStatus(messageFor(failure.code, failure.status));
    }
    save();
    if (answerText) receive(answerText, csat);
    else render();
    input.focus();
    /* A resend whose first answer was lost comes back `duplicate`: the stored history holds that answer. */
    if (duplicate) resume();
    else openEvents();
    if (!failure) drainQueued();
  }

  // ---- events ---------------------------------------------------------------------------------------------------
  launcher.addEventListener("click", function () {
    setOpen(panel.hidden);
  });
  closeBtn.addEventListener("click", function () {
    setOpen(false);
  });
  panel.addEventListener("keydown", function (e) {
    if (e.key === "Escape") {
      e.preventDefault();
      setOpen(false);
    }
  });
  attachBtn.addEventListener("click", function () {
    if (!attachBtn.disabled) fileInput.click();
  });
  fileInput.addEventListener("change", function () {
    var picked = Array.prototype.slice.call(fileInput.files || []);
    fileInput.value = "";
    addFiles(picked);
  });
  input.addEventListener("keydown", function (e) {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      send();
    }
  });
  form.addEventListener("submit", function (e) {
    e.preventDefault();
    send();
  });

  /* The host's own object (it may hold `identity`) gains `reset`. */
  try {
    var cfg = window.nonagenticChat;
    if (!cfg || typeof cfg !== "object") cfg = window.nonagenticChat = {};
    cfg.reset = reset;
  } catch (e) {
    /* a frozen host object: the widget still works without reset */
  }

  function mount() {
    document.body.appendChild(host);
    applyDisplay();
    if (state.open) setOpen(true, true);
    else render();
    resume();
  }
  if (document.body) mount();
  else document.addEventListener("DOMContentLoaded", mount);
})();
