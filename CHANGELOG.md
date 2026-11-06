# Changelog

All notable changes to this project. Format inspired by Bebok and Keep a Changelog.

## Unreleased

### Sending reached one device (own devices dropped out of the fan-out)

A sent DM went out with a single `<to>` — the recipient's device — and none
for our own. Sessions are stored under the **LID** address, the fan-out
resolved a device's identity through the LID map, and our own pair was the one
entry never in it: nothing wrote it, because the map is filled from the `_alt`
attributes of *incoming* messages and our own account does not send us any. So
our own devices stayed PN-addressed, found no session, hit "own device without
a session" and were dropped — the message reached the recipient but never the
user's own phone.

#### Fixed

- `Client::encryption_identity` now answers for our own account from the
  device itself (it knows both halves) when the LID map has nothing, and is
  the **single** resolver for both the fan-out and peer messages —
  `peer_encryption_identity` was a second implementation that had to agree
  with it.
- The own PN↔LID pair is stored on connect from the device's own `id`/`lid`
  instead of the `<success>` node's LID, which a reconnect does not always
  carry.
- The fan-out skips the device doing the sending (`fan_out_devices`, Go
  `encryptMessageForDevices`); our *other* devices stay in, since they are how
  a message reaches the user's phone.

#### Added

- `request_log` logs **both** directions: `GET /debug/log` now shows `wa<<`
  frames next to `wa>>`. An outgoing-only log turns every server rejection
  into guesswork — it cost two rounds of it on the `479` alone.

### History landed in a parallel chat (PN vs LID)

The phone answers a history request in **phone-number** form, while live
messages arrive **LID**-addressed — so an imported chat opened a second thread
next to the real one. In the test account the split was exact and contiguous:
`48600765779@s.whatsapp.net` held 200 imported messages up to 1793959467, and
`192680906768485@lid` the 29 live ones from 1793962644 — the same
conversation, two JIDs, and the open thread looked untouched after a fetch
that had in fact worked.

#### Added

- `Client::store_lid_pn_mapping` (Go `StoreLIDPNMapping`), called from the
  message path for `senderAlt`/`recipientAlt` and on connect for our own two
  identities. The LID map was **empty** until now — nothing ever wrote to it,
  so no PN could be resolved to a LID.
- `history_import::merge_split_chats` — a PN-keyed chat whose LID the engine
  knows is moved into the LID chat (`Store::merge_chat`, `chat_jids_stored`).
  It runs after every import and once at startup, and a message ID present in
  both keeps the **live** copy, which knows the sender's device and push name.
  A PN the LID map does not know yet is worth one `usync` round trip
  (`get_user_info` stores the mapping it answers with) before giving up.
- The import itself now canonicalises every conversation and sender JID
  through the LID map, so new blobs land in the right thread instead of
  needing the merge.

#### Fixed

- Unencrypted (channel) media is not downloadable the normal way: a
  `@newsletter` post carries a direct path and a plaintext hash but **no**
  media key, and the download refused it with "the media key is 0 bytes".
  `Downloadable::is_plaintext` now takes the Go branch — no HMAC, no CBC, just
  the plaintext hash check.

### Peer messages, the stored message kind and the app-state keys

