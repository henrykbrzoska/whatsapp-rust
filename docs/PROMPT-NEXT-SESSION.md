# Next session — media bubbles + history fetch (frontend half)

Hand-off written 2026-11-06. Read `AGENTS.md` first, then this file.
Everything below is the state of the tree **as committed on disk right now**:
engine and client both build, all tests pass.

---

## 0. Working rules (these came from the user, they override habits)

* **The app is Rust from scratch.** `whatsmeow/` (Go) is a *reference for the
  mechanisms only* — read it to learn how the protocol behaves, never link,
  vendor or port it verbatim beyond what the MPL headers already cover.
* **Work in stable slices.** Reach a point where `cargo test`, `ng test` and
  `ng build` are green, then stop and report. Do not chain three features in
  one go.
* **Never start the engine or the dev stack.** The user runs
  `npm run full-build-dev` themselves and pastes the
  `BEBOK_READY http://127.0.0.1:8787/?token=…` line. Each engine launch mints
  a new token, so an older link stops working.
* English only in code, comments, UI strings and docs. No i18n.
* The engine owns the protocol and the database; the client only renders.
  Keep protobuf decoding inside `wa-core` (that is why
  `download::downloadable_from_bytes` exists).

---

## 1. Where the app is

A local WhatsApp client: Angular 20 (standalone, signals) + Tauri 2 + a Rust
engine (`engine/crates/{wa-proto,wa-core,wa-store,bebok-server}`).

The user is **paired and connected**. Their chats are `@lid` JIDs, which have
no phone number to fall back on, so display names matter more than usual.

Current green state:

```
cd engine  && cargo test --workspace   # 536 tests, 0 failures
cd engine  && cargo clippy --workspace --all-targets   # 0 warnings
cd client  && npx ng test --watch=false --browsers=ChromeHeadless  # 104 specs
cd client  && npx ng build             # clean
```

---

## 2. What the last sessions built (do not redo)

### UI — it reads as a messenger now

* `features/chats/chats-shell.component.ts` — two-pane WhatsApp Web layout
  (chat list left, conversation right, one pane at a time under 860px) plus
  the "select a chat" empty pane. Routes are nested under it in
  `app.routes.ts`.
* `features/messages/message-list.component.ts` — bubble thread with day
  separators, author runs, clickable links, and a **custom virtual scroll**
  driven by measured row heights (no `@angular/cdk`; bubbles wrap, so
  fixed-height windowing would misplace them).
* `features/messages/composer.component.ts` — growing box, Enter sends,
  Shift+Enter newline, typing presence, keeps the draft when a send fails.
* `features/chats/chat.component.ts` — the thread pane: header with avatar,
  name, presence subtitle; search behind a 🔍 toggle; export + stats + group
  info behind the `⋮` menu.
* `core/services/contact-names.ts` — names from `GET /contacts`, from the
  `pushName` of live messages, from stored `push_name`, and from chat-list
  rows. Used by the sidebar, the thread, author labels and search results.
* `core/format.ts` — `formatClock`, `formatDay`, `sameDay`, `mediaLabel`,
  `initial`, `isGroupJid`, `linkify` (safe: only `http(s)`/`www`, never
  `javascript:`), all unit-tested in `core/format.spec.ts`.

### Engine — the bugs that were actually biting

* **Send failed with error 479** (`smax-invalid`): our DM stanza carried a
  `phash` attribute, which belongs on **group** stanzas only. Fixed in
  `client/send.rs` (`prepare_message_node_with` adds it only for groups);
  pinned by `only_a_group_stanza_carries_the_participant_hash`.
  `SendResponse.skipped_devices` now reports devices the message could not be
  encrypted for, surfaced by `POST /send/message` and in `GET /debug/log`.
* **Nothing ever synced the address book**: `fetch_app_state` existed but had
  no caller, so `GET /contacts` was always empty. New
  `bebok-server/src/appstate_sync.rs` syncs all five collections after
  `connected` (throttled to once a minute) and on `POST /appstate/sync`;
  the chat list has a **"Sync names"** button for it.
* **The engine never reconnected**: `GET /status` reported `loggedIn: false`
  for a paired device with no session, and the client never read `/status`.
  Now `Connection::status` consults the device container, and
  `WhatsappService.resumeSession()` adopts the state on load and calls
  `POST /connect` for a paired device.
