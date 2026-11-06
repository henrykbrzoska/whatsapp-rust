# API-WHATSAPP.md — WhatsApp contract (Tauri IPC + HTTP/SSE)

Source: the Go client (`client.go`, `types/events/events.go`, `store/store.go`, `pair*.go`, `message.go`, `send.go`, `group.go`, `presence.go`, `appstate/*`).
Rust port: `engine/crates/wa-core/` + `engine/crates/bebok-server/`.
Frontend: `client/src/app/core/services/whatsapp.service.ts` (Tauri `invoke`/`listen`, or HTTP+SSE in browser mode).

Status legend: ✅ implemented, 🚧 returns `501 Not Implemented` for now, unmarked = planned.

## 1. Transport

### 1.1 Tauri (desktop)

```ts
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
await invoke("connect");
const unlisten = await listen<WaEvent>("wa-event", (e) => handle(e.payload));
```

Wire types derive `specta::Type` (`WaEvent` in `wa-core/src/types/events.rs`, store rows in
`wa-store/src/lib.rs`); `npm run specta` runs `bebok-server/src/bin/export-types.rs` and writes
`client/src/app/types/specta.ts`, which `client/src/app/core/models.ts` re-exports. Tauri commands
gain `#[tauri::command] #[specta::specta]` in Phase 3. `bebok-server/tests/specta_bindings.rs`
fails when the committed bindings drift from the Rust types.

### 1.2 Browser (HTTP + SSE)

```
GET  http://127.0.0.1:8787/?token=...          # BEBOK_READY handshake
GET  /version, /config, /debug/log
GET  /events  (SSE, text/event-stream)         # WaEvent stream
GET  /connected, /logged_in                    # session state
POST /connect                                  # start the session (handshake)
POST /disconnect, /logout                      # Phase 3
POST /send/message, /send/media, /group/*, /presence/*
```

Auth: `Authorization: Bearer <token>` or `?token=`. `BEBOK_NO_AUTH=1` disables it.

## 2. Events (WaEvent)

A discriminated union `#[serde(tag="type")]`, emitted as `wa-event` (Tauri) and `data:` (SSE).

```ts
type WaEvent =
  | { type: "qr"; codes: string[] }
  | { type: "pair_success"; id: string; lid: string; businessName: string; platform: string }
  | { type: "pair_error"; error: string }
  | { type: "connected" }
  | { type: "disconnected" }
  | { type: "logged_out"; onConnect: boolean; reason: number }
  | { type: "stream_replaced" }
  | { type: "connect_failure"; reason: number; message: string }
  | { type: "client_outdated" }
  | { type: "temporary_ban"; code: number; expireMs: number }
  | { type: "keep_alive_timeout"; errorCount: number }
  | { type: "keep_alive_restored" }
  | { type: "message"; info: MessageInfo; message: string | null; raw: string | null; text: string | null; isEphemeral: boolean; isViewOnce: boolean; isEdit: boolean; retryCount: number }  // `message`/`raw` are hex-encoded `waE2E.Message` (protojson is TODO Phase 5)
  | { type: "fb_message"; info: MessageInfo; transport: string | null; application: string | null; retryCount: number }  // hex-encoded armadillo envelopes
  | { type: "receipt"; chat: string; sender: string; messageIds: string[]; timestamp: string; receiptType: string; messageSender: string }
  | { type: "chat_presence"; chatPresence: ChatPresenceData }
  | { type: "presence"; presence: PresenceData }
  | { type: "history_sync"; data: unknown }
  | { type: "undecryptable_message"; info: MessageInfo; isUnavailable: boolean; unavailableType: string; decryptFailMode: string }
  | { type: "group_info"; info: GroupInfoEvent }
  | { type: "joined_group"; info: JoinedGroup }
  | { type: "picture"; jid: string; author: string; timestamp: number | null; remove: boolean; pictureId: string }
  | { type: "user_about"; jid: string; status: string }
  | { type: "identity_change"; jid: string; implicit: boolean }
  | { type: "privacy_settings"; settings: PrivacySettingsData }
  | { type: "blocklist"; action: string; changes: unknown[] }
  | { type: "offline_sync_preview"; total: number; messages: number }
  | { type: "offline_sync_completed"; count: number }
  | { type: "media_retry"; messageId: string; chatId: string }
  | { type: "newsletter_join"; id: string }
  | { type: "newsletter_leave"; id: string }
  | { type: "newsletter_live_update"; id: string; time: number | null; messages: NewsletterMessageData[] }
  | { type: "app_state_sync_progress"; name: string; applied: number; total: number };
```

