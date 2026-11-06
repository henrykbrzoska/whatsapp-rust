# ARCHITECTURE.md — ngWhatsapp (Angular + Tauri 2 + Rust)

## 1. Goal

Port whatsmeow (Go) to Rust
as the engine for an Angular + Tauri 2 client. Keep WA Web semantics
(WebSocket + Noise + Signal + AppState + store), swap the Go runtime for Tokio
and expose the UI in two modes: desktop (Tauri) and browser (ng serve + engine).
The application language is English.

## 2. High-level diagram

```
┌─────────────────────────────────────────────────────────┐
│  Angular 17+ (standalone, signals, lazy routes)         │
│  - core/services: TauriService, WhatsappService          │
│  - features: auth (QR), chats, messages, groups, media  │
│  - state: signal store / NgRx (chats, messages, conn)   │
│  - assets: public/, styles.css, xterm (optional)        │
└──────────────────────┬──────────────────────────────────┘
                       │ Tauri IPC (invoke/events)  │  Browser: HTTP+SSE
                       │  invoke("connect")         │  fetch + EventSource
                       │  listen("wa-event")        │  GET /events
┌──────────────────────▼──────────────────────────────────┐
│  Tauri 2 shell (client/src-tauri)                       │
│  - tauri.conf.json (productName, bundle, externalBin)   │
│  - capabilities (fs, http, shell, dialog)               │
│  - sidecar: binaries/bebok-server (engine release)      │
└──────────────────────┬──────────────────────────────────┘
                       │
┌──────────────────────▼──────────────────────────────────┐
│  engine (Rust workspace, axum + tokio)                  │
│  ┌───────────────────────────────────────────────────┐  │
│  │ bebok-server (HTTP + SSE + Tauri commands)        │  │
│  │  routes/mod.rs — routing table (like Bebok)       │  │
│  │  GET /version, /config, /events (SSE), /debug/log │  │
│  │  POST /connect, /logout, /send, /media/*          │  │
│  │  Tauri commands (specta) → the same handlers      │  │
│  └──────────────────────┬────────────────────────────┘  │
│  ┌──────────────────────▼────────────────────────────┐  │
│  │ wa-core (protocol core)                           │  │
│  │  client.rs      — struct Client (40+ fields)      │  │
│  │  connection.rs  — Connect/Disconnect/autoReconnect│  │
│  │  handshake.rs   — Noise_XX_25519_AESGCM_SHA256    │  │
│  │  keepalive.rs   — ping 30s, KeepAliveTimeout      │  │
│  │  pair.rs        — QR, PairSuccess, LoggedOut      │  │
│  │  message.rs / send.rs / receipt.rs / retry.rs     │  │
│  │  upload.rs / download.rs / mediaconn.rs           │  │
│  │  group.rs / newsletter.rs / presence.rs           │  │
│  │  appstate/      — LTHash, encode/decode, patches  │  │
│  │  binary/        — node, encoder, decoder, token   │  │
│  │  socket/        — frame_socket, noise_socket      │  │
│  │  store/         — traits + sqlite impl + LID cache│  │
│  │  types/         — JID, MessageInfo, events         │  │
│  │  crypto/        — hkdf, gcm, cbc, keys            │  │
│  └──────────────────────┬────────────────────────────┘  │
│  ┌──────────────────────▼────────────────────────────┐  │
│  │ wa-proto (prost from vendored proto files)  │  │
│  │  build.rs — prost_build::compile_protos           │  │
│  └───────────────────────────────────────────────────┘  │
│  SQLite (sqlx, _foreign_keys=on) + sqlite-vec (vectors) │
│   wa_device, wa_version, sessions,                      │
│   identities, prekeys, sender_keys, appstate,           │
│   contacts, chat_settings, lid_map, msg_secrets,        │
│   privacy_tokens, event_buffer                          │
│  ┌───────────────────────────────────────────────────┐  │
│  │ wa-store (local chat database)                    │  │
│  │  SQLite + FTS5 + sqlite-vec (vec0)                │  │
│  │  messages, messages_fts, message_embeddings        │  │
│  │  fastembed/ort (MiniLM 384d, cosine, offline)     │  │
│  │  history export (JSON/JSONL/CSV, paginated)       │  │
│  └───────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────┘
```

## 3. Data flow

### 3.1 Connection and pairing

```
Angular QR Component
  → invoke("connect")
    → Client::connect() — FrameSocket::connect() → doHandshake(Noise XX)
    → keepAliveLoop + handlerQueueLoop (mpsc 2048)
    → emit WaEvent::QR { codes }  (Tauri event / SSE)
  ← listen("wa-event") → render QR (qrcode lib, rotation 60s/20s)
  → QR scanned → PairSuccess → Connected → emit Connected
  → autoReconnect (backoff AutoReconnectErrors*2s) on disconnect
```