* **Push names are persisted** (`client/message.rs::update_push_name`), and
  `wa-store` keeps `push_name` per message, with `GET /chats` falling back to
  it — so names work from the local database alone.
* **Scrolling was broken** (fixed, verified in the browser): Angular component
  hosts (`app-root`, `app-chat`, `app-message-list`, `app-chats-shell`) are
  plain blocks between the flex containers, so the height chain never reached
  the thread. See the `app-root { … }` block in `styles.css` — if you add a
  component between `.pane` and `.thread-scroll`, give its host
  `display:flex; flex:1 1 auto; min-height:0` too. The virtual scroller's
  corrections are bounded by `SETTLE_PASSES`; an unbounded one pins the view
  to the bottom and the thread cannot be scrolled at all.

### Engine — media + history (the enabling half, done and tested)

* `wa-core/src/client/mediaconn.rs` — `<iq xmlns="w:m"><media_conn/></iq>`,
  host list, TTL cache. 3 tests.
* `wa-core/src/client/download.rs` — the whole download mechanism:
  HKDF-SHA256 expansion to iv/cipher/mac/ref, the CDN URL
  (`{directPath}&hash=…&mms-type=…&__wa-mms=`), host failover, 403 → re-query
  the media conn, retries, and the three integrity checks
  (`fileEncSha256` → HMAC → `fileSha256`) before any byte is accepted. Plus
  `downloadable_from_message` / `downloadable_from_bytes`. 9 tests, including
  a full round trip and one case per tampering kind.
* `wa-core/src/client/history_blob.rs` — `DecodedHistory { conversations,
  messages }`: per-message `push_name`, media kind, captions as text, and the
  raw payload; per-conversation **name**, last timestamp, unread, pin,
  archive, mute, end-of-history. 7 tests.
* `wa-core/src/client/history_sync.rs` — `Client::download_history_blob`
  (download + decode) and `Client::request_history(oldest, count)` (the
  on-demand peer request). **No tests** — both need a live socket.
* `wa-store` — `media_blobs` table (`put/get/purge/count_media_blobs`),
  `insert_messages_if_new` (a live message always beats its history copy),
  `page_before` / `count_before`, `from_me`, `push_name`. 30 tests.
* `bebok-server/src/history_import.rs` — blob → `wa-store`, conversation names
  → contact store. Wired into the history-sync forwarder in `connection.rs`,
  emits `WaEvent::HistoryImported`.
* `bebok-server/src/routes/history.rs` — `POST /history/fetch`,
  `GET /media/{id}`, `GET /media/{id}/info`, 1-hour blob TTL with an
  opportunistic purge per request and `spawn_media_purge` every 10 minutes
  (started in `lib.rs::run`).

---

## 2.5 Built on 2026-11-06 — the frontend half is done

Everything in §3 below was implemented and tested; the section is kept for the
design rationale, not as a to-do list. What landed:

* **Media bubbles** (`message-list.component.ts`) — `<img>` / `<video>` /
  `<audio>` / download link per kind, caption under the media, a failed
  download falling back to the label plus retry. A message only renders as
  media when the engine kept its payload (`raw_json`), so no request is sent
  that is bound to 404.
* **The escalation** (`chat.component.ts`) — local pages → "Fetch older
  messages from WhatsApp" (`POST /history/fetch`) → "waiting for the phone…"
  → prepend plus "N older message(s) arrived" on `history_imported`.
* **"Fetch history"** in the chat list, for every known chat at once.
* **Service** — `fetchHistory`, `mediaUrl` (token in the query),
  `mediaInfo`; `TauriService.mediaUrl`, `COMMAND_ROUTES.fetch_history` /
  `media_info`; `core/format.ts::mediaRender`.
* **Tests** — `message-list.spec.ts` (new), `bebok-server/tests/history.rs`
  (new), plus `chat.component.spec.ts`, `chat-list.spec.ts`,
  `whatsapp.service.spec.ts` and `format.spec.ts` extended. Client 148 specs,
  engine 546 tests, clippy and `ng build` clean.