Go → Rust → TS mapping: `types/events/events.go` → `wa-core/src/types/events.rs` (serde + specta).

The Phase 5 payload types (all camelCase on the wire, all in `wa-core/src/types/events.rs`):

```ts
type GroupInfoEvent = {            // Go events.GroupInfo, "group_info"
  jid: string; notify: string; sender: string; senderPn: string; timestamp: number | null;
  joinReason: string;
  name?: GroupName; topic?: GroupTopic; locked?: { isLocked: boolean };
  announce?: { isAnnounce: boolean; announceVersionId: string };
  ephemeral?: { isEphemeral: boolean; disappearingTimer: number };
  delete?: { deleted: boolean; deleteReason: string };
  link?: GroupLinkChange; unlink?: GroupLinkChange;
  newInviteLink?: string; prevParticipantVersionId: string; participantVersionId: string;
  join: string[]; leave: string[]; promote: string[]; demote: string[];
  unknownChanges: string[]; membershipApprovalMode: boolean; suspended: boolean; unsuspended: boolean;
};
type JoinedGroup = {              // Go events.JoinedGroup, "joined_group"
  reason: string; type: string; createKey: string; sender: string; senderPn: string; notify: string;
  info: GroupInfo;
};
type GroupInfo = {                // Go types.GroupInfo
  jid: string; ownerJid: string; ownerPn: string; name: string; nameSetAt: number | null;
  nameSetBy: string; nameSetByPn: string; topic: string; topicId: string;
  topicSetAt: number | null; topicSetBy: string; topicSetByPn: string; topicDeleted: boolean;
  isLocked: boolean; isAnnounce: boolean; announceVersionId: string;
  isEphemeral: boolean; disappearingTimer: number; isIncognito: boolean; isParent: boolean;
  defaultMembershipApprovalMode: string; linkedParentJid: string; isDefaultSubGroup: boolean;
  isJoinApprovalRequired: boolean; addressingMode: string; groupCreated: number | null;
  creatorCountryCode: string; participantVersionId: string;
  participants: GroupParticipant[]; participantCount: number; memberAddMode: string; suspended: boolean;
};
type GroupParticipant = {         // Go types.GroupParticipant
  jid: string; lid: string; phoneNumber: string; displayName: string;
  isAdmin: boolean; isSuperAdmin: boolean; error: number;
  addRequest?: { code: string; expiration: number | null };
};
type NewsletterMessageData = {    // Go types.NewsletterMessage
  serverId: number; messageId: string; messageType: string; timestamp: number | null;
  message: string | null;          // hex-encoded waE2E::Message
};
```

`PresenceData`, `ChatPresenceData` and `PrivacySettingsData` are the payload types of
`client/presence.rs` and `client/privacysettings.rs` — the event wrappers carry them unchanged.

### 2.1 QR

* `QR { codes: string[] }` — codes to render as QR (first one 60s, the rest 20s). When they run out, the server closes the WS — call `connect` again.
* `QRScannedWithoutMultidevice` — phone without multidevice; the same QR can still be scanned.

## 3. Commands / HTTP endpoints

### 3.1 Auth and connection

| Tauri command | HTTP | Description |
|---|---|---|
| `connect()` | `POST /connect` ✅ (handshake only) | Starts the session and returns the state at once: the dial + Noise handshake run in the background, so the request never waits for the QR flow. Answers `{ state, connected, loggedIn, error, attempts, replaced }` (`API-WHATSAPP.md` §2 events report progress: `connected` / `connect_failure`). A second call while a session exists is a no-op. `replaced: true` means WhatsApp closed this socket because another login took the device over (`conflict type=replaced`); `error` carries the reason and clients must not auto-reconnect until the user connects explicitly. |
| `disconnect()` | `POST /disconnect` 🚧 | `Client::Disconnect()` — closes the WS, does not emit `disconnected` |
| `is_connected(): bool` | `GET /connected` ✅ | `Client::IsConnected()` — `true` once the Noise handshake completed |
| `is_logged_in(): bool` | `GET /logged_in` ✅ | `Client::IsLoggedIn()` — `true` when the store holds a paired device (`device_jids` non-empty) |
| `wait_for_connection(timeoutMs)` | — | `Client::WaitForConnection` |
| `logout()` | `POST /logout` ✅ | `Client::Logout()` — `remove-companion-device` + `Store.Delete()` |
| `get_qr_channel()` | — | QR subscription (alternative to the event) |
| `pair_with_code(phone, code)` | `POST /pair/code` | `Client::PairWithCode` |
| `pair_with_passkey(...)` | `POST /pair/passkey` | `Client::SendPasskeyResponse` / `SendPasskeyConfirmation` |