### 3.2 Incoming message

```
NoiseSocket::receiveEncryptedFrame → decrypt (AES-GCM, IV=count)
  → handleFrame → waBinary::Unpack → Unmarshal → Node
  → receiveResponse? → nodeHandlers["message"] → handleEncryptedMessage
    → Signal decrypt (SessionStore, IdentityStore)
    → UnwrapRaw (DeviceSent/Ephemeral/ViewOnce/Edited)
    → dispatchEvent(Message { info, message, isEphemeral... })
      → broadcast + tauri::Emitter → Angular WhatsappService.onMessage()
      → wa-store::insert_message (FTS + embedding)
```

### 3.3 Sending

```
Angular → invoke("send_message", {to, text})
  → Client::sendMessage → encrypt (Signal) → sendNode → SendFrame (Noise)
  → responseWaiters (oneshot) waits for <iq> / <message> ack
  → emit Receipt { type: Delivered/Read }
```

## 4. Key decisions

| Decision | Choice | Rationale |
|---|---|---|
| Runtime | `tokio` (full) | Goroutines → tokio tasks, `context.Context` → `CancellationToken` |
| WebSocket | `tokio-tungstenite` | Closest to `coder/websocket`, supports custom headers/proxy |
| Noise | `snow` + `aes-gcm` | `Noise_XX_25519_AESGCM_SHA256`, `generateIV(count)` 12B nonce |
| Signal | independent, spec-based implementation (no `libsignal-protocol`) | Written from the published Signal spec (X3DH + Double Ratchet, sender keys); the `libsignal-protocol` crate is GPLv3, so it is not linked — `go.mau.fi/libsignal` is only a behaviour/vector reference. Biggest risk, vector tests |
| Protobuf | `prost` + `prost-build` | From vendored `proto/**/*.proto`, `extra.go` → `extra.rs` helpers; `protoc-bin-vendored` (no system `protoc`) |
| DB (protocol) | `sqlx` (SQLite) | `sqlx::migrate!`, `?_foreign_keys=on` like `sqlstore/container.go` |
| DB (local chats) | `rusqlite` (bundled) + `sqlite-vec` | FTS5 + vec0 in one file, sync API wrapped in `spawn_blocking` |
| Log | `tracing` + `tracing-subscriber` | `waLog.Logger.Sub("Recv")` → `tracing::span!` |
| IPC types | `tauri-specta` | Generates TS from Rust, single source of truth |
| FE state | signals + signal store | Lightweight, no NgRx boilerplate (NgRx optional for big lists) |
| Rust tests | `#[cfg(test)]` + `engine/tests/` | Like Bebok — unit in the module, integration in `tests/` |
| Angular tests | Karma + Jasmine | Like Bebok — `*.spec.ts` next to components |
| Language | English | UI strings, code, comments, tests, docs; no i18n unless requested |

## 5. Workspace layout

```
engine/Cargo.toml
  [workspace]
  members = ["crates/wa-proto", "crates/wa-core", "crates/wa-store", "crates/bebok-server"]
  resolver = "3"
  edition = "2024"
  rust-version = "1.88"

engine/crates/wa-proto/
  Cargo.toml, build.rs, src/lib.rs (pub use prost-generated)

engine/crates/wa-core/
  Cargo.toml
  src/
    lib.rs, client.rs, connection.rs, handshake.rs, keepalive.rs
    pair.rs, message.rs, send.rs, receipt.rs, retry.rs, mediaretry.rs
    upload.rs, download.rs, mediaconn.rs, group.rs, newsletter.rs
    presence.rs, privacysettings.rs,
    appstate/{mod,hash,decode,encode,lthash,keys,recovery}  (keys = AppStateSyncKey request/share, recovery = patch/snapshot recovery)
    binary/{mod,node,attrs,encoder,decoder,token,errors,unpack}  (done — Phase 1)
    socket/{mod,frame_socket,noise_socket,noise_handshake}
    store/{mod,traits,device,sqlite/{mod,container,store,lid_map,tests}}  (done — Phase 1)
    types/{mod,jid,message,events,group,presence}
    crypto/{mod,keys,hkdf,gcm,cbc}
  migrations/  (sqlx)
  tests/

engine/crates/wa-store/
  Cargo.toml
  src/
    lib.rs (Store, schema, search), embed.rs (Embedder, HashEmbedder, fastembed), export.rs

engine/crates/bebok-server/
  Cargo.toml
  src/
    main.rs, lib.rs, config.rs, auth.rs, state.rs, routes/{mod,chats,sse}.rs
  tests/http.rs

client/
  angular.json, tsconfig.json, package.json
  src/
    main.ts, styles.css
    app/
      app.ts, app.routes.ts, app.config.ts
      core/services/{tauri.service.ts, whatsapp.service.ts}
      core/interceptors/
      features/auth/qr.component.ts
      features/chats/{chat-list.component.ts, chat.component.ts}
      features/messages/{message-view.component.ts, composer.component.ts}
      features/groups/
      shared/
  src-tauri/
    Cargo.toml, tauri.conf.json, capabilities/, icons/, build.rs
  public/
```