Found with the engine running against the live account (`GET /debug/log` plus
the engine's two SQLite files), and confirmed against a HAR of
web.whatsapp.com: the CDN requests there carry exactly our URL shape
(`{directPath}&hash=…&mms-type=md-app-state|md-msg-hist&__wa-mms=`).

#### Fixed

- **Every peer message failed** with `no signal session with
  <own phone number>`, which is what "Fetch older messages from WhatsApp"
  reported for each chat. A peer message is *addressed* to our own phone
  number, but the Signal session with our own devices is the **LID** one
  (`135124049993983@lid` here, not `48513483482@s.whatsapp.net`), so the
  stanza has to be encrypted for the LID identity — Go
  `preparePeerMessageNode` maps the PN through `Store.LIDs`, and for our own
  account the device already knows its LID (the LID map can still be empty).
  `send_peer_message` now does that, and carries `privacy_sensitive="1"` on a
  history-on-demand request like Go.
- **An `<enc>` node without `v="2"` is rejected with `<error code="479">`**
  (`smax-invalid`). Go sets `v` inside `encryptMessageForDevice`, so every
  stanza has it; in this port the fan-out callers set it, and a peer message
  has no such caller — so the history request went out as
  `<enc type="msg">` and the server refused it. The default now lives in
  `send::enc_attrs` (caller attributes still override, like Go's `maps.Copy`),
  which is also the single source of `ENC_VERSION`.
- **A rejected app-state key request burned its daily slot.** The throttle
  marked a key as "asked" before the send; a `479` then blocked the retry for
  24 hours (only an engine restart cleared it, because the history is
  in-process). The mark now happens after a successful send.
- **Nothing ever asked for the app-state sync keys**, so every collection
  decoded to nothing and "Sync names" reported "0 contacts, 0 errors" — the
  device had **zero** keys, and the mutations are encrypted with keys only the
  primary phone has. `Client::request_app_state_keys` (Go
  `requestAppStateKeys`) now asks for the missing IDs after a sync, at most
  once a day per key; the answer is an `AppStateSyncKeyShare`, which the
  client already persists. The key request is itself a peer message, so it
  needed the fix above first.
- **`SyncReport` hid what went wrong.** It reported only `errors`, and a sync
  that applies nothing produces no error — just skips. It now carries
  `skipped` and `requestedKeys`, and the chat list says "this device is still
  missing N app-state key(s); the phone was asked — try again in a few
  seconds" instead of "Synced 0".
- **Attachments never rendered**, because the stored kind came from the
  stanza: `<message type="media">` says only "this is an attachment", so every
  photo, video and voice note sat in the database as `media` and the thread
  drew `📎 media`. The kind is now derived from the payload
  (`history_blob::media_kind_from_bytes`), a GIF and a voice note get their
  own kinds (`gif`, `ptt`, matching how the UI plays them), and
  `history_import::repair_message_kinds` recovers the kind of rows already
  stored — once, at startup. A row whose payload was never kept stays `media`:
  there is nothing left to download.

### Media bubbles, on-demand history and the external app-state snapshot

#### Added

- Client — attachments render in the bubble (`features/messages/message-list.component.ts`):
  images and stickers as `<img>` (max 320px, click for full size), videos and
  GIFs as `<video controls preload="metadata">` (a GIF loops muted, nothing
  autoplays), audio and voice notes as `<audio controls preload="none">`, a
  document as a download link; the caption stays under the media. A failed
  download falls back to the old label plus a retry. Only a message whose
  payload the engine kept (`raw_json`) renders as media — the rest would 404.
- Client — "load earlier" now escalates (`chat.component.ts`): local pages
  first, then **"Fetch older messages from WhatsApp"** (`POST /history/fetch`),
  a quiet "waiting for the phone…" state, and a prepend plus "N older
  message(s) arrived" when the `history_imported` event lands. A **"Fetch
  history"** button next to "Sync names" does it for every known chat.
- Client service — `fetchHistory(chat?, count?)`, `mediaUrl(id)` (the
  capability token rides in the query, since a media element cannot send an
  `Authorization` header) and `mediaInfo(id)`; `core/format.ts` gained
  `mediaRender(kind)`.
- Tests — `message-list.spec.ts` (new, 24 specs: one per media kind, caption,
  error fallback and retry, the escalation states, day separators, author
  runs), `chat.component.spec.ts` + `chat-list.spec.ts` + `format.spec.ts` +
  `whatsapp.service.spec.ts` extended, and `bebok-server/tests/history.rs`
  (new, 8 HTTP tests for `/history/fetch` and `/media/{id}`). Client 109 → 148
  specs, engine 536 → 546 tests.

#### Fixed

- **"Sync names" failed for every collection** with `invalid app state patch
  list: the snapshot is external but cannot be downloaded`. A snapshot does not
  fit in a stanza, so WhatsApp sends an `ExternalBlobReference` and keeps the
  bytes on the media CDN; `fetch_app_state_patches` passed no downloader at
  all. It now collects every external reference in the `<collection>`,
  downloads it through the verified media path (`MediaType::AppState`, Go
  `downloadExternalAppStateBlob`) and hands the bytes to the synchronous
  decoder.
- A slow `open()` of a chat could wipe a newer one: both resolve, and the
  `jid !== this.jid()` guard does not catch two opens of the *same* chat (the
  route effect re-runs when the engine comes online). Each open now carries a
  generation and only adopts its page while that generation is current.
- `POST /history/fetch` reported `503 not connected` for a malformed JID,
  because the session was checked before the request was validated.
- `routes::groups::parse_jid` accepted `user@` and `@server`, neither of which
  can address a chat — a request with one would go out with an empty `to`.

### Phase 6 — Polish and release (in progress)

Plan and done criteria: `TODO.md` Phase 6, `docs/PHASE-6-GAP.md`.

- [x] **T1** — Deferred protocol ports: history sync (`WaEvent::HistorySync`, no `wa-store` backfill), app-state key shares (persist + apply), LID-migration payloads (→`CachedLIDMap`), bot/`msmsg` decryption (`crypto/msgsecret.rs`), placeholder resends, `NewsletterMessageMeta`, `UseRetryMessageStore`, `retryFrame`. Gate: `grep TODO(Phase6)` is empty (0 markers).
- [x] **T2** — Server wiring: `Client` in `AppState` (no 503 when connected), `GET /chats` → `ChatListEntry[]` (live mute/pin/archive), `POST /logout`, `/blocklist`, `/block`, `/unblock`, `/contact(s)`, `/picture`, `GET /debug/log`, redacted `GET /config`.
- [x] **T3** — Settings / About / Stats UI: providers + redacted config + log tail (`BEBOK_DIAGNOSTIC=1`), client/engine version + `protocolRef` sha, local library stats (About).
- [x] **T4** — Code intelligence: `code_index` (status + search), `code_ast` (status + search), `code_graph`, `code_map` (`routes/codeintel.rs`, offline heuristic, `BEBOK_PROJECT_ROOT`).
- [x] **T5** — Release: `scripts/release.mjs` (version/checksums), `SHA256SUMS.txt`, Tauri bundles via `npm run full-build-app` (sub-steps verified; one full run still needed).
- [x] **T6** — Docs: `CONTRIBUTING.md` (setup, build/test commands, IPC/store/frontend rules, the `AGENTS.md` §12 PR checklist); this changelog brought up to date with Phase 5.
- [x] **T7** — E2E: Playwright (QR flow, send, group create) against mock engine (`e2e/`, 8 passed / 2 fixme); CI jobs proposed (e2e + scripts smoke), `.github/workflows/ci.yml` untouched.

### Phase 5 — Groups, AppState, Presence (done)

#### Added

- `wa-core/src/appstate/` — port of `whatsmeow/appstate/`: `lthash.rs` (LTHash), `hash.rs` (patch/snapshot MACs), `decode.rs` / `encode.rs` (mutation decode + encode, roundtrip-tested), `keys.rs` (key requests/shares on top of `AppStateSyncKeyStore`), `recovery.rs` (patch recovery, `PatchList` / snapshot recovery), `mod.rs`.
- `wa-core/src/client/appstate.rs` — app-state sync driver (fetch + apply patches per collection, `AppStateSyncProgress` event); a collection that needs a key we do not have stays unsynced with a reason.
- `wa-core/src/client/group.rs` — `CreateGroup`, `GetGroupInfo`, `JoinGroupWithLink`, `LeaveGroup`, `UpdateGroupParticipants`, `SetGroupName` / `SetGroupTopic`, `GetGroupInviteLink`, the `groupCache` and `CanSend`.
- `wa-core/src/client/newsletter.rs` — newsletter subscribe/unsubscribe, live updates, join/leave.
- `wa-core/src/client/presence.rs` — `SubscribePresence`, `SendPresence`, `SendChatPresence`.
- `wa-core/src/client/privacysettings.rs` — `GetPrivacySettings` / `SetPrivacySettings`.
- `wa-core/src/client/notification.rs`, `client/user.rs` — group/newsletter/privacy notification dispatch and user queries.
- `WaEvent` variants (`types/events.rs`, specta-exported): `GroupInfo`, `JoinedGroup`, `Presence`, `ChatPresence`, `PrivacySettings`, `NewsletterLiveUpdate`, `AppStateSyncProgress`.
- `wa-store` — chat-settings read path: `Store::list_chats_with_meta` → `ChatListEntry` (mute/pin/archive, archive filter); no schema change.
- `bebok-server` — `routes/groups.rs` (`POST /group/create`, `POST /group/join`, `GET /group/{jid}/info`, `POST /group/{jid}/leave`, `/participants`, `/name`, `/topic`, `GET /group/{jid}/invite-link`, `POST /newsletter/subscribe` / `unsubscribe`) and `routes/presence.rs` (`POST /presence/subscribe`, `/presence/send`, `/presence/chat`, `GET`/`POST /privacy`) — see `API-WHATSAPP.md` §§3.3–3.5.
- Tauri commands (`client/src-tauri/src/lib.rs`): `group_create`, `group_info`, `group_leave`, `group_participants`, `group_set_name`, `group_set_topic`, `group_invite_link`, `group_join`, `newsletter_subscribe`, `newsletter_unsubscribe`, `presence_subscribe`, `presence_send`, `presence_chat`, `privacy_get`, `privacy_set`.
- Client — `features/chats/chat-list.component.ts` (mute/pin/archive, sorting, presence indicators), `features/groups/` (`group-create`, `group-info` with participants + invite link, `groups.api.ts`, `groups.routes.ts`).
- Tests: wa-core 261 → 383 (LTHash and patch-MAC golden vectors, encode/decode roundtrips, group cache / `CanSend`, presence subscribe/send), wa-store 23, bebok-server 33, client 51 → 68 (`chat-list.spec.ts` sort + mute/pin, `group-info.spec.ts`). `cargo fmt`, `clippy` and `ng build` clean.

#### Deferred to Phase 6

Eight `TODO(Phase6)` markers (`client/notification.rs` ×7, `client/appstate.rs` ×1): history sync import, app-state key shares, LID-migration payloads, bot/`msmsg` decryption, placeholder resends, `NewsletterMessageMeta`, `UseRetryMessageStore`, `retryFrame` — see `TODO.md` Phase 5 **Deferred**. The HTTP group/presence/privacy routes still return 503 until the `Client` is wired into the server state (Phase 6 T2).

### Added

- `wa-core/src/types/message.rs` — `MessageID` (`Display`/`FromStr`), `MessageSource`, `MessageInfo` (port of `whatsmeow/types/message.go`).
- `wa-core/src/types/group.rs` — `GroupName` struct plus the `ParticipantChange` / `ParticipantRequestChange` enums, with wire strings pinned by "every Go constant appears" drift tests (port of `whatsmeow/types/group.go` + `whatsmeow/group.go`).
- `wa-core/src/types/presence.rs` — `PresenceState`, `ChatPresenceState`, `ChatPresenceMedia` (port of `whatsmeow/types/presence.go`).
- `wa-core/src/socket/` — the transport layer of Phase 2: `FrameSocket` (WA connection header, 3-byte length framing, frame reassembly over `tokio-tungstenite`), `NoiseHandshake` (the WhatsApp `Noise_XX_25519_AESGCM_SHA256` variant, hand-rolled because the 32-byte padded pattern must be used verbatim) and `NoiseSocket` (encrypted frame layer with `write`/`read` keys and counters). Ports of `whatsmeow/socket/{framesocket,noisehandshake,noisesocket}.go`.
- `wa-core/src/keepalive.rs` — the keep-alive loop (20–30 s randomized ping, 10 s response deadline, 3 min max-fail-time → reconnect) emitting `KeepAliveTimeout` / `KeepAliveRestored` (port of `whatsmeow/keepalive.go`).
- Phase 2 golden vectors for the Noise handshake and key schedule, generated with `go run` against `whatsmeow/socket` (the temporary generator was deleted afterwards)
- `wa-core/src/client.rs` — `do_handshake` (the WhatsApp Noise handshake: `ClientHello` → `ServerHello` → certificate chain verification → `ClientFinish` → session keys), `verify_server_cert` with `WACertPubKey`/`WACertIssuerSerial`, `HandshakeOptions` (response timeout, CA override, validity check) and the `ClientError`/`CertError` enums (port of `whatsmeow/handshake.go`).
- `wa-core/src/types/version.rs` — `WaVersion`, `DeviceProps` and `build_client_payload`: the minimum `ClientPayload` a first connect needs to be well-formed (protocol version, user agent, `WebInfo`, connect type/reason). The device-registration fields are a documented Phase 4 TODO.
- `bebok-server`: `connection.rs` (the session handle behind `POST /connect` — dial + handshake in the background, state reported as JSON and via `WaEvent`), `POST /connect` (was 501), `GET /events` wired to the connection, `GET /connected`; new `--wa-url` / `--handshake-timeout` flags (env `BEBOK_WA_URL`, `BEBOK_HANDSHAKE_TIMEOUT_SECS`).
- Tests: engine 154 → 183 (`client` handshake suite with a synthetic server peer + a replayed Go golden transcript, `types::version`, `bebok-server` `/connect` + `/events`), client/src-tauri 4, client 51 — all offline.

- Repo scaffolding: `whatsmeow/` (Go reference), `engine/` (Rust workspace), `client/` (Angular + Tauri 2) — see `ARCHITECTURE.md`.
- Documentation: `AGENTS.md`, `README.md`, `ARCHITECTURE.md`, `API-WHATSAPP.md`, `CHANGELOG.md`, `TODO.md`.
- Tauri IPC + HTTP/SSE contract (`API-WHATSAPP.md`): `connect`, `qr`, `pair_code`, `send_message`, `send_media`, `group/*`, `presence/*`, `WaEvent` (specta).
- whatsmeow Go → Rust port plan (hybrid: Go sidecar → iterative module port) — `TODO.md` Phases 0–6.
- Engine workspace (edition 2024, Rust 1.88): `wa-proto` (prost types from all `whatsmeow/proto` files, vendored `protoc`), `wa-core` (`Jid`, `WaEvent`, store traits, sqlx `Container` with `_foreign_keys=on` + `wa_version`), `wa-store`, `bebok-server`.
- Lightweight local database from day 0: `engine/crates/wa-store/` (SQLite + FTS5 trigram + sqlite-vec 384d; MiniLM via `--features fastembed`, offline) — keyword / vector / hybrid (RRF) search without a server, `GET /search` — see `ARCHITECTURE.md` §6, `API-WHATSAPP.md` §3.8.
- `bebok-server` (axum): `BEBOK_READY`, capability token, CORS, `GET /version`, `GET /config` (redacted), `GET /events` (SSE), `GET /search`, `GET /chats/:jid/history/export` (json/jsonl/csv stream, `Content-Disposition`), `GET /chats/:jid/stats`, `DELETE /chats/:jid/messages`, `POST /debug/seed` (diagnostic only). `/connect`, `/disconnect`, `/logout` return 501 until Phases 2–3.
- "Download full chat history" button in `chat.component`: JSON/JSONL/CSV picker, progress (`ExportProgress`), error state. Browser mode downloads the blob from `GET /chats/:jid/history/export?format=…` under the `Content-Disposition` filename; Tauri mode calls the native `export_chat_history` command, which streams the same endpoint into the Downloads folder and returns the saved path — see `API-WHATSAPP.md` §3.8.
- `client/` — Angular 20 standalone + signals + `@tauri-apps/api`; `app.routes.ts` (`''`, `chat/:jid`, `settings`, `about`, `browser-view`), `core/services/tauri.service.ts` (Tauri `invoke`/`listen` + browser `fetch`/`EventSource`), `core/services/whatsapp.service.ts` (`connect`, `search_messages`, `export_history`, `connectionState`/`engineStatus` signals, SSE `wa-event`), `features/` (chat list, chat + export, search bar, settings, about, browser-view, QR placeholder), `testing/fakes.ts` (mock `invoke`/`listen`/`EventSource`).
- `client/src-tauri/` — Tauri 2 shell: `tauri.conf.json` (externalBin `bebok-server` sidecar, icons), `capabilities/default.json`, `src/main.rs` + `src/lib.rs` (spawns the sidecar with a random port, feeds the engine URL to the webview), icons generated via `tauri icon`.
- `scripts/bebok.mjs` — `doctor`, `engine`, `client`, `full-build-dev` (engine :8787 + ng serve :4200 + `?engine=` handshake), `full-build-app` (release + sidecar copy + Tauri bundles + SHA256SUMS.txt); `scripts/copy-sidecar.mjs` — copies `engine/target/release/bebok-server` → `client/src-tauri/binaries/bebok-server-<triple>`.
- CI — `.github/workflows/ci.yml`: `cargo test`+`clippy`+`fmt` on Linux/Windows/macOS, `client` `ng build`+`npm test` (ChromeHeadless).
- Root `package.json` — `doctor`, `engine`, `client`, `full-build-dev`, `full-build-app`, `sidecar:copy`, `test` (engine + client); `.nvmrc` (Node 22); root `.gitignore`.
- `specta` 1.0 bindings: `WaEvent` (`wa-core`), `StoredMessage`/`SearchMode`/`SearchHit`/`ChatStats`/`ChatSummary` (`wa-store`) derive `specta::Type`; `npm run specta` (`bebok-server/src/bin/export-types.rs`) writes `client/src/app/types/specta.ts`, re-exported by `core/models.ts`; `bebok-server/tests/specta_bindings.rs` fails CI when the committed bindings drift.
- Client tests (51): `app.spec.ts` (bare-route), `whatsapp.service.spec.ts` (mock invoke/listen, export per format in both run modes), `tauri.service.spec.ts` (fetch + SSE), `chat.component.spec.ts` (button, format picker, progress, error), `search-bar.component.spec.ts`.

### Phase 1 — Store + Binary (done)

- `wa-core/src/store/traits.rs`: async ports of every `whatsmeow/store/store.go` interface (`Device`, `IdentityStore`, `SessionStore`, `PreKeyStore`, `SenderKeyStore`, `AppStateSyncKeyStore`, `AppStateStore`, `ContactStore`, `ChatSettingsStore`, `MsgSecretStore`, `PrivacyTokenStore`, `NCTSaltStore`, `EventBuffer`, `LIDStore`, `Container`).
- `wa-core/src/store/sqlite/`: `Container` (`new`, `upgrade`, `get_all_devices`, `get_device`, `get_first_device`, `put_device`, `delete_device`, `new_device`), `SqlStore` (all session-scoped stores), global `CachedLIDMap` with bulk lookups and `migrate_pn_to_lid`.
- `wa-core/migrations/`: full `wa_`-prefixed port of `whatsmeow/store/sqlstore/upgrades/00-latest-schema.sql` (verified table-by-table against the Go schema), `wa_version`, `foreign_keys=on`.
- `wa-core/src/crypto/keys.rs`: X25519 key pairs (clamped scalars) plus the libsignal Curve25519 signature used for signed pre-keys.
- `wa-core/src/binary/`: `token.rs` (generated from Go), `node.rs` (+ XML rendering with credential redaction), `attrs.rs` (`AttrGetter`), `encoder.rs`, `decoder.rs`, `unpack.rs` (zlib). Verified against golden vectors produced by running the Go implementation.
- `types/jid.rs`: AD-JID/LID parse+format plus the small Go helpers (`to_non_ad`, `is_bot`, `is_group`, `with_server`, `with_device`, `user_int`, …).
- `types/message.rs`, `types/group.rs`, `types/presence.rs`: Phase 1 message metadata and group/presence wire enums.

### Changed

- **Renamed the project to "ngWhatsapp"** (was "WazzApp", originally `whatsapp-angular`). Same scope as the
  previous rename: engine/tooling names (`wa-core`, `wa-store`, `wa-proto`, `wa_*` tables, `wa-core.db`,
  `bebok-server`, `BEBOK_READY`) are unchanged.
  Package/crate names use lowercase: root `package.json` `wazzapp` → `ngwhatsapp`, `client/package.json` +
  `package-lock.json` `wazzapp-client` → `ngwhatsapp-client`, `client/src-tauri` package `wazzapp` → `ngwhatsapp`
  and lib `wazzapp_lib` → `ngwhatsapp_lib` (+ `Cargo.lock`, `main.rs`), `tauri.conf.json` `productName`/window title
  → `ngWhatsapp` and identifier `dev.bebok.wazzapp` → `dev.bebok.ngwhatsapp` (a new app-data directory on desktop),
  `index.html` title, `wa-core` `CLIENT_OS` (the device name shown under Linked Devices) → `ngWhatsapp`, and the
  headings/copyright in the docs and `docs/PROMPT-PHASE-{3,4}.md`.
- **The product is now called "WazzApp"** (it is still a local WhatsApp client). Renamed the product only —
  the engine/tooling names stay as they are: `wa-core`, `wa-store`, `wa-proto`, the `wa_*` SQLite tables,
  the `wa-core.db` file, the `bebok-server` sidecar binary and the `BEBOK_READY` stdout contract.
  Files touched: root `package.json` (`whatsapp-angular` → `wazzapp`), `client/package.json` +
  `client/package-lock.json` (`whatsapp-angular-client` → `wazzapp-client`), `client/src-tauri/Cargo.toml`
  (package `whatsapp-angular` → `wazzapp`, lib `whatsapp_angular_lib` → `wazzapp_lib`),
  `client/src-tauri/src/main.rs` (`wazzapp_lib::run()`), `client/src-tauri/tauri.conf.json`
  (`productName` + window title → `WazzApp`, identifier `dev.bebok.whatsapp-angular` → `dev.bebok.wazzapp`),
  `client/src/index.html` (`<title>WazzApp</title>`), headings/copyright in `README.md`, `AGENTS.md`,
  `ARCHITECTURE.md`, `TODO.md`, `THIRD_PARTY_NOTICES.md`, and the `bebok-server` / Tauri crate descriptions.
- Renamed the protocol crate `whatsmeow-rs` → `wa-core`, SQLite tables `whatsmeow_*` → `wa_*` (DB file `wa-core.db`), `GET /version` field `whatsmeowRef` → `protocolRef`; About page shows the author (Henryk Brzoska); added `THIRD_PARTY_NOTICES.md` (MPL-2.0 attribution).
- The application language is English (UI, code, tests, docs).
- `messages.chat_jid` no longer has a foreign key to `wa_device(jid)` — a chat JID is not a device JID, so every regular message insert would fail.

### Fixed

- `wa-store` test `replace_delete_and_stats`: KNN always returns the nearest remaining messages, so the test now checks that nothing from the deleted chat comes back instead of expecting an empty hybrid result.
- `chat.component.spec.ts`: `Response.blob()` resolves outside the Angular zone, so the export specs now flush pending microtasks instead of relying on `whenStable()` alone; the export button is found by `data-testid` (its label changes to "Exporting…").
- `marshal()` includes the frame data-type byte while `unmarshal()` expects the payload after `unpack()` — the same split as Go's `Marshal`/`Unpack`/`Unmarshal` (documented on both functions).

## 0.1.0 — 2026-11-05

### Added

- Initial documentation and architecture skeleton.
- Technology decisions: `tokio`, `tokio-tungstenite`, `snow` (Noise XX), `libsignal-protocol`, `prost`, `sqlx` (SQLite `_foreign_keys=on`), `tracing`, `tauri-specta`, Angular standalone + signals.
- Test strategy like Bebok: Rust `#[cfg(test)]` + `engine/tests/` (`cargo test`), Angular Karma+Jasmine (`ng test`), optional Playwright E2E — see `AGENTS.md` §5.
- Browser mode: engine `:8787` + `ng serve :4200` with the `?engine=` handshake and SSE (`GET /events`) — see `README.md` and `AGENTS.md` §4.

---

## Entry template (for future versions)

### Added — new features
### Changed — changes to existing features
### Deprecated — to be removed soon
### Removed — removed
### Fixed — bug fixes
### Security — vulnerabilities

Each entry: a short description + a module/file reference when relevant (e.g. `wa-core/store/sqlite`).
Versioning: SemVer (`MAJOR.MINOR.PATCH`), `Unreleased` on top.
