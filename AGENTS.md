# ngWhatsapp — Angular + Tauri 2 + Rust

An orientation guide for everyone (humans and agents) working in this repo:
what isn't obvious from a first look at the code.

## 1. What this project is

A local WhatsApp client: **Angular (standalone, signals) + Tauri 2 + Rust engine**.
All WhatsApp logic (WebSocket, Noise, Signal, AppState, store) lives in Rust —
the frontend only renders and sends input. It runs in two modes:

* **Desktop (Tauri 2)** — Angular in a WebView + the engine as a sidecar (`src-tauri/binaries/bebok-server` next to the binary).
* **Browser** — `cargo run` (engine on `:8787`) + `ng serve` (client on `:4200`) with the `?engine=` handshake like in Bebok.

**Language: English.** The application UI, code identifiers, comments, log messages,
test fixtures and all docs are in English. Do not add i18n unless asked.

Protocol behaviour is inspired by whatsmeow (Go) — no files from that repository are vendored in this repo.
Rust port: `engine/crates/wa-core/` — see `ARCHITECTURE.md` and `API-WHATSAPP.md`.

Reading order: `README.md` → `ARCHITECTURE.md` → `API-WHATSAPP.md` → this file → `TODO.md` → code:
`engine/crates/bebok-server/src/routes/mod.rs` (routing table),
`engine/crates/wa-core/src/client.rs` (`do_handshake`, Phase 2; struct `Client` is Phase 3),
`engine/crates/wa-core/src/store/mod.rs` (Device traits),
`engine/crates/wa-store/src/lib.rs` (local message DB + search),
`client/src/app/app.routes.ts`.

## 2. Repo layout

```
/
│   ├── client.go, handshake.go, binary/, socket/, store/, proto/, types/
├── engine/                     # Rust workspace
│   ├── Cargo.toml              # [workspace] members = wa-proto, wa-core, wa-store, bebok-server
│   ├── crates/
│   │   ├── wa-proto/           # prost from vendored proto files (build.rs, vendored protoc)
│   │   ├── wa-core/            # protocol core: client, socket, binary, store, crypto
│   │   ├── wa-store/           # lightweight local DB: SQLite + FTS5 + sqlite-vec + fastembed (from day 0)
│   │   └── bebok-server/       # HTTP + SSE + Tauri IPC (axum)
│   └── tests/                  # integration (optional)
├── client/                     # Angular 17+ (standalone)
│   ├── src/app/
│   │   ├── core/services/whatsapp.service.ts  # invoke + listen
│   │   ├── core/services/tauri.service.ts
│   │   ├── features/auth/qr.component.ts
│   │   ├── features/chats/
│   │   └── features/messages/
│   ├── src-tauri/              # Tauri 2 (tauri.conf.json, icons, capabilities)
│   ├── angular.json, tsconfig.json, karma.conf.js
│   └── package.json            # ng test, tauri:dev, sidecar:copy
├── scripts/
│   ├── bebok.mjs               # doctor, full-build-dev, full-build-app (copy from bebok-1.9.2)
│   └── copy-sidecar.mjs
├── docs/                       # extra docs (optional)
├── AGENTS.md, README.md, ARCHITECTURE.md, API-WHATSAPP.md, CHANGELOG.md, TODO.md
└── .bebok/                     # local data (gitignored)
```

## 3. Working rules

* **The engine is the source of truth.** The frontend holds no keys and doesn't parse Noise/Signal.
* **One process = one Device.** Multi-account = multiple `Device`s in a `Container` (`store/sqlstore/container.go` → `wa-core/store/sqlite`).
* **Async everywhere:** Tokio + `mpsc::channel(2048)` (handlerQueue), `broadcast` for events, `oneshot` for `responseWaiters`.
* **Event-driven:** Go `AddEventHandler(func(any))` → Rust `tokio::broadcast` + `tauri::Emitter` + `specta` (TS types generated from Rust).
* **English only, no i18n** unless asked.
* **Prefer native tools** (`read_file`, `write_file`, `code_index_search`, `code_ast`) over `bash` where possible.
* **Search the index first** (`code_index_search`), then `code_ast`, then `grep`.

## 4. Running

```bash
npm run doctor                    # rustc/cargo, node/npm, tauri cli, system deps
npm run full-build-dev -- --open  # engine :8787 + ng serve :4200, browser already connected
npm run full-build-app            # engine release + sidecar + Tauri bundles + SHA256
```

Details in `README.md` (flags `--port`, `--client-port`, `--no-auth`, `--tauri`).
Manually:

```bash
cd engine && cargo run                          # http://127.0.0.1:8787, prints BEBOK_READY
cd client && npm install && npm start           # http://localhost:4200, paste the BEBOK_READY URL
```

Engine flags: `--host`, `--port` (`0` = random, used by the Tauri sidecar), `--addr`, `--diagnostic`.
Logs go to stderr; stdout is reserved for `BEBOK_READY http://127.0.0.1:<port>/?token=...`.

## 5. Tests — like Bebok

### Engine (Rust)

