# ngWhatsapp — Angular + Tauri 2 + Rust

Phase plan like Bebok: each phase has done criteria and required tests (engine + client).
Browser mode is required from Phase 0 — every frontend change must work in Tauri and in `ng serve`.
The application language is English (UI strings, code, comments, tests, docs).

## Phase 0 — Scaffolding (1–2 weeks)

- [x] `cargo create-tauri-app` / `ng new` — `client/` (Angular 17+ standalone, signals), `client/src-tauri/` (`tauri.conf.json`, `capabilities/`, `icons/`)
- [x] `engine/Cargo.toml` workspace (`wa-proto`, `wa-core`, `wa-store`, `bebok-server`), `resolver="3"`, `edition="2024"`, `rust-version="1.88"`
- [x] `engine/crates/wa-proto` — `build.rs` (`prost_build::compile_protos` from `whatsmeow/proto/**/*.proto`, vendored `protoc`), `src/lib.rs`
- [x] `engine/crates/bebok-server` — `main.rs`/`lib.rs`, `routes/mod.rs` (routing table like Bebok), `GET /version`, `BEBOK_READY` on stdout, `BEBOK_ADDR`/`BEBOK_TOKEN`/`BEBOK_NO_AUTH`
- [x] `engine/crates/wa-core` — `lib.rs`, `store/traits.rs` (trait skeletons), `types/jid.rs`, `types/events.rs`
- [x] `scripts/bebok.mjs` — `doctor`, `full-build-dev` (engine :8787 + ng serve :4200 + `?engine=` handshake), `full-build-app` (release + sidecar + bundles), `engine`, `client` (copy from `bebok-1.9.2/scripts/bebok.mjs`)
- [x] `scripts/copy-sidecar.mjs` — copies `engine/target/release/bebok-server` → `client/src-tauri/binaries/`
- [x] `client/src/app/app.routes.ts` — `''`, `'chat/:jid'`, `'settings'`, `'about'`, `'browser-view'` (bare)
- [x] `client/src/app/core/services/tauri.service.ts` — `invoke`/`listen` wrapper (mocked in tests, `fetch`+`EventSource` in browser mode)
- [x] SQLite — `?_foreign_keys=on`, `migrations/` with `wa_version`
- [x] **Lightweight local database from day 0** — `engine/crates/wa-store/` (`rusqlite` + `sqlite-vec` + `fastembed` MiniLM 384d, offline): `messages`, `messages_fts` (FTS5 trigram), `message_embeddings` (vec0 cosine), `GET /search` (hybrid FTS+KNN RRF), `GET /chats/:jid/history/export` (json/jsonl/csv stream)
- [x] **"Download full chat history"** — `chat.component` button + JSON/JSONL/CSV picker, browser mode via `GET /chats/:jid/history/export` (saved under the `Content-Disposition` name), Tauri mode via the native `export_chat_history` command (streams into Downloads, returns the path), `ExportProgress`/error states (see `ARCHITECTURE.md` §6, `API-WHATSAPP.md` §3.8)
- [x] `specta` — `specta` 1.0 in `bebok-server`/`wa-core`/`wa-store`, `npm run specta` generates `client/src/app/types/specta.ts` (drift guarded by `bebok-server/tests/specta_bindings.rs`)
- [x] CI — `cargo test` + `npm test` on Linux/Windows/macOS (like Bebok)

**Done when:** `npm run full-build-dev -- --open` opens a browser connected to the engine, `GET /version` and `GET /search?q=test` work, the export button downloads a file, `cargo test` and `npm test` are green (skeletons).

**Tests:**
- Engine: `#[cfg(test)]` in `wa-proto` (proto compiles), `bebok-server` (BEBOK_READY, /version) ✅
- Client: `app.spec.ts` (bare-route like Bebok), `tauri.service.spec.ts` (mock invoke/listen) ✅ (51 client specs: app routing, services, chat/search specs)

## Phase 1 — Store + Binary (2–3 weeks)