Errors: `ErrAlreadyConnected`, `ErrNotConnected`, `ErrNotLoggedIn`, `ErrClientIsNil`, `store.ErrDeviceDeleted`.

### 3.2 Messages

| Command | HTTP | Description |
|---|---|---|
| `send_message(to, text, opts?)` | `POST /send/message` | `Client::SendMessage` → `MessageID` |
| `send_media(to, buffer, mime, opts?)` | `POST /send/media` (multipart: `to`, `file`, `mime?`, `caption?`, `kind=image\|gif`) | `Client::Upload` → `ImageMessage` (or `VideoMessage` with `gif_playback` for GIFs) → `Client::SendMessage` |
| `fetch_history(chat, limit?)` | `GET /history?chat=&limit=` | history sync / `ParseWebMessage` |
| `get_message(chat, id)` | `GET /message?chat=&id=` | `GetMessageForRetry` / `EventBuffer` |
| `mark_read(chat, ids)` | `POST /receipt/read` | `read` receipt |
| `mark_played(chat, ids)` | `POST /receipt/played` | `played` receipt |

`MessageInfo`:

```ts
type MessageInfo = {
  id: string; chat: string; sender: string;
  isFromMe: boolean; isGroup: boolean;
  pushName: string; timestamp: string;
  deviceSentMeta?: { destinationJid: string; phash: string };
  msgMetaInfo?: { threadMessageId: string; threadMessageSenderJid: string };
};
```

### 3.3 Groups

| Command | HTTP | Description |
|---|---|---|
| `create_group(name, participants)` | `POST /group/create` | `Client::CreateGroup` |
| `get_group_info(jid)` | `GET /group/{jid}/info` | `Client::GetGroupInfo` (cache first) |
| `join_group(inviteLink)` | `POST /group/join` | `Client::JoinGroupWithLink` |
| `leave_group(jid)` | `POST /group/{jid}/leave` | `Client::LeaveGroup` |
| `update_group_participants(jid, action, jids)` | `POST /group/{jid}/participants` | add/remove/promote/demote |
| `set_group_name(jid, name)` | `POST /group/{jid}/name` | |
| `set_group_topic(jid, topic)` | `POST /group/{jid}/topic` | |
| `get_group_invite_link(jid, reset?)` | `GET /group/{jid}/invite-link?reset=` | |

### 3.4 Contacts, presence, privacy

| Command | HTTP | Description |
|---|---|---|
| `get_contact(jid)` | `GET /contact?jid=` | `ContactStore::GetContact` |
| `get_all_contacts()` | `GET /contacts` | `ContactStore::GetAllContacts` |
| `get_profile_picture(jid)` | `GET /picture?jid=` | `Client::GetProfilePictureInfo` |
| `subscribe_presence(jid)` | `POST /presence/subscribe` | `Client::SubscribePresence` |
| `send_presence(state)` | `POST /presence/send` | `Client::SendPresence` (`available`/`unavailable`) |
| `send_chat_presence(chat, state, media)` | `POST /presence/chat` | `composing`/`paused` |
| `get_privacy_settings()` | `GET /privacy` | `Client::GetPrivacySettings` |
| `set_privacy_setting(name, value)` | `POST /privacy {name, value}` | one setting per call, returns full `PrivacySettings` |
| `get_blocklist()` | `GET /blocklist` | `Client::GetBlocklist` |
| `block(jid)` / `unblock(jid)` | `POST /block`, `POST /unblock` | |

### 3.5 Newsletters

| Command | HTTP | Description |
|---|---|---|
| `newsletter_subscribe(jid)` | `POST /newsletter/subscribe` | |
| `newsletter_unsubscribe(jid)` | `POST /newsletter/unsubscribe` | |
| `newsletter_mark_viewed(jid, ids)` | `POST /newsletter/viewed` | |

### 3.6 Store / Device

| Command | HTTP | Description |
|---|---|---|
| `get_devices()` | `GET /devices` | `Container::GetAllDevices` |
| `get_device(jid)` | `GET /device?jid=` | `Container::GetDevice` |
| `new_device()` | `POST /device/new` | `Container::NewDevice` (without Save) |
| `delete_device(jid)` | `DELETE /device?jid=` | `Device::Delete` |

### 3.7 Media