* Unit: `#[cfg(test)]` in every module (`cargo test`).
* Integration: `engine/tests/`, `crates/*/tests/` (e.g. `bebok-server/tests/http.rs`).
* Required coverage:
  - `binary/` — encoder/decoder vs Go dump (`hex.EncodeToString` from `handleFrame`)
  - `socket/noise` — Noise_XX handshake, `generateIV`, encrypt/decrypt roundtrip
  - `store/sqlite` — `PutDevice`/`GetDevice`/`DeleteDevice`, `wa_version` migrations
  - `client` — `Connect`/`Disconnect`/`autoReconnect`, `responseWaiters`, `handlerQueueLoop`
  - `crypto` — Signal sessions, prekeys, sender keys (vs libsignal vectors)
  - `wa-store` — keyword/vector/hybrid search, chat filter, FTS injection, replace/delete triggers, export formats

```bash
cd engine && cargo test                          # all crates
cd engine && cargo test -p wa-core -- --nocapture
cd engine && cargo test -- --test-threads=1      # when SQLite contention matters
```

CI: `cargo test` on Linux/Windows/macOS (like Bebok). `profile.test` with `debug = "line-tables-only"`.

Note: vector (KNN) search always returns the nearest remaining rows — never assert that a vector/hybrid search is empty; assert on what must *not* come back.

### Client (Angular)

* Unit: Karma + Jasmine (`ng test`), `*.spec.ts` files next to components.
* Bebok pattern: `client/src/app/app.spec.ts` (bare-route), `whatsapp.service.spec.ts` (mock `invoke`/`listen`).
* E2E (optional): Playwright / Cypress for the QR flow and sending messages (mock engine).

```bash
cd client && npm test                # karma (ChromeHeadless)
cd client && npm run test -- --watch # watch mode
```

### E2E / Browser verification

* After a frontend change: ask the user whether to verify in the browser (start the dev server, open the route, screenshot, console).
* Dev servers: `bash` with `background:true` + `ready_port`/`ready_text` + `ready_timeout 120000`.
* Check the proxy (`proxy.conf.json`, `vite.config server.proxy`, `environment.ts`) and start every target (engine + client).

## 6. Lightweight local database (wa-store, from day 0)

* Crate `engine/crates/wa-store/` — SQLite (`rusqlite`, bundled) + FTS5 + `sqlite-vec` (`vec0`, cosine) + `fastembed` (MiniLM 384d, offline, behind the `fastembed` cargo feature). No separate vector server.
* Tables: `messages`, `messages_fts` (FTS5 trigram), `message_embeddings` (vec0) — see `ARCHITECTURE.md` §6. `messages.chat_jid` has **no** FK to `wa_device`.
* Search: `GET /search?q=&chat=&limit=&mode=hybrid|keyword|vector` (FTS BM25 + KNN → RRF), `search_messages` (Tauri).
* Export: `GET /chats/:jid/history/export?format=json|jsonl|csv` (stream, `Content-Disposition`), "Download full chat history" button in `chat.component` — see `API-WHATSAPP.md` §3.8.
* Test data: `POST /debug/seed` (only with `--diagnostic`).

## 7. Tauri IPC — contract

* Commands: `tauri::command` + `specta::specta` → generated TS types (`npm run specta`).
* Events: `app.emit("wa-event", WaEvent)` → `listen("wa-event")` in Angular.
* In browser mode: the same events via SSE (`GET /events`).

See `API-WHATSAPP.md` — full list: `connect`, `get_qr`, `pair_code`, `send_message`, `send_media`, `fetch_history`, `logout`, `is_connected`, `search_messages`, `export_chat_history`.

## 8. Store and migrations

* SQLite with `?_foreign_keys=on` (required, see `sqlstore/container.go:Upgrade()`).
* Tables: `wa_device`, `wa_version`, `wa_*` (sessions, identities, prekeys, appstate, contacts, chat_settings, lid_map).
* Migrations: `engine/crates/wa-core/migrations/` via `sqlx::migrate!`.
* `LIDStore` is global (`CachedLIDMap`), not per-device — a bug here = missing messages.

## 9. Security

* Per-launch capability token (`BEBOK_READY ...?token=...`), `Authorization: Bearer` or `?token=`.
* Path guards — file tools confined to the project root.
* Secrets redacted in `GET /config` and diagnostic logs.
* `browser_*` tools default to `Ask` even in read-only mode.

## 10. Code intelligence (like Bebok)

* `code_index` (tantivy) — `code_index_status` / `code_index_search`
* `code_ast` — `ast_search.enabled` in `.bebok/config.json`
* `code_graph` — `code_graph.enabled`, `GET /code-graph`
* `code_map` — `code_map.enabled`, `GET /code-map`

## 11. What doesn't exist

No `SPEC.md` — the spec is the code + `ARCHITECTURE.md`.
No central WA spec — WA Web is a reverse-engineered protocol; the sources are whatsmeow and `Baileys`.

## 12. PR checklist

* [ ] `cargo test` green (engine)
* [ ] `npm test` green (client)
* [ ] `cargo fmt` + `cargo clippy` with no warnings
* [ ] `ng build` with no errors
* [ ] IPC change: `specta` regenerated and `whatsapp.service.ts` updated
* [ ] Store change: migration + `PutDevice`/`GetDevice` test
* [ ] Frontend change: screenshot / browser verification (with consent)
* [ ] All new user-facing strings, comments and docs in English
