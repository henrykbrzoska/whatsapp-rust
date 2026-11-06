# Phase 5 prompt — Groups, AppState, Presence

Copy the block below into a fresh session to start Phase 5.

---

You are implementing **Phase 5 (Groups, AppState, Presence)** of ngWhatsapp — an Angular + Tauri 2
client whose WhatsApp engine is a Rust port of `whatsmeow/` (Go, MPL-2.0). Phases 0–2 are merged;
Phase 3 (`docs/PROMPT-PHASE-3.md` — `struct Client`, connection state machine, pairing, keepalive,
QR screen) and Phase 4 (`docs/PROMPT-PHASE-4.md` — messaging + crypto: Signal layer, send/retry,
media, `WaEvent::Message`) are the direct prerequisites of this work. Read `README.md` →
`ARCHITECTURE.md` → `API-WHATSAPP.md` → `AGENTS.md` → `TODO.md` (Phase 5) first.

> **Status gate: do not start Phase 5 until Phases 3 and 4 are merged.** Phase 5 needs
> `struct Client` + `responseWaiters` + `handlerQueueLoop` (Phase 3) and the message path +
> `WaEvent::Message` + chat-settings writes (Phase 4). Group sends need Phase 4 sender keys;
> app-state decryption needs Phase 4 HKDF/CBC. If those are missing, stop and say so.

The engine is the source of truth: no protocol logic moves into the frontend, no keys leave Rust.
Every ported Rust file keeps the MPL-2.0 header + the three-line boilerplate. English only, no
i18n, no `unwrap`/`expect`/`panic!` outside `#[cfg(test)]`.

Your work is **six tasks in two halves**:

* **Backend half** — Task A (AppState core: LTHash + encode/decode), Task B (AppState sync:
  keys, recovery, mutations), Task C (groups + newsletters).
* **Presence half** — Task D (presence + privacy), Task E (server endpoints + Tauri commands +
  `wa-store` wiring), Task F (Angular chat-list / group UI).

Task ownership (§1): never edit files outside the ownership list of the task you are on. The two
halves meet at the boundary contract in §2 (AppState API, group-cache API, single
`dispatch_event` sink, additive `WaEvent` variants).

Protocol sources: `whatsmeow/group.go`, `newsletter.go`, `presence.go`, `privacysettings.go`,
`appstate.go`, `appstate/{decode,encode,errors,hash,keys,recovery}.go`, `appstate/lthash/lthash.go`,
`notification.go` (group/picture/newsletter notifications), `user.go` (GetUserInfo, profile
pictures), `tctoken.go`/`cstoken.go` (privacy tokens for presence subscribe), `broadcast.go`.
Never modify `whatsmeow/`.

## Baseline (do not regress)

- `cd engine && cargo test --workspace` → green (was **183 tests** before Phase 3; Phases 3–4 grew it — report the real count)
- `cd client/src-tauri && cargo test` → 4 tests
- `cd client && npx ng test --watch=false --browsers=ChromeHeadless` → 51 tests
- `cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings` clean
- `npx ng build` clean; `npm run specta` produces no diff unless wire types changed

## Execution — run with the fleet (`code1` + `code2`)

You are the **main thread**: you plan, brief, supervise, merge and run the gates. The hands-on work
goes to the fleet through `fleet` with `names: ["code1", "code2"]`, using heterogeneous `tasks`
(one different task per member per wave). Never invent other member names, never run more than
these two at once, and never let both members own the same file in the same wave.

