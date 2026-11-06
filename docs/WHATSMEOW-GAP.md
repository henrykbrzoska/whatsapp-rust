# whatsmeow → ngWhatsapp — gap analysis

Date: 2026-11-06. Source: `whatsmeow/*.go` (40 root files), `whatsmeow/types/events/events.go` (76 event types),
public `Client` methods vs `engine/crates/wa-core/src/**`, `engine/crates/bebok-server/src/routes/mod.rs`,
Tauri commands (`client/src-tauri`) and `client/src/app/**`.
Baseline: Phases 0–5 merged (see `docs/PHASE-6-GAP.md`).

Method: Go symbols were matched against Rust/TS by name (grep + code index). A few items may exist under a
different name — verify before starting work on any single item.

## Verdict

The protocol core is largely ported: Noise, binary, Signal, store, QR pairing, groups, presence, AppState
and part of newsletters. What is missing is the path from a message to the user:

* messages cannot be sent over HTTP/IPC,
* history-sync messages are not persisted in wa-store (live messages are, since step 1),
* there is no media support (upload/download/mediaconn),
* there is no message view in the UI.

Search and export now also see live incoming messages (step 1, A5). Until A4 is wired, history-sync data
is still only reachable through `POST /debug/seed`.

Progress log:
* Step 1 (partial): A5 live-message persistence + A4 blob decoder (4 + 8 tests green). Remaining for A4/A5: fetch the blob (A2/A3), call `decode_history_blob`, then `insert_messages`.

## A. Critical (blocks QR → send/receive → history)

| # | whatsmeow | State in ngWhatsapp |
|---|---|---|
| A1 | `upload.go`: `Upload`, `UploadReader`, `UploadNewsletter`, `DeleteMedia` | ❌ missing in wa-core |
| A2 | `download.go` / `download-to-file.go`: `Download*`, `DownloadThumbnail`, `FetchStickerPack` | ❌ missing |
| A3 | `mediaconn.go` (`mediaConnCache`, refresh) | ❌ missing (`client.rs:579` TODO) |
| A4 | `message.go`: `DownloadHistorySync` | ⚠️ decode half done: `wa-core/src/client/history_blob.rs::decode_history_blob` (zlib + `HistorySync` → `HistoryMessage`, tested). Not wired: the blob download is blocked on A2/A3, and the decoded records are not yet written to wa-store |
| A5 | Persisting messages (`Message`, history sync) into wa-store | ⚠️ live `WaEvent::Message` is persisted by the event sink (`bebok-server/src/connection.rs::event_sink`, tested). History-sync records are not persisted yet (waits on A4 download) |
| A6 | `send.go`: `SendMessage`, `RevokeMessage`, `BuildEdit/Reaction`, `SetDisappearingTimer` | ⚠️ present in wa-core (`client/send.rs`) but no HTTP endpoint or Tauri command |
| A7 | `receipt.go`: `MarkRead`, `mark_played` | ⚠️ present in wa-core, no endpoint or IPC |
| A8 | `prekeys.go`: prekey upload and `sendUnifiedSession` after pairing | ⚠️ `pair.rs:381` TODO; `send.rs:1428` has a temporary workaround |
| A9 | `StoreLIDPNMapping`, `GetLIDForPN`, `migrateSessionStore` | ⚠️ TODO in `pair.rs:461`, `connection_events.rs:91`, `message.rs:103`. Per AGENTS.md §8 a bug here means missing messages |
| A10 | Offline sync (`OfflineSyncPreview` / `OfflineSyncCompleted`) | ❌ `connection_events.rs:206` TODO |

## B. Domains not ported at all

| whatsmeow | Missing |
|---|---|
| `call.go` | `RejectCall` and `<call>` node handling; no `Call*` events (Offer, Accept, Reject, Terminate, Transport, RelayLatency, …) |
| `broadcast.go` | `GetStatusPrivacy`, status/stories, broadcast lists (`message.rs:222` TODO) |
| `business.go` | `GetOrderDetails`, `GetBusinessProfile`, `BusinessName` / verified name (`message.rs:288,312`) |
| `push.go` | `RegisterForPushNotifications`, `GetServerPushNotificationConfig` |
| `pair-code.go` | `PairPhone` — pairing with a code instead of QR |
| `pair-passkey.go` | `SendPasskeyResponse/Confirmation` and `PairPasskey*` events |
| `sendfb.go` / `armadillomessage.go` | `SendFBMessage`, `handleDecryptedArmadillo` (`message.rs:532`, `fb_message.rs:47`) |
| `reportingtoken.go`, `cstoken.go` | missing (`tc_token` exists) |
| `connectionevents.go` | CAT refresh on `413`/`414` and `CATRefreshError` (`connection_events.rs:113`) |
| `internals.go` | `DangerousInternals` (low priority) |

## C. Partial ports in wa-core

* **`user.go`:** missing `IsOnWhatsApp`, `SetStatusMessage`, `GetContactQRLink`, `ResolveContactQRLink`,
  `ResolveBusinessMessageLink`, `GetBotListV2`, `GetBotProfiles`. Bot handling in `ack.rs:95` is a TODO.
