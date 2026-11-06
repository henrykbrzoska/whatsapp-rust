# Phase 6 — gap analysis

Date: 2026-11-05. Source: `TODO.md` Phase 6 vs working tree.
Baseline: Phases 0–5 merged (wa-core 383, wa-store 23, bebok-server 33, client 68 specs — green; fmt + clippy clean; `ng build` clean).

## Verdict: NOT STARTED — 0 of 7 done

All Phase 6 boxes unchecked. Deferred markers: exactly 8 (`client/notification.rs` ×7 + `client/appstate.rs` ×1).

| # | Task | State |
|---|---|---|
| 1 | Deferred protocol ports (history sync, key shares, LID migration, bot/msmsg, placeholder resends, `NewsletterMessageMeta`, `UseRetryMessageStore`, `retryFrame`) | ❌ none ported; Go sources present (`msgsecret.go`, `retry.go` 544, `mediaretry.go`, `HistorySyncNotification`); `wa_appstate*` + `AppStateSyncKeyStore`/`MsgSecretStore` already exist — reuse, no migrations |
| 2 | Server wiring (`GET /chats`→`list_chats_with_meta`, `Client` in `AppState` kills 503 in `groups.rs::session()`, `POST /logout`, `GET /debug/log`, `/blocklist`, `/block`, `/unblock`, `/contact(s)`, `/picture`) | ❌ only `chats/groups/presence/mod/sse` routes; `/logout` 501; `/chats` plain `ChatSummary[]` |
| 3 | Settings/About/Stats UI (providers + redacted `/config`, `/debug/log`, `/version`+`protocolRef`, stats optional) | 🟡 stubs render status/version only |
| 4 | Code intelligence (`code_index`, `code_ast`, `code_graph`, `code_map` like Bebok) | ❌ nothing |
| 5 | Release (`scripts/release.mjs`, `SHA256SUMS.txt`, Tauri bundles, `full-build-app` → installers) | ❌ only `bebok.mjs`+`copy-sidecar.mjs`; `full-build-app` exists — verify bundles |
| 6 | Docs (`CONTRIBUTING.md`, `CHANGELOG.md` Unreleased predates Phase 5) | ❌ |
| 7 | E2E (Playwright QR/send/group-create vs mock engine; CI Linux/Win/macOS) | ❌ no `e2e/`; `.github/workflows/ci.yml` exists — check fmt/clippy/specta/build/test coverage |

## Done criteria

- **T1:** each item = `wa-core` port + tests, marker removed. Gate: `grep TODO(Phase6)` → empty. No wa-store backfill for history sync.
- **T2:** `GET /chats` → `ChatListEntry[]` (mute/pin/archive live); no 503 when connected; `/logout`, `/blocklist`, `/block`, `/unblock`, `/contact(s)`, `/picture`, `/debug/log` per API-WHATSAPP §§3.4/3.8; `/config` redacted.
- **T3:** settings (providers + redacted config + log tail, `BEBOK_DIAGNOSTIC=1`); about (client/engine + `protocolRef` sha); stats optional.
- **T4:** index/ast/graph/map answer like Bebok (status + search).
- **T5:** `npm run full-build-app` → signed installers + `SHA256SUMS.txt` from clean checkout.
- **T6:** `CONTRIBUTING.md` (build/test/PR per AGENTS.md §12); `CHANGELOG.md` Phase 5+6.
- **T7:** `npx playwright test` (QR/send/group-create vs mock); CI matrix green.

## Prerequisites — ✅ all met

Phase 5 APIs/routes/commands/service/specta merged (T2/T3 build, no rework) · store traits+schema exist · CI skeleton + `bebok.mjs` exist (extend, don't invent) · fleet `code1`+`code2`; main thread owns `routes/mod.rs`, `types/events.rs`, `app.routes.ts`, `Cargo.toml`.

## Fleet plan — 4 waves, max 2 parallel (code2 preferred for code)

Different files only; when in doubt, one coder. Subagents never touch routing table / events enum / `app.routes.ts` / workspace `Cargo.toml`; main thread syncs `API-WHATSAPP.md` §§3.4/3.8 + `TODO.md` boxes after each wave.

| Wave | Slot 1 (code2) | Slot 2 | Gate |
|---|---|---|---|
| W1 | **T1a** retry/history: `retryFrame` (`client/retry.rs`), receipts consult store (512-entry fallback kept), `HistorySyncNotification`→event, no backfill | **T6** docs: `CONTRIBUTING.md` + `CHANGELOG.md` Phase 5 (zero code overlap) | `cargo test -p wa-core` green; markers 8→5 |
| W2 | **T1b** crypto/LID: `msgsecret.rs` + bot decrypt, placeholder resends, LID apply to `CachedLIDMap`, `NewsletterMessageMeta` parse | **T2** server: `Client` in `AppState`, kill 503, `/logout`, `/chats`→meta, blocklist/contact/picture/debug-log, redacted `/config` | `--workspace` green; markers 5→1; `/chats` has mute/pin/archive |
| W3 | **T1c** key shares: persist + patch apply (markers → 0) | **T3** UI: settings/about/`protocolRef`/stats; specs green | `grep TODO(Phase6)` empty; `npm test` + `ng build` green |
| W4 | **T4** code intel (port Bebok plugin/flags/routes) | **T5+T7** `release.mjs` + checksums + bundle verify; Playwright 3 flows; CI check | Full gates: `cargo test`, `fmt --check`, `clippy -D warnings`, `npm test`, `ng build`, `playwright test`, CHANGELOG |

Start (W1): code2 reads `whatsmeow/retry.go` + `client/retry.rs` + `retry_receipt.rs` → `retryFrame` + store-backed receipts + history-sync event, stops at green `cargo test -p wa-core`; code1 reads `TODO.md` Phase 5 + `git log` → CONTRIBUTING + CHANGELOG, no code.