Three bugs were fixed on the way (details in `CHANGELOG.md`): the external
app-state snapshot was never downloaded (which is why **"Sync names" failed
for every collection**), a slow `open()` could wipe a newer one, and
`POST /history/fetch` answered `503` instead of `400` for a bad JID.

### Still open from this round

* **`GET /chats/{jid}/messages` ships `raw_json` for every row.** It is how the
  client knows a message has an attachment, but it means the whole hex payload
  of 50 messages travels on every page. If a thread feels slow, replace the
  field with a boolean (`hasMedia`) in `MessagePage` and keep `raw_json` for
  `GET /media/{id}` alone.
* `composer.spec.ts` and `contact-names.spec.ts` (§3.5) are still unwritten.
* The media cache still has no byte budget, only the 1-hour TTL.

## 3. The design of the above (implemented — kept for the rationale)

> "zrób download history że pobierze np miesiąc a potem podczas przewijania
> niech się po kliknięciu w pojawiający button doładuje" — and a global button
> "żeby całą listę zrobić (z 1 ostatnią wiadomością)"; plus
> "media powinny się wyświetlać (gify/obrazy/video po kliknięciu play)",
> cached "jako blob tymczasowo i czyszczone po godzinie".

The engine side of all of that exists. **What is missing is the frontend**
plus HTTP-level tests.

### 3.1 Client service (`core/services/whatsapp.service.ts`)

```ts
fetchHistory(chat?: Jid, count?: number): Promise<FetchReport>   // POST /history/fetch
mediaUrl(messageId: string): string                              // GET /media/{id}?token=…
mediaInfo(messageId: string): Promise<MediaInfo>                 // GET /media/{id}/info
```

`mediaUrl` must embed `?token=` (an `<img src>` cannot send an
`Authorization` header; the engine accepts the query parameter). Build it from
`TauriService.endpoint()`; add a small `mediaUrl` helper there rather than
reaching into the signal from the component.

`FetchReport` = `{ count, requested: [{chat, oldestId, oldestTimestamp,
requestId}], skipped: string[] }`.
`MediaInfo` = `{ id, kind, available, cached, mimeType, size }`.

### 3.2 Media bubbles (`features/messages/message-list.component.ts`)

Today a non-text message renders `mediaLabel(kind)` as a grey italic line.
Replace that with, by `message.type`:

* `image` / `sticker` → `<img [src]="mediaUrl(m.id)" loading="lazy">` with a
  max height (~320px), click to open full size;
* `video` / `gif` → `<video [src]="…" controls preload="metadata">` (the user
  explicitly wants "video po kliknięciu play", so do **not** autoplay; a GIF
  arrives as `video/mp4` with `gifPlayback`, so `loop muted` is fine there);
* `audio` / `ptt` → `<audio controls preload="none">`;
* `document` → a download link with the filename and size.

Keep the caption (`message.text`) under the media. Handle the error case: the
first `GET /media/{id}` downloads from WhatsApp and can fail (`502`), so bind
`(error)` and fall back to the current label plus a "retry" affordance. Do not
fetch media for rows outside the virtual-scroll window — the window already
handles that, just make sure the `src` is bound inside the `@for`, not
precomputed for the whole thread.

### 3.3 "Load earlier" escalation (`features/chats/chat.component.ts`)

`MessageListComponent` already emits `loadOlder` near the top and shows a
"Load earlier messages" button when `hasMore()` is true. Extend the parent:

1. local pages first (current behaviour, `wa.listMessages(..., before)`);
2. when the local history is exhausted (`hasMore() === false`), show
   **"Fetch older messages from WhatsApp"** instead, which calls
   `wa.fetchHistory(jid, 200)`;
3. that request is answered minutes later by a blob, so react to the
   `history_imported` event (already on the `wa-event` stream): reload the
   newest page / prepend, and tell the user "N older messages arrived".
   Show a quiet "waiting for the phone…" state meanwhile — nothing arrives
   synchronously.

### 3.4 "Fetch all chats" (`features/chats/chat-list.component.ts`)

A button next to "Sync names" calling `wa.fetchHistory()` (no chat → every
known chat). Report `requested.length` and `skipped`. Note the honest
limitation in the UI copy: a chat the engine has **no** message for cannot be
anchored, so the full chat list really arrives with the initial history sync
(at pairing) — which is now imported automatically.