- [x] Port `store/store.go` → `wa-core/src/store/traits.rs` (`IdentityStore`, `SessionStore`, `PreKeyStore`, `SenderKeyStore`, `AppStateSyncKeyStore`, `AppStateStore`, `ContactStore`, `ChatSettingsStore`, `MsgSecretStore`, `PrivacyTokenStore`, `NCTSaltStore`, `EventBuffer`, `LIDStore`, `Device`, `Container`) — async traits (`async_trait`)
- [x] `store/sqlite/container.rs` — `Container::new`, `upgrade` (migrations), `get_all_devices`, `get_device`, `get_first_device`, `put_device`, `delete_device`, `new_device` (NoiseKey, IdentityKey, AdvSecretKey), `initialize_device`
- [x] `store/sqlite/store.rs` — `SqlStore` implementing all traits, `CachedLIDMap` (global, not per-device, `store/sqlite/lid_map.rs`)
- [x] `sqlx` migrations from `store/sqlstore/upgrades/` → `wa-core/migrations/` (`wa_device`, `wa_version`, sessions, identities, prekeys, sender_keys, appstate, contacts, chat_settings, lid_map, msg_secrets, privacy_tokens, event_buffer)
- [x] `crypto/keys.rs` — X25519 `KeyPair` (clamped private keys) + the libsignal Curve25519 signature used for signed pre-keys
- [x] Port `binary/` — `node.rs` (+ `xml.rs` rendering), `attrs.rs`, `encoder.rs`, `decoder.rs`, `token.rs` (generated from Go), `unpack.rs`
- [x] `types/jid.rs` — `JID` (user/server/agent/device/integrator), AD-JID + LID parse/format, `FromStr`/`Display`, Go-parity helpers (`to_non_ad`, `is_bot`, `with_server`, …)
- [x] `types/message.rs`, `types/group.rs`, `types/presence.rs` — `MessageID` (+ `MessageSource`/`MessageInfo` fields), `GroupName`/`ParticipantChange`/`ParticipantRequestChange`, `PresenceState`/`ChatPresenceState`/`ChatPresenceMedia` (Go constants pinned by drift tests)

**Done when:** `PutDevice`/`GetDevice` roundtrip works, `binary` encode/decode passes on Go dumps.

**Tests:**
- Engine: `store::sqlite` (Put/Get/Delete roundtrip, idempotent migrations, `foreign_keys=on` enforced, LID map lookups, cascade on delete) ✅, `binary` (encoder/decoder vs Go dumps — golden vectors generated with `go run` against `whatsmeow/binary`) ✅, `types::jid` (parse/format, AD-JID, LID) ✅, `types::{message,group,presence}` (MessageID parse/format, serde roundtrips, "every Go constant appears" drift tests) ✅
- Client: none (store not visible in UI)

## Phase 2 — Socket + Handshake (2–3 weeks)

- [x] `socket/frame_socket.rs` — `FrameSocket` (WebSocket `tokio-tungstenite`, `recv` channel, `set_on_disconnect`, `send_frame`, `connect`, `close`), WA connection header + 3-byte length framing + frame reassembly
- [x] `socket/noise_handshake.rs` — `NoiseHandshake` (Noise_XX_25519_AESGCM_SHA256 WhatsApp variant): `start(pattern, prologue)`, `authenticate`, `encrypt`/`decrypt`, `mix_shared_secret_into_key`, `mix_into_key`, `finish_keys`, `generate_iv(count)` 12B BE. Ported line-by-line instead of using `snow`: the WhatsApp pattern is exactly 32 bytes and is used *verbatim* as the initial hash, which a generic Noise library (SHA-256 of the bare 32-byte name) would get wrong.
- [x] `socket/noise_socket.rs` — `NoiseSocket` (`aes-gcm`, `write_cipher`/`read_cipher`, `write_counter`/`read_counter`, `send_frame`, decrypting consume task, `stop(disconnect, allow_on_disconnect)`)
- [x] `keepalive.rs` — `KeepAlive` loop (20–30 s randomized ping, 10 s response deadline, 3 min max-fail-time → reconnect), `KeepAliveTimeout`/`KeepAliveRestored` events
- [x] `client.rs` — `do_handshake(frames, ephemeral, noise, client_payload)` → `NoiseSocket`, plus `verify_server_cert` (port of `handshake.go:doHandshake` + `verifyServerCert`) and the `ClientError`/`CertError` enums. Certificate verification uses `WACertPubKey`/`WACertIssuerSerial` and `crypto::keys::verify_signature`; nothing panics outside tests.
- [x] `types/version.rs` — `WaVersion` (`ParseVersion`/`LessThan`/`ProtoAppVersion`), `DeviceProps` and `build_client_payload` (`store.BaseClientPayload`, registration flavour): protocol version, user agent (`WEB`/`RELEASE`, `2.3000.1052556849`), `WebInfo`, `ConnectType::WIFI_UNKNOWN`, `ConnectReason::USER_ACTIVATED`, `passive = false`. Pinned against a golden vector from the Go reference.
- [x] `bebok-server` — `POST /connect` (starts the session, returns the state immediately; dial + handshake run in the background and report via `/events`), `GET /events` (SSE), `GET /connected` backed by the connection handle. `--wa-url` / `--handshake-timeout` (env `BEBOK_WA_URL` / `BEBOK_HANDSHAKE_TIMEOUT_SECS`) make the endpoint testable against a local socket.
- [ ] `POST /disconnect`, `POST /logout` — still 501 (Phase 3 wiring)