| Wave | `code1` | `code2` | Why this pairing |
|------|---------|---------|------------------|
| 1 | Task A — AppState core (LTHash + encode/decode) | Task F, part 1 — chat-list/group-info scaffolding against local interfaces + mocked service | no shared files; F needs no engine code yet |
| 2 | Task B — AppState sync (keys, recovery, mutations) | Task D — presence + privacy (uses B's `AppStateStore` traits, already in `store/traits.rs`) | both need only A + existing traits |
| 3 | Task C — groups + newsletters | Task E, part 1 — `wa-store` chat-settings/contact read path + group routes design | C owns `types/events.rs` this wave; E part 1 must not touch it |
| 4 | — (main thread merges C, runs gates) | Task E, part 2 — group/presence/privacy routes + Tauri commands against the C/D signatures in §2 | E only calls C/D |
| 5 | — (main thread runs every gate) | Task F, part 2 — switch F to the `specta` types, wire `group_info`/`presence`/`chat_presence` events | after `npm run specta` |

Rules for every wave:

* **Self-contained briefs.** A member cannot see this document unless you tell it to read it. Each
  brief names: the task section to read in this file, the exact file-ownership list (§1), the
  boundary contract it may rely on (§2), the tests it must add, and a stop condition
  ("stop after `cargo test -p wa-core appstate::` is green and report the files changed").
* **Shared files are yours.** `src/client.rs` (`mod` lines/fields), `src/lib.rs`, workspace
  `Cargo.toml` and `types/events.rs` conflicts are applied by you between waves, never by two members
  at once. Tell members to list the edits they need there instead of making them, unless the wave
  table gives them that file.
* **Gate between waves.** After `task_wait` returns both results, run the §7 gates yourself. Fix or
  re-task failures before you start the next wave.
* **Supervise.** Loop `task_wait` (60–150 s) → `task_status`. A child flagged looping/wandering →
  `task_cancel` → `task_report` (work log, changed files, last output) → re-task the same part with a
  tighter brief that builds on its partial work. Flagged a second time → finish that part yourself.
  An errored/aborted child reports no text: always `task_report` it before re-tasking.
* **Fallback.** If a member cannot run at all, give its wave slot to the other member on the next
  wave or do it yourself, and say so in the report. Never skip a task because delegation failed.
* **Report by member name**: which member did which task, which were re-tasked or taken over.

---

# Part 1 — backend half (Tasks A–C)

You are implementing the backend half of **Phase 5 (Groups, AppState, Presence)** of ngWhatsapp —
an Angular + Tauri 2 client whose WhatsApp engine is a Rust port of `whatsmeow/` (Go, MPL-2.0).
Phases 0–4 are merged (see the status gate above — verify before writing code). Read `README.md` →
`ARCHITECTURE.md` → `API-WHATSAPP.md` → `AGENTS.md` → `TODO.md` (Phase 5) first.

## 1. File ownership (Tasks A–C own `wa-core` only)

| File | Action | Task |
|---|---|---|
| `engine/crates/wa-core/src/appstate/mod.rs` | **new** — module root, `AppStateError`, re-exports | A |
| `engine/crates/wa-core/src/appstate/lthash.rs` (or `lthash/`) | **new** — port of `whatsmeow/appstate/lthash/lthash.go`, must match Go exactly | A |
| `engine/crates/wa-core/src/appstate/hash.rs` | **new** — `HashState`, verify/patch MAC | A |
| `engine/crates/wa-core/src/appstate/decode.rs` | **new** — `SyncActionData` decode, `PatchList` parse | A |
| `engine/crates/wa-core/src/appstate/encode.rs` | **new** — encode + roundtrip with decode | A |
| `engine/crates/wa-core/src/appstate/keys.rs` | **new** — key requests/shares, `AppStateSyncKeyStore` usage | B |
| `engine/crates/wa-core/src/appstate/recovery.rs` | **new** — patch recovery, `SnapshotRecovery` | B |
| `engine/crates/wa-core/src/client/appstate.rs` | **new** — `FetchAppState`, `handleAppStateNotification`, mutation dispatch into `ChatSettingsStore`/`ContactStore` (port of `whatsmeow/appstate.go`) | B |
| `engine/crates/wa-core/src/client/group.rs` | **new** — port of `whatsmeow/group.go` | C |
| `engine/crates/wa-core/src/client/newsletter.rs` | **new** — port of `whatsmeow/newsletter.go` | C |
| `engine/crates/wa-core/src/client/notification.rs` | **new** — group/picture/newsletter notifications (port of `whatsmeow/notification.go`) | C |
| `engine/crates/wa-core/src/client/user.rs` | **new** — `GetUserInfo`, profile pictures (port of `whatsmeow/user.go`) | C |
| `engine/crates/wa-core/src/types/events.rs` | **extend** — additive variants only (see §2.3) | C |
| `API-WHATSAPP.md` §2 (events) | **update** for new `WaEvent` variants | C |
| `TODO.md` Phase 5 checkboxes | **tick** what lands | A, B, C |

Store traits already exist — reuse, do not rewrite: `AppStateStore`, `AppStateSyncKeyStore`,
`ContactStore`, `ChatSettingsStore`, `MsgSecretStore`, `LidStore` (`store/traits.rs`), implemented
in `store/sqlite/store.rs`. Schema is the single `migrations/0001_latest_schema.sql` via
`sqlx::migrate!` — no schema drift from Go (`wa_appstate*` tables stay as-is).

## 2. Boundary contracts (settle first — Tasks D–F rely on these)

### 2.1 AppState API (Task B provides)

```rust
fn fetch_app_state(name: &str, full_sync: bool) -> Result<Vec<SyncAction>, AppStateError>;
fn apply_app_state_patch(patch: PatchList) -> Result<(), AppStateError>; // verifies MAC, dispatches mutations
fn get_chat_settings(jid: &str) -> Result<Option<ChatSettings>, StoreError>; // existing trait
```

LTHash golden vectors: generate with `go run` against `whatsmeow/appstate` (`ARCHITECTURE.md`:
"must match Go exactly"). Patch-MAC verification rejects bad patches before any store write.

### 2.2 Group-cache API (Task C provides)

```rust
struct GroupInfo { jid: String, name: String, topic: Option<String>, participants: Vec<Participant>, /* … */ }
fn create_group(name: &str, participants: &[String]) -> Result<GroupInfo, ClientError>;
fn get_group_info(jid: &str) -> Result<GroupInfo, ClientError>; // consults group_cache first
fn update_group_participants(jid: &str, action: ParticipantAction, p: &[String]) -> Result<(), ClientError>;
fn get_group_invite_link(jid: &str, reset: bool) -> Result<String, ClientError>;
fn join_group_with_link(code: &str) -> Result<GroupInfo, ClientError>;
fn leave_group(jid: &str) -> Result<(), ClientError>;
fn set_group_name(jid: &str, name: &str) -> Result<(), ClientError>;
fn set_group_topic(jid: &str, topic: &str) -> Result<(), ClientError>;
```

`groupCache` type: in-memory map keyed by JID with TTL, invalidated on `GroupInfo` notifications.
`CanSend` check before group sends (needs Phase 4 sender keys).

### 2.3 New `WaEvent` variants (additive, `serde(tag="type")`, then `npm run specta`)

```rust
GroupInfo { info: GroupInfoData } | JoinedGroup { info: GroupInfoData } |
Presence { from: String, unavailable: bool, last_seen: Option<i64> } |
ChatPresence { chat: String, sender: String, state: String } |
PrivacySettings { settings: PrivacyData } |
NewsletterLiveUpdate { id: String, update: NewsletterData } |
AppStateSyncProgress { name: String, applied: u64, total: u64 }
```

Existing variants (`Qr` … `SearchIndexProgress`) are untouched. Update `API-WHATSAPP.md` §2.

### 2.4 Go → Rust function tables

**Task A** (`appstate/lthash`, `hash`, `decode`, `encode`):

| Go (`whatsmeow/appstate/…`) | Rust |
|---|---|
| `lthash.go`: `New`, `Add`, `Subtract`, `PerformMutations` | `lthash.rs`: same, byte-identical digests |
| `hash.go`: `HashState`, `VerifyPatchMAC`, `VerifySnapshotMAC` | `hash.rs` |
| `decode.go`: `DecodeSyncActionData`, `DecodePatchList` | `decode.rs` |
| `encode.go`: `EncodeSyncActionData` | `encode.rs`, roundtrip-tested vs decode |

**Task B** (`appstate.go` → `client/appstate.rs`, `keys.rs`, `recovery.rs`):

| Go | Rust |
|---|---|
| `FetchAppState`, `fetchAppStatePatches` | `fetch_app_state` |
| `handleAppStateNotification`, `dispatchAppState` | `handle_app_state_notification` |
| `keys.go`: `GetAppStateSyncKey`, `handleAppStateSyncKeyShare` | `keys.rs` (self-generated keys only — key *shares* are deferred, see §5) |
| `recovery.go`: `SnapshotRecovery`, `PatchList` recovery | `recovery.rs` |

**Task C** (`group.go`, `newsletter.go`, `notification.go`, `user.go`):

| Go | Rust |
|---|---|
| `CreateGroup`, `GetGroupInfo`, `JoinGroupWithLink`, `LeaveGroup`, `UpdateGroupParticipants`, `SetGroupName/Topic`, `GetGroupInviteLink` | `client/group.rs` |
| `SubscribeNewsletter`, `UnsubscribeNewsletter`, `NewsletterJoin/Leave` | `client/newsletter.rs` |
| `handleGroupNotification`, `handlePictureNotification`, `handleNewsletterNotification` | `client/notification.rs` → `GroupInfo` events |
| `GetUserInfo`, `GetProfilePicture` | `client/user.rs` |

`tctoken.go`/`cstoken.go` (privacy tokens) and `broadcast.go` are ported only as far as presence
subscribe needs them — no standalone modules unless the call sites demand it.

## 3. Offline tests (no network, no real WhatsApp)

- `appstate::lthash` — golden vectors from `go run` (byte-identical to Go), add/subtract/perform-mutations
- `appstate::hash` — patch-MAC verify accepts good / rejects tampered
- `appstate::{decode,encode}` — encode→decode roundtrip; decode of a Go-produced blob
- `appstate::recovery` — snapshot + patch recovery converges to the snapshot hash
- `client::group` — cache hit/miss/invalidation, `CanSend` true/false, participant-action IQ building against a fake `send_iq`
- `client::appstate` — mutation dispatch writes mute/pin/archive into `ChatSettingsStore` (in-memory SQLite)
- Every test is `#[cfg(test)]`, deterministic, no sleeps, no sockets

Expected delta: **+25–35 engine tests** over the Phase 4 baseline. Report the real before/after counts.

## 4. Gates (main thread runs these)

- `cd engine && cargo test --workspace` green; new tests actually run (not `#[ignore]`d)
- `cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings` clean
- LTHash vectors match Go (re-run the `go run` generator if in doubt)
- `API-WHATSAPP.md` §2 lists every new event variant with its wire JSON

## 5. Explicitly deferred to Phase 6 (leave `TODO(Phase6)` markers)

These Phase 4 leftovers from `TODO.md` Phase 5 **Deferred** are in scope to *handle gracefully*
(parse, emit event or `UndecryptableMessage`, do not crash) but NOT to fully implement:

- History sync — parse `HistorySyncNotification`, emit event, do **not** backfill `wa-store`
- App-state key shares — self-generated keys only; patches needing a shared key stay unapplied
- LID-migration payloads — decode only, do not apply to `CachedLIDMap` (column `lid_migration_ts` already written)
- Bot decryption — `msmsg`/`msgsecret` stays unported → `UndecryptableMessage`
- Placeholder resends — do not send re-encrypt requests
- `NewsletterMessageMeta` — do not parse views/reaction counts
- `UseRetryMessageStore` — in-memory `recentMessages` (512) only
- `retryFrame` — message-level retry receipts only (folds into Phase 4 `retry.rs` otherwise)

Closing gate: `grep -rn "TODO(Phase6)" engine/crates/wa-core/src` lists exactly these eight, each
with a one-line reason.

---

# Part 2 — presence half (Tasks D–F)

You are implementing the presence half of **Phase 5 (Groups, AppState, Presence)** of ngWhatsapp.
The backend half (Tasks A–C: AppState core/sync, groups, newsletters, new `WaEvent` variants) is
merged — verify the §2 boundary contracts exist before writing code. Read `README.md` →
`ARCHITECTURE.md` → `API-WHATSAPP.md` (§2, §3.3–§3.5) → `AGENTS.md` → `TODO.md` (Phase 5) first.

## 1. File ownership (D = `wa-core`, E = `bebok-server` + `wa-store`, F = Angular)

| File | Action | Task |
|---|---|---|
| `engine/crates/wa-core/src/client/presence.rs` | **new** — port of `whatsmeow/presence.go` | D |
| `engine/crates/wa-core/src/client/privacysettings.rs` | **new** — port of `whatsmeow/privacysettings.go` | D |
| `engine/crates/bebok-server/src/routes/groups.rs` | **new** — `/group/*`, `/newsletter/*` (see §2.1) | E |
| `engine/crates/bebok-server/src/routes/presence.rs` | **new** — `/presence/*`, `/privacy` | E |
| `engine/crates/bebok-server/src/routes/mod.rs` | **extend** — routing table only | E |
| `engine/crates/wa-store/src/lib.rs` | **extend** — chat-settings/contact read path for the UI (mute/pin/archive) | E |
| `client/src-tauri/src/lib.rs` | **extend** — `group_*`, `presence_*`, `privacy_*` commands | E |
| `client/src/app/core/services/whatsapp.service.ts` | **extend** — same surface as E | E, F |
| `client/src/app/features/chats/` | **extend** — `chat-list` (mute/pin/archive), `group-info`, presence indicators | F |
| `client/src/app/features/groups/` | **new** — create, invite link, participants | F |
| `client/src/app/features/groups/*.spec.ts`, `chat-list.spec.ts` | **new/extend** — sort, mute/pin, create flow | F |
| `API-WHATSAPP.md` §3.3–§3.5 | **update** if routes differ | E |
| `TODO.md` Phase 5 checkboxes | **tick** what lands | D, E, F |

`types/events.rs` is owned by Task C (wave 3). Task E must not touch it — list needed changes for
the main thread instead.

## 2. Contracts

### 2.1 Routes (E provides, mirror `API-WHATSAPP.md` §3.3–§3.5)

```
POST /group/create {name, participants[]} → GroupInfo
GET  /group/:jid/info | POST /group/:jid/leave | POST /group/:jid/participants {action, participants[]}
POST /group/:jid/name {name} | POST /group/:jid/topic {topic}
GET  /group/:jid/invite-link?reset=bool | POST /group/join {code}
POST /newsletter/subscribe {id} | POST /newsletter/unsubscribe {id}
POST /presence/subscribe {jid} | POST /presence/send {type} | POST /presence/chat {chat, state}
GET  /privacy | POST /privacy {…}
GET  /chats (extend: mute/pin/archive from ChatSettingsStore)
```

`POST /disconnect` + `/logout` (still 501 from Phase 2) must be wired before presence work can be
exercised against a live session — confirm with Phase 3/4 or do it here and say so in the report.

### 2.2 Go → Rust (Task D)

| Go | Rust |
|---|---|
| `presence.go`: `SubscribePresence`, `SendPresence`, `SendChatPresence`, `handlePresence` | `client/presence.rs` → `Presence`/`ChatPresence` events |
| `privacysettings.go`: `GetPrivacySettings`, `SetPrivacySettings`, `handlePrivacySettings` | `client/privacysettings.rs` → `PrivacySettings` event |

### 2.3 Angular (Task F)

- `chat-list`: sort (pinned first, then recency), mute/pin/archive badges from `GET /chats`,
  presence dot from `Presence`/`ChatPresence` events
- `group-info`: name/topic/participants, invite-link display + copy, leave button
- `features/groups/`: create flow (name + participant picker), join-via-link
- `chat-list.spec.ts` (sort, mute/pin), `group-info.spec.ts` (render, leave) — new files
  (`features/chats/` has `chat-list.component.ts` but no spec today; `features/groups/` is new)

## 3. Offline tests

- Engine: `presence` (subscribe IQ shape, `SendChatPresence` state strings, event mapping)
- Client: `chat-list.spec.ts`, `group-info.spec.ts` (mocked `WhatsappService`, no backend)
- `cargo test --workspace` + `npm test` green; report real before/after counts

## 4. Gates (main thread runs these)

- `cd engine && cargo test --workspace` green, `cd client && npm test` green
- `cargo fmt` + `clippy -D warnings` clean, `ng build` clean
- `npm run specta` produces no diff unless wire types changed; `whatsapp.service.ts` matches new commands
- `API-WHATSAPP.md` §§2–3 match the implemented routes/events
- `TODO.md` Phase 5 boxes ticked; deferred eight still marked `TODO(Phase6)`

## 5. Out of scope → Phase 6

Settings/providers pages, `/config`, `/debug/log`, `/version`, code intelligence, release/packaging,
Playwright E2E, multi-device, proxy, vector-search cutoff tuning. The eight §5-backend deferred
items stay deferred.

## 6. Rules

- Every ported file: MPL-2.0 header + three-line boilerplate. No `unwrap`/`expect`/`panic!` outside `#[cfg(test)]`.
- `WaEvent` additions are additive only; `serde(tag="type")` + camelCase fields; regenerate `specta`.
- No schema drift: `wa_appstate*` tables unchanged; app-state version + mutation-MAC storage reuse existing columns.
- Store change: migration only if Go schema demands it + `PutDevice`/`GetDevice`-style test.
- IPC change: `specta` regenerated + `whatsapp.service.ts` updated.
- Frontend change: ask the user about browser verification (dev server + screenshot + console).
- English only, no i18n.

## 7. Report template (fill in, per member)

```
Task: <A–F> by <member> (re-tasked/taken over: <yes/no, why>)
Files: <created/extended>
Tests: <names + counts, before/after>
Gates: <cargo test / npm test / fmt / clippy / build / specta — pass?>
Deferred markers: <grep TODO(Phase6) output>
TODO.md: <boxes ticked>
Honest failure: <what did NOT land and why, or "none — all green">
```

**Honest-failure clause:** if any gate is red, a prerequisite is missing, or a scope item cannot be
done offline, stop that task and report it plainly — do not fake green, do not silently descope.
An aborted/errored child reports no text: `task_report` it before re-tasking.