### 3.5 Tests to add

* `message-list.spec.ts` — media bubbles per kind, caption, error fallback,
  link rendering, day separators, `loadOlder` emission.
* `composer.spec.ts` — Enter vs Shift+Enter, disabled state, draft kept on
  failure, typing events.
* `contact-names.spec.ts` — the four name sources and the `@lid` fallback.
* `chat.component.spec.ts` — extend: load/append/mark-read, the escalation
  from local pages to `fetchHistory`.
* `bebok-server/tests/` — HTTP tests for `POST /history/fetch` (503 without a
  session, 400 on a bad JID, "no messages to ask from" skip) and
  `GET /media/{id}` (404 unknown id, 404 no attachment, cache hit served from
  `media_blobs` without a session).

---

## 3.6 Regressions fixed after the first draft of this file (keep them fixed)

* **Route order.** The messenger layout is an empty-path route *with children*.
  An empty-path parent matches any URL, and when no child matches the router
  does **not** fall through to the routes declared after it — the URL lands on
  the `**` wildcard instead. `/login`, `/settings` and `/about` all silently
  redirected to `/`, which looks exactly like being logged out. Every
  full-page route is now declared **before** the shell in `app.routes.ts`, and
  `app.spec.ts` navigates to each one to keep it that way.
* **The Login page did not know the `connecting` state** (added with the
  auto-reconnect) and fell into its "scan this QR to link a device" branch for
  an already-linked device. It now shows "This device is linked — connecting
  to WhatsApp…" and hides the pairing instructions when `wa.paired()`.
* **A bare 404 from the engine** is now reported as "This engine build has no
  `<path>` endpoint. Restart the engine…", because the engine answers its own
  404s with a JSON `{"error": …}` body. The user hit this with "Sync names"
  against an engine older than the client.

### A dev-server trap

After editing lazily-loaded routes, Vite's dependency optimizer in the running
`ng serve` can serve stale chunks: the browser then logs
`504 (Outdated Optimize Dep)` / `Failed to fetch dynamically imported module`
and the navigation aborts back to `/` — which looks like a routing bug but is
not. Ask the user to restart `npm run full-build-dev` (or clear
`client/.angular/cache`) before chasing it in the code.

## 4. Known problems worth chasing

* **`(no text)` bubbles.** In the user's `135124049993983@lid` chat every
  stored message has `type: "text"` and `text: null`. Those rows predate the
  `raw_json` change, so their payload is gone — but find out *why* the text
  was empty in the first place (a `protocolMessage`? an edit? a decryption
  that produced no body?). New messages now keep their raw payload, so
  inspect one with `GET /chats/{jid}/messages` and decode `raw_json`.
* **Own messages marked as incoming.** A message from the user's own device
  (`135124049993983:8@lid`) was stored with `from_me: false`, which puts it on
  the wrong side of the thread. Suspect the own-JID comparison for `@lid`
  identities in `parse_message_source` / `MessageInfo::is_from_me`.
* **Most devices fail to encrypt.** In a live send, 7 devices needed a
  pre-key bundle and 6 failed (`skipped_devices` now reports them). Only the
  device with an existing session got the message. Chase this with the user's
  `GET /debug/log` after a send — it is the difference between "sent" and
  "sent to one device".
* The media cache has no size cap, only the 1-hour TTL. If a chat with many
  photos becomes slow, add a byte budget to `purge_media_blobs`.

---

## 5. How to verify in the browser

Ask the user for a fresh `BEBOK_READY` link (never start the engine). Then the
`mcp__bebok__browser_*` tools work against `http://localhost:4200`:

```
browser_open http://localhost:4200/?engine=<url-encoded BEBOK_READY line>
browser_eval  …  // measure layout, scrollHeight/clientHeight, computed styles
browser_screenshot
browser_console level=error
```

The layout chain is fragile in exactly one way: every Angular host element
between `body` and `.thread-scroll` must be a flex item with `min-height: 0`.
If scrolling breaks again, dump the chain:

```js
let e = document.querySelector('.thread-scroll'), out = [];
while (e && e !== document.body) { const cs = getComputedStyle(e);
  out.push([e.tagName, e.className, e.clientHeight, cs.display, cs.flex, cs.minHeight]); e = e.parentElement; }
```
