# Phase 5 — gap analysis (Groups, AppState, Presence)

Date: 2026-11-05. Source: `docs/PROMPT-PHASE-5.md` vs working tree.
Baseline: Phases 0–4 merged (wa-core 261 tests + bebok-server/http/specta green; leftover `TODO(Phase 3/4)` comments exist in `client.rs`, `connection_events.rs`, `pair.rs`, `retry_receipt.rs`, `types/version.rs` — unrelated to Phase 5 scope).

## Verdict: NOT STARTED — 0 of 6 tasks done

No Phase 5 file exists. All TODO.md Phase 5 boxes are unchecked.

## Task-by-task status

| Task | Spec files | Actual state |
|---|---|---|
| A — AppState core (`appstate/mod.rs`, `lthash.rs`, `hash.rs`, `decode.rs`, `encode.rs`) | 5 new files | ❌ `src/appstate/` does not exist |
| B — AppState sync (`keys.rs`, `recovery.rs`, `client/appstate.rs`) | 3 new files | ❌ none exist |
| C — groups + newsletters (`client/group.rs`, `newsletter.rs`, `notification.rs`, `user.rs`, extend `types/events.rs`, API-WHATSAPP §2) | 4 new + 2 extended | ❌ none exist; `WaEvent` ends at `MediaRetry` (Phase 4), no `GroupInfo`/`JoinedGroup`/`NewsletterLiveUpdate`/`AppStateSyncProgress` |
| D — presence + privacy (`client/presence.rs`, `client/privacysettings.rs` → `Presence`/`ChatPresence`/`PrivacySettings` events) | 2 new files | ❌ none exist; only `types/presence.rs` (Phase 3/4 enum `PresenceState`) and `TODO(Phase 5)` comments at `client.rs:559` (groupCache/appStateProc) and `client.rs:1056` (node dispatch: `presence` is in `is_known_node` but falls through to "didn't handle" debug) |
| E — server + store + Tauri (`routes/groups.rs`, `routes/presence.rs`, `routes/mod.rs`, `wa-store` chat-settings read path, `lib.rs` commands) | 2 new + 3 extended | ❌ `routes/` has only `chats.rs`, `mod.rs`, `sse.rs`; API-WHATSAPP §§3.3–3.5 (groups/presence/newsletters) are spec-only, no handlers |
| F — Angular (`features/chats/` chat-list/group-info, `features/groups/` new, specs) | extend + new | ❌ `features/` has only `about`, `browser-view`, `chats`, `search`, `settings`; no `groups/`, no `chat-list.spec.ts`/`group-info.spec.ts` |

## Boundary contracts (§2) — all missing

- **§2.1 AppState API** (`fetch_app_state`, `apply_app_state_patch`, LTHash golden vectors, patch-MAC verify): does not exist.
- **§2.2 Group-cache API** (`GroupInfo`, `create_group`, `get_group_info`, `groupCache` with TTL, `CanSend`): does not exist. Only trace: `client.rs:559` comment `TODO(Phase 4+): groupCache, userDevicesCache, appStateProc` and `ClientError::NoPermissionInGroup`.
- **§2.3 New `WaEvent` variants** (`GroupInfo`, `JoinedGroup`, `Presence`, `ChatPresence`, `PrivacySettings`, `NewsletterLiveUpdate`, `AppStateSyncProgress`): none added; `specta.ts` matches current (Phase 4) enum (grep for the 6 new variant names → 0 hits).

## Deferred markers (§5) — none present

`grep -rn "TODO(Phase6)" engine/crates/wa-core/src` → empty (spec says closing gate needs exactly 8: history sync, app-state key shares, LID-migration, bot/msmsg, placeholder resends, `NewsletterMessageMeta`, `UseRetryMessageStore`, `retryFrame`).
Note: TODO.md Phase 5 **Deferred** list still says `TODO(Phase5)` markers — spec §5 renames them to `TODO(Phase6)`. Also `grep TODO(Phase5)` in `wa-core/src` → empty, so no deferred markers of either form exist yet.

## Prerequisites for starting (from spec)

- Status gate (§2 of prompt): Phases 3+4 merged — ✅ satisfied (261 wa-core tests, build/clippy/fmt/specta green).
- Store traits (`AppStateStore`, `AppStateSyncKeyStore`, `ContactStore`, `ChatSettingsStore`, `MsgSecretStore`) already in `store/traits.rs` **with `SqlStore` impls** (`store/sqlite/store.rs:478,526,625,781,866`) — ✅ reuse, no rewrite needed. (`LidStore` trait exists; impl lookup inconclusive — verify before Task B.)
- `wa_appstate*` schema present (`wa_app_state_sync_keys`, `wa_app_state_version`, `wa_app_state_mutation_macs` in `migrations/0001_latest_schema.sql`) — ✅ no migration expected.
- Wave plan: 5 waves over `code1`+`code2` (A+F1 → B+D → C+E1 → merge+E2 → gates+F2); `client.rs`/`lib.rs`/`Cargo.toml`/`types/events.rs` merges are main-thread only.

## Suggested start (Wave 1 per spec)

- `code1`: Task A — read `docs/PROMPT-PHASE-5.md` §2.4 table A + `whatsmeow/appstate/{lthash,hash,decode,encode}`, create `src/appstate/` (mod, lthash, hash, decode, encode) with golden-vector + roundtrip tests; stop after `cargo test -p wa-core appstate::` green.
- `code2`: Task F part 1 — chat-list/group-info scaffolding against local interfaces + mocked service, no engine files.
