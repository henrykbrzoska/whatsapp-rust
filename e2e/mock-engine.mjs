#!/usr/bin/env node
// Mock engine for the Playwright E2E suite — a dependency-free Node HTTP server
// that implements the subset of the `API-WHATSAPP.md` contract the client uses
// today, so the browser can run the full handshake → SSE → command path without
// Rust, WhatsApp or a phone.
//
//   node e2e/mock-engine.mjs [--port 8788] [--host 127.0.0.1] [--token e2e-token]
//
// Contract coverage (§1.2 transport, §3 commands)
//   GET  /                       liveness probe (unauthenticated, for webServer readiness)
//   GET  /version                { version, protocolRef }
//   GET  /config                 engine config (secrets redacted, like the real one)
//   GET  /connected, /logged_in  session state
//   GET  /status                 ConnectionStatus (state/connected/loggedIn/error/attempts)
//   POST /connect                starts the "session" and pushes a `qr` event
//   POST /disconnect, /logout    session teardown
//   GET  /events                 SSE stream of WaEvent (§2), `?token=`
//   GET  /chats, /chats/:jid/stats, /chats/:jid/history/export
//   GET  /search                 hybrid search reply (always empty)
//   POST /send/message           { id, to, text } — see NOTE below
//   POST /group/create, /group/:jid/info, /group/:jid/participants, /group/:jid/leave
//   GET  /debug/log              event ring buffer (handy when a spec fails)
//
// NOTE: `POST /send/message` has no Rust handler yet (`docs/PROMPT-PHASE-4.md`:
// "No send/message routes yet") and the client has no composer. The mock answers
// `{ id, to, text }` and stores the message so `GET /chats` reports it; when the
// engine route lands, adjust the reply shape here and in `tests/send-message.spec.ts`.
//
// Test control API (never part of the engine contract, never authenticated):
//   GET  /__mock/requests   every request seen ({ method, path, query, authorization, body })
//   GET  /__mock/state      { sse, connected, messages, groups, requests }
//   POST /__mock/emit       publish one WaEvent to all SSE subscribers
//   POST /__mock/reset      drop recorded requests + messages, back to fixtures
//   POST /__mock/pair       mark the device paired and publish `pair_success`

import { randomBytes } from 'node:crypto';
import { createServer } from 'node:http';

// ---------------------------------------------------------------- arguments

const argv = process.argv.slice(2);
const arg = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : fallback;
};

const PORT = Number(arg('port', process.env.E2E_MOCK_PORT ?? 8788));
const HOST = arg('host', '127.0.0.1');
/** Capability token the mock requires; `--token ''` disables the check. */
const TOKEN = arg('token', process.env.E2E_MOCK_TOKEN ?? 'e2e-token');

// ---------------------------------------------------------------- fixtures

const ALICE = '48123123123@s.whatsapp.net';
const GROUP = '120363000000000001@g.us';

function seed() {
  return {
    connected: false,
    loggedIn: false,
    requests: [],
    /** jid → { id, text, timestamp, sender, isFromMe }[] */
    messages: new Map([
      [ALICE, [{ id: 'MOCK-IN-1', text: 'Hello from the mock engine', timestamp: 1_767_000_000, sender: ALICE, isFromMe: false }]],
      [GROUP, [{ id: 'MOCK-IN-2', text: 'Group fixture message', timestamp: 1_767_000_100, sender: '48123999888@s.whatsapp.net', isFromMe: false }]],
    ]),
    names: new Map([
      [ALICE, 'Alice (mock)'],
      [GROUP, 'Mock group'],
    ]),
    groups: new Map(),
    /** Published WaEvents, for `GET /debug/log` and test debugging. */
    events: [],
    seq: 0,
  };
}

let state = seed();
/** Open SSE responses (`ServerResponse`). */
const subscribers = new Set();
let messageSeq = 0;

// ---------------------------------------------------------------- helpers