**Done when:** connecting to `web.whatsapp.com/ws/chat` + handshake + receiving `stream:error`/`success` works (log).

**Tests:**
- Engine: `client` (`do_handshake` against a synthetic server implementing the server half of XX — full transcript, both directions decryptable, counters; Go golden transcript replayed byte for byte: `ClientHello`, `ClientFinish` and the derived session keys all match; timeout → `HandshakeTimeout`; truncated `ServerHello` (missing static/payload, 31-byte ephemeral) rejected; foreign CA rejected; `verify_server_cert` accepts a sound chain and rejects tampering, key mismatch and expiry) ✅, `types::version` (version parse/compare/proto, payload well-formedness + golden vector) ✅, `socket::noise_handshake` (handshake + key-schedule vectors generated with `go run` against `whatsmeow/socket`, encrypt/decrypt roundtrip, tamper rejection, `generate_iv` golden values) ✅, `socket::noise_socket` (local WS server: encrypted send/decrypt receive, counters, idempotent `stop`, expected-close semantics) ✅, `socket::frame_socket` (local WS server: WA header sent once, length framing, split/packed message reassembly, remote disconnect) ✅, `keepalive` (timeout/restored, reconnect hook, response deadline) ✅
- Client: `whatsapp.service.spec.ts` (mock SSE, `connectionState` signal)
- HTTP: `bebok-server/tests/http.rs` — `/connect` requires the token, returns before the session settles, surfaces a failing attempt as `connect_failure` on `/events`, and never stacks sessions ✅

**Deferred:** the `pair_*`/`qr` node handling after the handshake (`success`, `failure`, `stream:features`) and the device-registration fields of `ClientPayload` (`eRegid`, `eIdent`, pre-keys) — Phase 3/4; see the TODO in `types/version.rs`.

## Phase 3 — Client Core (3–4 weeks)

- [ ] `client.rs` — `struct Client` (40+ fields: `socket`, `responseWaiters: HashMap<String, oneshot>`, `eventHandlers: RwLock<Vec>`, `groupCache`, `recentMessages: RwLock<RecentMessages 512>`, `isLoggedIn`, `enableAutoReconnect`, `appState`, `mediaConnCache`, `privacySettingsCache`)
- [ ] `connection.rs` — `Connect`/`ConnectContext`, `Disconnect`, `unlockedConnect`, `autoReconnect` (backoff `AutoReconnectErrors*2s`), `WaitForConnection`, `onDisconnect`, `isRetryableConnectError`
- [ ] `pair.rs` — `QR` (Codes, rotation 60s/20s), `PairSuccess`, `PairError`, `LoggedOut`, `ConnectFailure`, `StreamError`, port of `pair-code.go`/`pair-passkey.go`
- [ ] `types/events.rs` — all events from `types/events/events.go` (serde + specta, `#[serde(tag="type")]`)
- [ ] Tauri commands + HTTP: `connect`, `disconnect`, `is_connected`, `is_logged_in`, `wait_for_connection`, `logout`, `get_qr_channel`
- [ ] `client/src/app/features/auth/qr.component.ts` — render QR (`qrcode` lib), states `disconnected`/`qr`/`connected`/`logged_out`
- [ ] `client/src/app/core/services/whatsapp.service.ts` — `connect()`, `onQR()`, `onConnected()`, `connectionState` signal

**Done when:** QR shows in Tauri and in the browser (`ng serve`), scan → `PairSuccess` → `Connected`, disconnect → `autoReconnect`.

**Tests:**
- Engine: `client::connection` (Connect/Disconnect, responseWaiters, handlerQueueLoop 2048, autoReconnect), `pair` (QR channel, PairSuccess)
- Client: `qr.component.spec.ts` (render QR, rotation, states), `whatsapp.service.spec.ts` (invoke connect, listen wa-event)

## Phase 4 — Messaging + Crypto (4–6 weeks, the hardest)

