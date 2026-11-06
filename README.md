# ngWhatsapp — Angular + Tauri 2 + Rust

Author: **Henryk Brzoska**

A local WhatsApp client — **Angular (standalone, signals) + Tauri 2 + Rust engine**.
The protocol core lives in `engine/crates/wa-core/`. Protocol reference: whatsmeow (Go, MPL-2.0) — see `THIRD_PARTY_NOTICES.md`.
The frontend runs in two modes: **desktop (Tauri WebView)** and **browser (ng serve + engine on localhost)**.

> Status: Phase 2 complete — transport + Noise handshake + local store; the QR/session state machine is Phase 3. The Rust engine core is in `engine/crates/wa-core/`. See `ARCHITECTURE.md`, `API-WHATSAPP.md`, `TODO.md`.
>
> The application language is **English** — UI strings, code, comments, tests and docs.

## Features (target)

* Pairing via QR / code / passkey, multi-device, auto-reconnect
* 1:1 chats, groups, newsletters, presence (typing, online)
* Sending/receiving messages (text, media, reactions, edits, view-once)
* Media upload/download, retry receipts, history sync
* AppState sync (contacts, chat settings, mute/pin/archive)
* E2E encryption (Signal — sessions, prekeys, sender keys, LID/PN mapping)
* SQLite store (per-device, `sqlx` migrations), global LID cache
* Lightweight local database from day 0: SQLite + FTS5 + sqlite-vec — fast local search (keyword + vector/semantic), no server
* Local vector search (embeddings computed in Rust, `fastembed`/`ort` MiniLM 384d, cosine) — works offline
* "Download full chat history" button — export a chat to JSON/JSONL/CSV, paginated and streamed
* Live events to the UI (Tauri events / SSE in browser mode)

## Requirements

* Rust >= 1.88 (edition 2024), Node.js 22 (>= 20), npm
* Tauri 2 system deps: WebView2 + MSVC (Windows), `webkit2gtk-4.1` / `libsoup-3.0` / `gtk+-3.0` / `librsvg-2.0` (Linux), Xcode CLT (macOS) — https://tauri.app/start/prerequisites/
* SQLite (bundled via `sqlx` / `rusqlite`, no separate install)
* `protoc` is not required — `wa-proto` uses `protoc-bin-vendored`

## Quick start

```bash
npm run doctor                    # checks rustc/cargo, node/npm, tauri cli, OS deps
npm run full-build-dev -- --open  # engine :8787 + ng serve :4200, browser already connected
npm run full-build-app            # engine release + sidecar + Tauri bundles + SHA256
```

Run from the repo root (`dev.sh` / `dev.cmd` are aliases for `full-build-dev`; `dev.sh tauri` runs `tauri dev`).

Flags: `--port`, `--client-port`, `--no-auth` (`BEBOK_NO_AUTH=1`), `--diagnostic` (`BEBOK_DIAGNOSTIC=1`), `--open`, `--tauri`; `full-build-app` accepts `--bundles nsis,msi|deb,appimage|dmg`, `--skip-engine`, `--skip-tauri`. `npm run engine` / `npm run client` start one half.

Manually:

```bash
cd engine && cargo run                          # http://127.0.0.1:8787, prints BEBOK_READY
cd client && npm install && npm start           # http://localhost:4200, paste the BEBOK_READY URL
cd engine && cargo build --release && cd ../client && npm run sidecar:copy && npm run tauri:build
```

Engine flags: `--host IP`, `--port PORT` (`0` = random, used by the Tauri sidecar), `--addr IP:PORT`; CLI flags win over `BEBOK_ADDR`. Logs go to stderr; stdout carries only `BEBOK_READY http://127.0.0.1:<port>/?token=...`.

Optional engine feature: `cargo run --features fastembed` enables real MiniLM embeddings (downloads a ~80 MB model on first use). Without it, `hybrid` search behaves like `keyword`.

## Configuration

Layered JSONC: defaults → global → project (`<project>/.bebok/config.json`, providers merged by name).

| OS | Global config | Engine data (SQLite, sessions) |
|---|---|---|
| Linux | `~/.config/bebok/config.json` | `~/.local/share/bebok/` |
| Windows | `%APPDATA%\bebok\config.json` | `%APPDATA%\bebok\` |
| macOS | `~/Library/Application Support/bebok/config.json` | `~/Library/Application Support/bebok/` |

Engine environment variables:

| Variable | Effect |
|---|---|
| `BEBOK_ADDR` | bind `IP:PORT` (default `127.0.0.1:8787`; CLI flags win) |
| `BEBOK_NO_AUTH=1` | disable the capability token (unauthenticated API — loud log) |
| `BEBOK_TOKEN` | fixed token instead of a random per-launch one (`--token` wins) |
| `BEBOK_PORT` | engine / sidecar port (validated as u16) |
| `BEBOK_CORS` | extra allowed origins, comma-separated |
| `BEBOK_DIAGNOSTIC=1` | enable `GET/DELETE /debug/log` and `POST /debug/seed` |
| `BEBOK_SHELL` | shell for `bash` and the terminal (`cmd /C` on Windows, `sh -c` elsewhere) |
| `BEBOK_WA_URL` | WhatsApp WebSocket endpoint (`--wa-url`); `wss://web.whatsapp.com/ws/chat` by default — point it at a local socket to exercise `POST /connect` offline |
| `BEBOK_HANDSHAKE_TIMEOUT_SECS` | how long the server may take to answer the Noise handshake (`--handshake-timeout`, default 20 s) |

## Layout

```
/
├── engine/                     # Rust workspace (wa-proto, wa-core, wa-store, bebok-server)
├── client/                     # Angular + Tauri 2
│   ├── src/app/                # features, core/services
│   └── src-tauri/              # tauri.conf.json, capabilities
├── scripts/bebok.mjs           # doctor, full-build-dev, full-build-app
└── AGENTS.md, ARCHITECTURE.md, API-WHATSAPP.md, CHANGELOG.md, TODO.md
```

Details in `ARCHITECTURE.md` and `AGENTS.md`.

## Tests

Like Bebok — both layers have tests, CI requires both to be green.

```bash
cd engine && cargo test                          # Rust unit + integration
cd engine && cargo test -p wa-core -- --nocapture
cd client && npm test                            # Karma + Jasmine (ChromeHeadless)
```

More in `AGENTS.md` §5.

## Browser mode

Without Tauri: engine on `:8787` + `ng serve` on `:4200`. On startup the client reads `?engine=<url-encoded BEBOK_READY>` (URL + token), stores it like a manual Connect, clears the address bar and talks to the engine with `Authorization: Bearer`. SSE (`GET /events`) replaces Tauri events.

## Security

* Per-launch capability token, `Authorization: Bearer` or `?token=`
* Path guards — file tools confined to the project root
* CORS for known origins only, bind `127.0.0.1`
* Secrets redacted in `GET /config` and logs

## License

Protocol code ported from whatsmeow (Go, MPL-2.0) — see `THIRD_PARTY_NOTICES.md`. Port and client code — see `LICENSE` (once added).

## See also

* `AGENTS.md` — guide for agents and humans
* `ARCHITECTURE.md` — architecture, diagrams, decisions
* `API-WHATSAPP.md` — Tauri IPC / HTTP contract + events
* `CHANGELOG.md` — change history
* `TODO.md` — phase plan