/**
 * Response → owning request, so the CORS helper can echo `Origin` without
 * threading `req` through every route handler.
 */
const owners = new WeakMap();
const requestOf = (res) => owners.get(res) ?? {};

const json = (res, status, body, extra = {}) => {
  const text = JSON.stringify(body);
  res.writeHead(status, {
    'Content-Type': 'application/json',
    'Content-Length': Buffer.byteLength(text),
    ...cors(requestOf(res)),
    ...extra,
  });
  res.end(text);
};

function cors(req) {
  return {
    // Bearer tokens are sent as a header (not cookies), so `*` is safe here.
    'Access-Control-Allow-Origin': req.headers?.origin ?? '*',
    'Access-Control-Allow-Methods': 'GET, POST, PUT, PATCH, DELETE, OPTIONS',
    'Access-Control-Allow-Headers': 'authorization, content-type',
    'Access-Control-Max-Age': '600',
    Vary: 'Origin',
  };
}

function readBody(req) {
  return new Promise((ok, fail) => {
    const chunks = [];
    let size = 0;
    req.on('data', (c) => {
      size += c.length;
      if (size > 8 * 1024 * 1024) fail(new Error('body too large'));
      else chunks.push(c);
    });
    req.on('end', () => {
      const text = Buffer.concat(chunks).toString('utf8');
      if (!text) return ok(null);
      try {
        ok(JSON.parse(text));
      } catch {
        ok(text);
      }
    });
    req.on('error', fail);
  });
}

function tokenOf(req, url) {
  const auth = req.headers.authorization ?? '';
  const m = /^Bearer\s+(.+)$/i.exec(auth);
  return m ? m[1] : url.searchParams.get('token');
}

/** Every published event also lands in the ring buffer (oldest first). */
function publish(event) {
  state.seq += 1;
  state.events.push({ seq: state.seq, ts: Date.now(), event });
  if (state.events.length > 200) state.events.shift();
  const frame = `data: ${JSON.stringify(event)}\n\n`;
  for (const res of subscribers) {
    try {
      res.write(frame);
    } catch {
      subscribers.delete(res);
    }
  }
}

function chatEntries() {
  const out = [];
  for (const [jid, msgs] of state.messages) {
    if (!msgs.length) continue;
    const last = msgs[msgs.length - 1];
    out.push({
      jid,
      name: state.names.get(jid) ?? null,
      count: msgs.length,
      last_timestamp: last.timestamp,
      last_text: last.text,
      muted: false,
      muted_until: null,
      pinned: false,
      archived: false,
    });
  }
  return out.sort((a, b) => b.last_timestamp - a.last_timestamp);
}

function storedGroup(jid, name, participants = []) {
  return {
    jid,
    ownerJid: '48123000000@s.whatsapp.net',
    ownerPn: '48123000000@s.whatsapp.net',
    name,
    nameSetAt: null,
    nameSetBy: '',
    nameSetByPn: '',
    topic: '',
    topicId: '',
    topicSetAt: null,
    topicSetBy: '',
    topicSetByPn: '',
    topicDeleted: false,
    isLocked: false,
    isAnnounce: false,
    announceVersionId: '',
    isEphemeral: false,
    disappearingTimer: 0,
    isIncognito: false,
    isParent: false,
    defaultMembershipApprovalMode: '',
    linkedParentJid: '',
    isDefaultSubGroup: false,
    isJoinApprovalRequired: false,
    addressingMode: 'pn',
    groupCreated: Math.floor(Date.now() / 1000),
    creatorCountryCode: '48',
    participantVersionId: '1',
    participants: [
      {
        jid: '48123000000@s.whatsapp.net',
        lid: '',
        phoneNumber: '48123000000',
        displayName: 'You (mock)',
        isAdmin: true,
        isSuperAdmin: true,
        error: 0,
        addRequest: null,
      },
      ...participants.map((jid) => ({
        jid,
        lid: '',
        phoneNumber: jid.split('@')[0],
        displayName: '',
        isAdmin: false,
        isSuperAdmin: false,
        error: 0,
        addRequest: null,
      })),
    ],
    participantCount: participants.length + 1,
    memberAddMode: 'all_member_add',
    suspended: false,
  };
}