* **`group.go`:** missing `SetGroupPhoto`, `GetJoinedGroups`, `GetSubGroups`, `LinkGroup`/`UnlinkGroup`
  (communities) and join-request handling (`GetGroupRequestParticipants`, `UpdateGroupRequestParticipants`).
* **`newsletter.go`:** missing `CreateNewsletter`, `GetNewsletterInfo*`, `GetSubscribedNewsletters`,
  `NewsletterMarkViewed`, `NewsletterSendReaction`. `w:mex` mutations are not ported (`send.rs:702`), and
  newsletters always downgrade the receipt type (`receipt.rs:157`).
* **AppState:** `dispatch_mutation` only handles `Mute`, `Pin`, `Archive` and `Contact`. Missing: Star,
  DeleteForMe, DeleteChat, ClearChat, MarkChatAsRead, labels, PushNameSetting, UnarchiveChatsSetting.
  Sending patches (`SendAppState`) and `MarkNotDirty` are also missing.
* **`message.go` / `retry.go`:** missing `IdentityChange` with `clearUntrustedIdentity` (`message.rs:517`),
  `immediateRequestMessageFromPhone` (`message.rs:384`) and peer-message retry (`retry_receipt.rs:116`).
  Media captions in `types/message.rs:307` are a TODO.
* **`receipt.go`:** missing `SetForceActiveDeliveryReceipts`.
* **Other code TODOs:** handler queue and `unpack` (`client.rs:287,1091`), `userDevicesCache` (`client.rs:579`),
  device registration in `types/version.rs:155`, `getCachedGroupData` in `send.rs:1401`.

## D. Go events missing from `WaEvent`

About 48 of 76 types are missing. The most important:

* **Sync:** `AppState`, `AppStateSyncComplete`, `AppStateSyncError`, `OfflineSyncPreview`, `OfflineSyncCompleted`
* **Chat state:** `Archive`, `Pin`, `Mute`, `Star`, `DeleteChat`, `DeleteForMe`, `ClearChat`, `MarkChatAsRead`, `UnarchiveChatsSetting`
* **Contacts and profile:** `Contact`, `PushName`, `PushNameSetting`, `BusinessName`, `UserAbout`, `UserStatusMute`, `Blocklist`, `BlocklistChange`
* **Labels:** `LabelEdit`, `LabelAssociationChat`, `LabelAssociationMessage`
* **Newsletters:** `NewsletterJoin`, `NewsletterLeave`, `NewsletterMuteChange`, `NewsletterMessageMeta`, `NewsletterMilestone`
* **Calls:** all `Call*` and `UnknownCallEvent`
* **Other:** `MediaRetryError`, `CATRefreshError`, `RotateADVSecret`, `MexNotificationData`, `NotifyAccountReachoutTimelock`, `PairPasskey*`

## E. Missing HTTP endpoints and Tauri commands

Documented in `API-WHATSAPP.md`, but absent from `routes/mod.rs` and `src-tauri`:

* **Send:** `send_message`, `send_media`, revoke, edit, reaction, disappearing timer
* **History:** `fetch_history` / `GET /history`, `GET /message`
* **Receipts:** `POST /receipt/read`, `POST /receipt/played`
* **Media:** `POST /media/upload`, `POST /media/download`, `GET /media/conn`
* **Devices:** `GET /devices` (`get_user_devices` already exists in wa-core)
* **Pairing:** `get_qr` (the `Qr` event exists, the pull command doesn't), `POST /pair/code`, `POST /pair/passkey`
* **Other:** `wait_for_connection` (exists in wa-core), `group/{jid}/announce|locked|approval|photo`,
  newsletter info/viewed/reaction/mute, `is_on_whatsapp`, set status message
* **Tauri:** `connect`, `logout` and `is_connected` are not registered as commands (some may go through the HTTP fallback).

## F. Frontend (`client/src/app`)

* **No `features/messages/`** — message view with virtual scroll, composer, media preview
  (`ARCHITECTURE.md` §5). `chat.component.ts:72` only renders a placeholder.
* **`whatsapp.service.ts` lacks** `sendMessage`, `sendMedia`, `fetchHistory`, `markRead`, `downloadMedia`, `pairCode`.
* **No UI for:**
  * pairing with a code or passkey (login only knows QR),
  * incoming calls,
  * status/stories,
  * labels,
  * star / archive / mute / pin state in the chat list,
  * newsletters (beyond subscribing),
  * communities and group photo.

## Suggested order

1. **A5 + A4** — persist messages and history sync into wa-store.
2. **A6/A7 + E** — endpoints and IPC for send/history/receipt, then `features/messages`.
3. **A1–A3** — media.
4. **A8–A10** — LID, prekeys, offline sync.
5. **C (AppState) + D** — remaining AppState mutations and missing events.
6. **B** — remaining domains: pair-code and calls first, then business, push and FB/Armadillo.