| Command | HTTP | Description |
|---|---|---|
| `upload_media(buffer, mime)` | `POST /media/upload` | `Client::Upload` → URL |
| `download_media(message)` | `POST /media/download` | `Client::Download` → bytes |
| `get_media_conn(forceRefresh?)` | `GET /media/conn` | `MediaConn` cache |
| `fetch_history(chat?, count?)` | `POST /history/fetch` ✅ | Asks our own phone for the messages before the oldest one stored, for one chat or every known chat. Answers with what was *requested* (`{count, requested[], skipped[]}`); the blob arrives minutes later and shows up as a `history_imported` event. A chat with no stored message cannot be anchored and is skipped |
| `media_info(id)` | `GET /media/{id}/info` ✅ | `{id, kind, available, cached, mimeType, size}` without downloading, so the UI can pick between an inline element and a label |
| — (an `<img>`/`<video>`/`<audio>` `src`) | `GET /media/{id}` ✅ | The decrypted attachment bytes with the sender's `Content-Type`. Served from the `media_blobs` cache (1-hour TTL, purged opportunistically and every 10 minutes); a miss downloads from the CDN and needs a live session. The token may ride in `?token=`, because a media element cannot send an `Authorization` header |

### 3.8 Local database — search + history (wa-store, from day 0)

Lightweight local database: SQLite + FTS5 + sqlite-vec (see `ARCHITECTURE.md` §6). Every message (`WaEvent::Message`/`HistorySync`) goes into `messages` + `messages_fts` + `message_embeddings` (384d embedding, `fastembed` MiniLM, offline).

| Tauri command | HTTP | Description |
|---|---|---|
| `search_messages(query, opts?)` | `GET /search?q=&chat=&limit=&mode=` ✅ | Hybrid FTS5 (BM25) + vec KNN (cosine) → RRF. `mode=hybrid|keyword|vector`, `chat` filters by JID, `limit` default 20 |
| `export_chat_history(chat, opts?)` | `GET /chats/:jid/history/export?format=&from=&to=` ✅ | Streamed `json|jsonl|csv`, paginated by `timestamp`/`id`, `Content-Disposition: attachment`. Large chats: NDJSON + `history_export_progress` event |
| `delete_chat_messages(chat)` | `DELETE /chats/:jid/messages` ✅ | Clears `messages` + FTS + vec for the chat |
| `get_chat_stats(chat)` | `GET /chats/:jid/stats` ✅ | Message count, date range, size |
| — | `POST /debug/seed` ✅ | Insert test messages (JSON array). Only with `--diagnostic` / `BEBOK_DIAGNOSTIC=1` |

Additional events:

```ts
| { type: "history_export_progress"; chat: string; exported: number; total: number }
| { type: "search_index_progress"; indexed: number; total: number }
```

Frontend: `SearchBar` (`mode` toggle keyword/vector/hybrid) → `whatsapp.service.search()`. "Download full chat history" button in `chat.component` (chat header) → `GET /chats/:jid/history/export` → `blob` → download (progress bar for large chats).

## 4. JID types

```ts
type JID = string; // "48123456789@s.whatsapp.net" | "123@g.us" | "123@lid" | "status@broadcast"
```

Servers: `s.whatsapp.net` (user), `g.us` (group), `lid` (hidden), `broadcast` (status), `newsletter`.
`LIDStore` maps `lid ↔ pn` globally.

## 5. Errors

All commands return `Result<T, String>` (Tauri) / `{ error: string }` (HTTP 4xx/5xx).
Codes from `types/events`:

* `ConnectFailureReason`: 400 generic, 401 logged_out, 402 temp_banned, 403 main_gone, 405 outdated, 409 bad_ua, 413/414 CAT, 500/503 server.
* `TempBanReason`: 101 too_many_people, 102 blocked, 103 too_many_groups, 104 same_message, 106 broadcast.

## 6. Example — WhatsappService

```ts
@Injectable({ providedIn: 'root' })
export class WhatsappService {
  private tauri = inject(TauriService);
  connectionState = signal<"disconnected"|"qr"|"connected">("disconnected");
  qrCodes = signal<string[]>([]);

  async connect() { await this.tauri.invoke("connect"); }
  async sendMessage(to: string, text: string) {
    return this.tauri.invoke<string>("send_message", { to, text });
  }
  listen() {
    return this.tauri.listen<WaEvent>("wa-event", (e) => {
      if (e.type === "qr") { this.qrCodes.set(e.codes); this.connectionState.set("qr"); }
      if (e.type === "connected") this.connectionState.set("connected");
      if (e.type === "message") this.handleMessage(e);
    });
  }
}
```

## 7. Versioning

* `GET /version` ✅ → `{ version: "0.1.0", protocolRef: "<git sha>" }`
* `specta` — a breaking change in `WaEvent` requires a minor bump + regenerating `client/src/app/types/specta.ts`.