// ---------------------------------------------------------------- routing

async function route(req, url, body) {
  const path = url.pathname.replace(/\/+$/, '') || '/';
  const method = req.method ?? 'GET';

  // --- test control (no auth) ---
  if (path === '/__mock/requests') return { status: 200, body: { requests: state.requests } };
  if (path === '/__mock/state') {
    return {
      status: 200,
      body: {
        sse: subscribers.size,
        connected: state.connected,
        requests: state.requests.length,
        chats: chatEntries(),
        groups: [...state.groups.keys()],
      },
    };
  }
  if (path === '/__mock/emit' && method === 'POST') {
    if (!body || typeof body !== 'object') return { status: 400, body: { error: 'event object required' } };
    publish(body);
    return { status: 200, body: { ok: true, sse: subscribers.size } };
  }
  // Test control: finish the (fake) pairing, so the "paired" branch is reachable
  // (POST /connect alone leaves the device unpaired, like the real engine).
  if (path === '/__mock/pair' && method === 'POST') {
    state.connected = true;
    state.loggedIn = true;
    publish({ type: 'pair_success', jid: ALICE, lid: null, businessName: null, platform: null });
    return { status: 200, body: { ok: true } };
  }
  if (path === '/__mock/reset' && method === 'POST') {
    state = seed();
    messageSeq = 0;
    return { status: 200, body: { ok: true } };
  }

  // --- liveness (also the webServer readiness probe, so no token needed) ---
  if (path === '/') return { status: 200, body: { ok: true, engine: 'mock', contract: 'API-WHATSAPP.md' } };

  // --- auth, like the real engine (§1.2) ---
  if (TOKEN && tokenOf(req, url) !== TOKEN) {
    return { status: 401, body: { error: 'unauthorized: bad or missing capability token' } };
  }

  if (path === '/version') return { status: 200, body: { version: '0.1.0', protocolRef: 'mock-engine' } };
  if (path === '/config') {
    return {
      status: 200,
      body: {
        addr: `http://${HOST}:${PORT}`,
        auth: Boolean(TOKEN),
        token: TOKEN ? '<redacted>' : null,
        cors: ['*'],
        diagnostic: true,
        dataDir: '.bebok/mock-engine',
        waUrl: null,
        handshakeTimeoutSecs: 20,
        vectorSearch: false,
      },
    };
  }
  if (path === '/connected') return { status: 200, body: state.connected };
  if (path === '/logged_in') return { status: 200, body: state.loggedIn };
  // Same shape as `bebok-server`'s `ConnectionStatus` (§2.1): the login screen
  // polls it every 2 s while pairing, so `loggedIn` is what tells "socket up"
  // from "paired".
  if (path === '/status') {
    return {
      status: 200,
      body: {
        state: state.loggedIn ? 'connected' : state.connected ? 'connecting' : 'disconnected',
        connected: state.connected,
        loggedIn: state.loggedIn,
        error: null,
        attempts: state.connected || state.loggedIn ? 1 : 0,
      },
    };
  }

  if (path === '/connect' && method === 'POST') {
    state.connected = true;
    state.loggedIn = false;
    // The real engine follows the handshake with QR codes; mirror that with two
    // fake codes so a spec can rely on `/connect` instead of `/__mock/emit`.
    setTimeout(() => publish({ type: 'qr', codes: ['2@MOCK-QR-FIRST,AA,BB', '2@MOCK-QR-SECOND,CC,DD'] }), 10);
    return {
      status: 200,
      body: { state: 'connecting', connected: false, loggedIn: false, error: null, attempts: 1 },
    };
  }
  if (path === '/disconnect' && method === 'POST') {
    state.connected = false;
    publish({ type: 'disconnected' });
    return { status: 200, body: { ok: true } };
  }
  if (path === '/logout' && method === 'POST') {
    state.connected = false;
    state.loggedIn = false;
    publish({ type: 'logged_out', onConnect: false, reason: 401 });
    return { status: 200, body: { ok: true } };
  }

  if (path === '/chats') return { status: 200, body: chatEntries() };
  if (path === '/search') return { status: 200, body: { mode: url.searchParams.get('mode') ?? 'hybrid', vector: false, results: [] } };
  if (path === '/debug/log') {
    return {
      status: 200,
      body: {
        capacity: 200,
        entries: state.events.map((e) => ({ seq: e.seq, ts: e.ts, kind: 'event', message: e.event.type, data: e.event })),
      },
    };
  }

  if (method === 'POST' && path === '/send/message') {
    const to = String(body?.to ?? '');
    const text = String(body?.text ?? '');
    if (!to || !text) return { status: 400, body: { error: 'to and text are required' } };
    const id = `MOCK-OUT-${++messageSeq}-${randomBytes(3).toString('hex').toUpperCase()}`;
    const list = state.messages.get(to) ?? [];
    list.push({ id, text, timestamp: Math.floor(Date.now() / 1000), sender: '48123000000@s.whatsapp.net', isFromMe: true });
    state.messages.set(to, list);
    // The engine echoes our own message back as a `message` event (§2).
    publish({
      type: 'message',
      info: {
        chat: to,
        sender: '48123000000@s.whatsapp.net',
        senderAlt: '',
        recipientAlt: '',
        isFromMe: true,
        isGroup: to.endsWith('@g.us'),
        addressingMode: 'pn',
        id,
        serverId: 0,
        type: 'text',
        pushName: 'You (mock)',
        timestamp: Math.floor(Date.now() / 1000),
        category: '',
        multicast: false,
        mediaType: '',
        edit: '',
        msgBotInfo: { editType: '', editTargetId: '', editSenderTimestampMs: 0, clientThreadId: '' },
        msgMetaInfo: { targetId: '', targetSender: '', targetChat: '', deprecatedLidSession: null, threadMessageId: '', threadMessageSenderJid: '' },
        deviceSentMeta: null,
      },
      message: null,
      raw: null,
      text,
      isEphemeral: false,
      isViewOnce: false,
      isEdit: false,
      retryCount: 0,
    });
    return { status: 200, body: { id, to, text } };
  }

  if (method === 'POST' && path === '/group/create') {
    const name = String(body?.name ?? '').trim();
    const participants = Array.isArray(body?.participants) ? body.participants.map(String) : [];
    if (!name) return { status: 400, body: { error: 'group name must not be empty' } };
    // Error-path hook: a name containing "reject" fails, so a spec can assert
    // how the client surfaces a `{ error }` body.
    if (name.includes('reject')) return { status: 400, body: { error: 'group name rejected by the mock engine' } };
    const jid = `120363${String(Date.now()).slice(-9)}@g.us`;
    const info = storedGroup(jid, name, participants);
    state.groups.set(jid, info);
    state.names.set(jid, name);
    if (!state.messages.has(jid)) state.messages.set(jid, []);
    publish({ type: 'joined_group', info: { reason: 'create', type: 'new', createKey: '', sender: '', senderPn: '', notify: name, info } });
    return { status: 200, body: info };
  }

  const groupMatch = /^\/group\/([^/]+)\/(info|leave|participants|name|topic|invite-link)$/.exec(path);
  if (groupMatch) {
    const jid = decodeURIComponent(groupMatch[1]);
    const action = groupMatch[2];
    const info = state.groups.get(jid);
    if (!info) return { status: 404, body: { error: `group not found: ${jid}` } };
    if (action === 'info') return { status: 200, body: info };
    if (action === 'invite-link') return { status: 200, body: { link: 'https://chat.whatsapp.com/MOCKINVITE' } };
    if (action === 'leave') {
      state.groups.delete(jid);
      return { status: 200, body: { ok: true } };
    }
    if (action === 'participants') return { status: 200, body: { ok: true, participants: info.participants } };
    if (action === 'name' && body?.name) {
      info.name = String(body.name);
      return { status: 200, body: { ok: true } };
    }
    if (action === 'topic') return { status: 200, body: { ok: true } };
  }

  const statsMatch = /^\/chats\/([^/]+)\/stats$/.exec(path);
  if (statsMatch) {
    const jid = decodeURIComponent(statsMatch[1]);
    const msgs = state.messages.get(jid) ?? [];
    return {
      status: 200,
      body: {
        count: msgs.length,
        first_timestamp: msgs.length ? msgs[0].timestamp : null,
        last_timestamp: msgs.length ? msgs[msgs.length - 1].timestamp : null,
      },
    };
  }

  const exportMatch = /^\/chats\/([^/]+)\/history\/export$/.exec(path);
  if (exportMatch) {
    const jid = decodeURIComponent(exportMatch[1]);
    const msgs = state.messages.get(jid) ?? [];
    return {
      status: 200,
      body: msgs,
      extra: { 'Content-Disposition': `attachment; filename="chat-${jid}.json"` },
    };
  }

  return { status: 404, body: { error: `no mock route for ${method} ${path}` } };
}