- [ ] `crypto/` — `keys.rs` (`KeyPair`, `PreKey`), `hkdf.rs`, `gcm.rs`, `cbc.rs` (port of `util/keys`, `hkdfutil`, `gcmutil`, `cbcutil`)
- [ ] Signal — an **independent, spec-based implementation** of the Signal protocol (X3DH key agreement + Double Ratchet, sender keys), written from the published Signal specification instead of linking the `libsignal-protocol` crate (**GPLv3** — incompatible with this repo's MPL-2.0/third-party notices): `SessionStore`/`IdentityStore`/`PreKeyStore`/`SenderKeyStore` on `SqlStore`, `IsTrustedIdentity`, `PutIdentity`. `go.mau.fi/libsignal` is used only as a behaviour reference and for test vectors.
- [ ] `message.rs` — `handleEncryptedMessage`, `handleUnencryptedMessage`, `ParseWebMessage`, `UnwrapRaw` (DeviceSent/Ephemeral/ViewOnce/ViewOnceV2/DocumentWithCaption/LottieSticker/Edited), `FBMessage`
- [ ] `send.rs` / `retry.rs` / `mediaretry.rs` — `SendMessage`, `SendMedia`, retry receipts, `GetMessageForRetry`, `PreRetryCallback`, `UseRetryMessageStore`, `recentMessagesMap` 512
- [ ] `upload.rs` / `download.rs` / `mediaconn.rs` — `Upload`/`Download` (mediaHTTP vs websocketHTTP), `MediaConn` cache, port of `download-to-file.go`
- [ ] `receipt.rs` — `handleReceipt`, `Receipt` event, `MessageIDs` ordering (Android vs iOS)
- [ ] Tauri commands: `send_message`, `send_media`, `fetch_history`, `get_message`, `mark_read`, `mark_played`, `upload_media`, `download_media`
- [ ] `client/src/app/features/messages/` — `message-view` (virtual scroll), `composer` (send, retry), `media-preview`
- [ ] Every incoming/sent message → `wa-store::insert_message` (search + export)

**Done when:** sending/receiving text and media works E2E, retry receipts pass, `UndecryptableMessage` only when keys are missing.

**Tests:**
- Engine: `crypto` (HKDF/GCM/CBC vectors), `signal` (session encrypt/decrypt, prekeys), `message` (UnwrapRaw, ParseWebMessage), `send` (retry, recentMessages), `upload/download` (mock HTTP)
- Client: `message-view.spec.ts` (render, view-once, ephemeral), `composer.spec.ts` (send, error)

## Phase 5 — Groups, AppState, Presence (2–3 weeks)

- [x] `client/group.rs` — `CreateGroup`, `GetGroupInfo`, `JoinGroupWithLink`, `LeaveGroup`, `UpdateGroupParticipants`, `SetGroupName/Topic`, `GetGroupInviteLink`, `GroupInfo`/`JoinedGroup` events, `groupCache`
- [x] `client/newsletter.rs` — subscribe/unsubscribe, `NewsletterLiveUpdate`, `NewsletterJoin/Leave`
- [x] `client/presence.rs` — `SubscribePresence`, `SendPresence`, `SendChatPresence`, `Presence`/`ChatPresence` events
- [x] `client/privacysettings.rs` — `GetPrivacySettings`/`SetPrivacySettings`, `PrivacySettings` event
- [x] `appstate/` — `mod.rs`, `hash.rs` (LTHash), `decode.rs`/`encode.rs`, `keys.rs` (key requests/shares + `AppStateSyncKeyStore`), `recovery.rs` (patch recovery, `PatchList`/`SnapshotRecovery`), `lthash/` (port of `whatsmeow/appstate/`)
- [x] `client/src/app/features/chats/` — `chat-list` (mute/pin/archive, `ChatSettingsStore`), `group-info`, presence indicators
- [x] `client/src/app/features/groups/` — create, invite link, participants

**Done when:** groups, newsletters, presence and AppState sync work, `chat-list` shows mute/pin/archive.

**Tests:**
- Engine: `group` (cache, CanSend), `appstate` (LTHash, encode/decode roundtrip), `presence` (subscribe/send)
- Client: `chat-list.spec.ts` (sort, mute/pin), `group-info.spec.ts`

**Deferred** (explicitly out of Phase 5: the module above lands with a `TODO(Phase6)` marker where one of these is needed, and the work moves to Phase 6):

- **History sync** — `HistorySyncNotification` is parsed and emitted as an event, but is **not** imported into `wa-store` (no `insert_message` backfill of conversations/messages from `HistorySync` blobs).
- **App-state key shares** — `AppStateSyncKeyShare` is not persisted; only self-generated keys are handled, so patches from another device that require a shared key stay unapplied (`appstate/keys.rs` + `AppStateSyncKeyStore`).
- **LID-migration payloads** — `lid_migration` (`PN → LID` migration notifications) are decoded but not applied to `CachedLIDMap` / `wa_lid_map`; the schema column `lid_migration_ts` is already written by `store/sqlite/container.rs`.
- **Bot decryption** — `msmsg`/`msgsecret` (`whatsmeow/msgsecret.go`) is not ported, so bot messages encrypted with a `msmsg` secret arrive as `UndecryptableMessage`.
- **Placeholder resends** — placeholder resend requests (a `ProtocolMessage` reply asking the peer to re-encrypt a message we could not decrypt) are not sent.
- **`NewsletterMessageMeta`** — newsletter message metadata (views/reaction counts) is not parsed on incoming newsletter messages.
- **`UseRetryMessageStore`** — retry receipts do not consult the store; only the in-memory `recentMessages` (512) map is used, so a retry for a message after a restart is lost.
- **`retryFrame`** — the generic retry-frame helper (a backoff wrapper around a failed frame, as in `whatsmeow`) is not implemented; only message-level retry receipts exist. Tracked as `TODO(Phase6)` where a call site needs it; otherwise it folds into the Phase 4 `retry.rs` work.

## Phase 6 — Polish and release (2 weeks)

- [x] Deferred protocol ports — all 8 `TODO(Phase6)` markers removed: `retryFrame` (`client/retry.rs`), `UseRetryMessageStore` + store-backed receipts, `HistorySyncNotification`→`WaEvent::HistorySync` (no wa-store backfill), `msgsecret.rs` + bot decrypt, placeholder resends, LID migration→`CachedLIDMap`, `NewsletterMessageMeta` parse, key-share persist + patch apply
- [x] Server wiring — `Client` in `AppState` (no 503 when connected), `POST /logout`, `GET /chats`→`ChatListEntry[]` (mute/pin/archive live), `/blocklist`, `/block`, `/unblock`, `/contact(s)`, `/picture`, `GET /debug/log`, redacted `/config`
- [x] `client/src/app/features/settings/` — providers, `GET /config` (redacted secrets), `GET /debug/log` (`BEBOK_DIAGNOSTIC=1`)
- [x] `client/src/app/features/about/` — `GET /version` (`protocolRef` sha) + local library stats
- [x] `client/src/app/features/stats/` — covered by About "Local library" aggregate (optional per-chat stats already in chat.component)
- [x] Code intelligence — `code_index` (status + search), `code_ast` (status + search), `code_graph`, `code_map` (`routes/codeintel.rs`, offline heuristic)
- [x] `package:mcp` / `release` — `scripts/release.mjs`, `SHA256SUMS.txt`, Tauri bundles (NSIS/MSI, deb/AppImage, dmg); `npm run full-build-app` sub-steps verified, one full run still needed
- [x] Docs — `CONTRIBUTING.md`, `CHANGELOG.md` Phase 5+6
- [x] E2E — Playwright (QR flow, send/receive, group create) with mock engine (`e2e/`); CI jobs proposed (e2e + scripts smoke), `.github/workflows/ci.yml` untouched

**Done when:** `npm run full-build-app` builds installers, `cargo test` + `npm test` green on CI (Linux/Windows/macOS), `CHANGELOG.md` updated.

**Tests:**
- Engine: full `cargo test`, `cargo clippy` 0 warnings, `cargo fmt --check`
- Client: `npm test`, `ng build` 0 errors, Playwright E2E (optional)
- Browser verification: Tauri and `ng serve` — QR, chats, messages, media (with the user's consent)

## Backlog / Nice to have

- [ ] Multi-device — multiple `Device`s in `Container`, switcher in the UI (like Bebok projects)
- [ ] `chrome-extension/` — companion like in Bebok (optional)
- [ ] `capacitor` — Android/iOS (like `client/android/` in Bebok)
- [ ] `skills/` — `SKILL.md` per feature (optional)
- [ ] Proxy — `SetProxy`/`SetSOCKSProxy` in `reqwest`/`tungstenite`
- [ ] Port `tctoken.go`/`cstoken.go` when needed — `msgsecret.go` (bot/`msmsg` decryption) is tracked in the Phase 5 **Deferred** list, not here
- [ ] Vector search: cosine-distance cutoff so unrelated queries return nothing (tune once MiniLM runs)

## Rules

- Every PR: `cargo test` + `npm test` green, `cargo fmt` + `clippy`, `ng build` with no errors.
- IPC change: regenerate `specta` + update `whatsapp.service.ts`.
- Store change: migration + `PutDevice`/`GetDevice` test.
- Frontend change: ask about browser verification (dev server + screenshot + console).
