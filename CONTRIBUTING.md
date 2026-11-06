# Contributing to ngWhatsapp

Thanks for helping. This file is the short version of how to build, test and open a PR.
Background reading: `README.md` → `ARCHITECTURE.md` → `API-WHATSAPP.md` → `AGENTS.md` → `TODO.md`.

## Ground rules

* **English only** — UI strings, code identifiers, comments, log messages, test fixtures and docs. No i18n unless asked.
* **The engine is the source of truth.** All WhatsApp logic (socket, Noise, Signal, AppState, store) lives in Rust
  (`engine/`); the Angular client only renders and sends input. The frontend never holds keys.
* Port protocol behaviour from the Go reference (whatsmeow, MPL-2.0 — see `THIRD_PARTY_NOTICES.md`); keep the Go
  file/function name in a doc comment so the port stays traceable.
* Every change must work in both run modes: **desktop (Tauri)** and **browser** (`ng serve` + engine on localhost).

## Setup

Requirements: Rust >= 1.88 (edition 2024), Node.js 22 (see `.nvmrc`), npm, and the
[Tauri 2 system deps](https://tauri.app/start/prerequisites/). `protoc` and SQLite are vendored/bundled.

```bash
npm run doctor                    # checks rustc/cargo, node/npm, tauri cli, OS deps
cd client && npm install          # client dependencies
npm run full-build-dev -- --open  # engine :8787 + ng serve :4200, browser already connected
```

Manual runs and flags are in `README.md` (Quick start) and `AGENTS.md` §4.

## Build and test

Engine (Rust, from `engine/`):

```bash
cargo test --workspace                                  # unit + integration (all crates)
cargo test -p wa-core -- --nocapture                    # one crate, with output
cargo test -- --test-threads=1                          # when SQLite contention matters
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings   # zero warnings
```

Client (Angular, from `client/`):

```bash
npm test                          # Karma + Jasmine (ChromeHeadless)
npx ng build                      # must build with no errors
```

From the repo root, `npm test` runs both layers. CI (`.github/workflows/ci.yml`) runs the same commands on
Linux, Windows and macOS, plus the specta drift check.

Test conventions (details in `AGENTS.md` §5):

* Rust unit tests live in `#[cfg(test)]` modules; integration tests in `crates/*/tests/` (e.g. `bebok-server/tests/http.rs`).
* Protocol code is pinned against golden vectors produced by running the Go reference.
* Tests are offline — no real WhatsApp connection in CI.
* Client specs (`*.spec.ts`) sit next to the component; mock `invoke`/`listen`/`EventSource` via `client/src/app/testing/fakes.ts`.
* Vector (KNN) search always returns the nearest remaining rows — assert on what must *not* come back, never on an empty result.

## Change-specific requirements

* **IPC / event change** (Tauri command, HTTP route, `WaEvent` variant, a type deriving `specta::Type`):
  regenerate the TS bindings with `npm run specta` (writes `client/src/app/types/specta.ts`), update
  `client/src/app/core/services/whatsapp.service.ts`, and document the contract in `API-WHATSAPP.md`.
  `bebok-server/tests/specta_bindings.rs` fails when the committed bindings drift.
* **Store change** (`wa-core` schema or store traits): add a migration under `engine/crates/wa-core/migrations/`
  (never edit an applied one) and cover it with a `PutDevice`/`GetDevice` (`put_device`/`get_device`) roundtrip test.
  SQLite runs with `_foreign_keys=on`; `CachedLIDMap` is global, not per-device.
* **Frontend change**: verify it in the browser (dev server, open the route, screenshot, check the console) — and in
  Tauri when the change touches native commands. Attach the screenshot to the PR.
* **Deferred work**: mark a known gap with `TODO(PhaseN)` and list it in `TODO.md`, rather than leaving it silent.

## Pull requests

Keep PRs focused (one phase task or fix). Update `CHANGELOG.md` (`Unreleased`, Keep a Changelog sections) and tick
the matching box in `TODO.md`. Before requesting review, check the list from `AGENTS.md` §12:

* [ ] `cargo test` green (engine)
* [ ] `npm test` green (client)
* [ ] `cargo fmt` + `cargo clippy` with no warnings
* [ ] `ng build` with no errors
* [ ] IPC change: `specta` regenerated and `whatsapp.service.ts` updated
* [ ] Store change: migration + `PutDevice`/`GetDevice` test
* [ ] Frontend change: screenshot / browser verification
* [ ] All new user-facing strings, comments and docs in English

## Security

Never commit tokens, session databases or `.bebok/` data. Secrets must stay redacted in `GET /config` and in logs.
Report vulnerabilities privately to the maintainer (Henryk Brzoska) instead of opening a public issue.