// ---------------------------------------------------------------- server

const server = createServer(async (req, res) => {
  owners.set(res, req);
  const url = new URL(req.url ?? '/', `http://${req.headers.host ?? `${HOST}:${PORT}`}`);
  const path = url.pathname.replace(/\/+$/, '') || '/';

  if (req.method === 'OPTIONS') {
    res.writeHead(204, cors(req));
    res.end();
    return;
  }

  // Server-Sent Events: long-lived, never buffered, removed on close.
  if (path === '/events') {
    if (TOKEN && tokenOf(req, url) !== TOKEN) {
      json(res, 401, { error: 'unauthorized: bad or missing capability token' });
      return;
    }
    state.requests.push({ method: req.method, path, query: url.search, authorization: req.headers.authorization ?? null, body: null });
    res.writeHead(200, {
      'Content-Type': 'text/event-stream',
      'Cache-Control': 'no-cache, no-transform',
      Connection: 'keep-alive',
      'X-Accel-Buffering': 'no',
      ...cors(req),
    });
    res.write(': connected\n\n');
    subscribers.add(res);
    const beat = setInterval(() => {
      try {
        res.write(': ping\n\n');
      } catch {
        /* closing */
      }
    }, 15_000);
    req.on('close', () => {
      clearInterval(beat);
      subscribers.delete(res);
    });
    return;
  }

  let body = null;
  try {
    body = await readBody(req);
  } catch (e) {
    json(res, 400, { error: String(e.message ?? e) });
    return;
  }

  let out;
  try {
    out = await route(req, url, body);
  } catch (e) {
    out = { status: 500, body: { error: String(e?.message ?? e) } };
  }

  state.requests.push({
    method: req.method,
    path,
    query: url.search || null,
    authorization: req.headers.authorization ?? null,
    body: body && typeof body === 'object' ? body : null,
    status: out.status,
  });

  if (out.body !== null && typeof out.body === 'object') json(res, out.status, out.body, out.extra ?? {});
  else {
    res.writeHead(out.status, { 'Content-Type': 'text/plain', ...cors(req), ...(out.extra ?? {}) });
    res.end(String(out.body ?? ''));
  }
});

server.listen(PORT, HOST, () => {
  // stdout is the machine-readable line Playwright's webServer probe can wait on.
  console.log(`MOCK_READY http://${HOST}:${PORT}/?token=${TOKEN}`);
});

for (const sig of ['SIGINT', 'SIGTERM']) {
  process.on(sig, () => {
    for (const res of subscribers) {
      try {
        res.end();
      } catch {
        /* closing */
      }
    }
    server.close(() => process.exit(0));
  });
}