## 6. Lightweight local database — SQLite + FTS5 + sqlite-vec (from day 0)

Requirement: a lightweight database from the start, fast local search (keyword + vector), and a "Download full chat history" button.

* **Engine:** SQLite (same data dir as `whatsmeow`), extended with **FTS5** (keyword) and **sqlite-vec** (`vec0` virtual table, cosine) — no separate vector server. Alternatives: `sqlite-vss`/`usearch`, but `sqlite-vec` is the lightest and ships prebuilt for `rusqlite`.
* **Crate:** `engine/crates/wa-store/` (next to `wa-core`) — `rusqlite` + `sqlite-vec` + `fastembed` (ONNX `all-MiniLM-L6-v2`, 384d, ~80 MB model, cached in `.bebok/models/`), behind the `fastembed` cargo feature. Embeddings are computed in Rust (tokio `spawn_blocking`), offline. Fallback: FTS5 only when no embedder is available (`hybrid` == `keyword`).
* **Schema:**
  ```sql
  CREATE TABLE messages(
    id TEXT PRIMARY KEY, chat_jid TEXT NOT NULL, sender TEXT NOT NULL,
    timestamp INTEGER NOT NULL, type TEXT NOT NULL, text TEXT, raw_json TEXT
  );
  CREATE VIRTUAL TABLE messages_fts USING fts5(text, chat_jid, tokenize='trigram');
  CREATE VIRTUAL TABLE message_embeddings USING vec0(embedding float[384] distance_metric=cosine);
  -- triggers: INSERT/UPDATE/DELETE messages → messages_fts + message_embeddings
  CREATE INDEX idx_messages_chat_ts ON messages(chat_jid, timestamp);
  ```
  Note: there is deliberately **no** `FOREIGN KEY(chat_jid) REFERENCES wa_device(jid)` — a chat JID is not a device JID, so such a key would reject every regular message.
* **Write path:** every `WaEvent::Message`/`HistorySync` → `wa-store::insert_message` (transaction + FTS + embedding). History backfill: `fetch_history` paginates and inserts in batches.
* **Search:** `wa-store::search(query, chat?, limit, mode)` → hybrid: `FTS5 MATCH` (BM25; queries < 3 chars fall back to `LIKE`, user input is quoted against FTS syntax injection) + `vec0 KNN` (cosine, top-k) → RRF fusion. Endpoint `GET /search?q=&chat=&limit=&mode=hybrid|keyword|vector`. KNN always returns the nearest rows, so a distance cutoff is still TODO.
* **History export:** `GET /chats/:jid/history/export?format=json|jsonl|csv&from=&to=` — streamed body, paginated by `timestamp`/`id`, `Content-Disposition: attachment`. CSV cells starting with `= + - @` are prefixed with `'` (formula injection). Frontend: button in `chat.component` → `GET` → `blob` → download. Large chats: NDJSON streaming + `history_export_progress` event.
* **Privacy:** the database is local and never sent anywhere; embeddings are local; `DELETE /chats/:jid/messages` clears FTS + vec.

## 7. Store — traits and SQLite (keys/protocol, separate from wa-store)

Port of `store/store.go` — each `*Store` is an `#[async_trait]`:

```rust
#[async_trait]
pub trait IdentityStore { async fn put_identity(...) -> Result<()>; ... }
#[async_trait]
pub trait SessionStore { async fn get_session(...) -> Result<Option<Vec<u8>>>; ... }
#[async_trait]
pub trait PreKeyStore { async fn get_or_gen_prekeys(...) -> Result<Vec<PreKey>>; ... }
// + SenderKeyStore, AppStateSyncKeyStore, AppStateStore, ContactStore,
//   ChatSettingsStore, MsgSecretStore, PrivacyTokenStore, NCTSaltStore,
//   EventBuffer, LIDStore

pub trait DeviceStore: IdentityStore + SessionStore + PreKeyStore + ... {}
pub struct Device { pub id: Option<JID>, pub lid: JID, pub noise_key: KeyPair, ... }
pub struct Container { db: SqlitePool, lid_map: CachedLIDMap }
impl Container {
  pub async fn get_all_devices(&self) -> Result<Vec<Device>> { ... }
  pub async fn get_device(&self, jid: &JID) -> Result<Option<Device>> { ... }
  pub async fn put_device(&self, device: &Device) -> Result<()> { ... }
  pub fn new_device(&self) -> Device { ... } // NoiseKey, IdentityKey, AdvSecretKey
}
```

* `LIDStore` is global (`CachedLIDMap`), not per-device — see `store/sqlstore/container.go:initializeDevice`.
* Migrations from `store/sqlstore/upgrades/` → `sqlx` migrations, `wa_version` table.
* `Device::save()` / `delete()` like in Go, `Deleted` flag.

## 8. Socket and Noise

* `FrameSocket` — WebSocket + `Frames chan []byte`, `OnDisconnect`, `SendFrame`.
* `NoiseSocket` — `writeKey/readKey: AesGcm`, `writeCounter/readCounter: u32`, `generateIV(count)` → 12B BE.
* `doHandshake(fs, keyPair)` → `NoiseSocket` (XX pattern, prologue, handshake payload).
* `handlerQueueLoop` — `mpsc::channel(2048)`, `tokio::select!` on `connCtx.done()`, 30s timeout per node, warn > 5s.

## 9. Client — fields and synchronization

Port of `client.go:Client` (40+ fields):

```rust
pub struct Client {
  pub store: Device,
  pub log: tracing::Span,
  pub socket: RwLock<Option<NoiseSocket>>,
  pub response_waiters: Mutex<HashMap<String, oneshot::Sender<Node>>>,
  pub event_handlers: RwLock<Vec<Handler>>,
  pub message_retries: Mutex<HashMap<String, u32>>,
  pub group_cache: Mutex<HashMap<JID, GroupMetaCache>>,
  pub recent_messages: RwLock<RecentMessages>, // 512, ring buffer
  pub is_logged_in: AtomicBool,
  pub enable_auto_reconnect: AtomicBool,
  // ...
}
```

* `responseWaiters` — one `oneshot` per `id`, `clearResponseWaiters` on disconnect.
* `eventHandlers` — `RwLock<Vec>`, `dispatchEvent` with `catch_unwind` (like `recover()` in Go).
* `recentMessagesMap` — LRU 512, needed for retry receipts.

## 10. Frontend — Angular

* Standalone components, `input()`/`output()`, `computed()`, `effect()`.
* Routing: `app.routes.ts` — `''` (chats), `'chat/:jid'`, `'settings'`, `'about'`, `'browser-view'` (bare).
* `WhatsappService` — `invoke` + `listen`, signals `connectionState`, `chats`, `messages`.
* `TauriService` — abstraction over `@tauri-apps/api` (mocked in tests / browser mode).
* In browser mode: `fetch` + `EventSource` instead of Tauri IPC, `?engine=` handshake.
* All UI text is English.

## 11. Security

* Per-launch capability token, `Authorization: Bearer` or `?token=`, `BEBOK_NO_AUTH=1` for local use only.
* Path guards — `read_file`/`write_file` confined to the project root.
* CORS — only `http://localhost:4200` + `tauri://localhost` + `BEBOK_CORS`.
* Secrets redacted in `GET /config` and `GET /debug/log`.

## 12. Code intelligence

* `code_index` (tantivy) — `code_index_status` / `code_index_search`
* `code_ast` — `ast_search.enabled`
* `code_graph` — `code_graph.enabled`, `GET /code-graph`
* `code_map` — `code_map.enabled`, `GET /code-map`

## 13. Porting strategy

Recommended **hybrid**: start with a Go sidecar (fast MVP — QR/media work), then iteratively rewrite modules in Rust (store → binary → socket → client → crypto). A full rewrite from scratch is 3–6 months without a working app.

## 14. Challenges

* LID/PN mapping — global cache; a bug here = missing messages.
* AppState LTHash — patches must match Go exactly.
* RecentMessages 512 — without it retry receipts → `UndecryptableMessage`.
* Signal — the `libsignal-protocol` crate is GPLv3, so the protocol is implemented independently from the spec; its API is not 1:1 with `go.mau.fi/libsignal`, which is used only as a behaviour and test-vector reference.
* Protobuf `oneof` — `prost` generates different names than Go.
