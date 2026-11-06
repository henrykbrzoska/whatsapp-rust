# Phase 4 prompt — Messaging + Crypto (the hardest phase)

Copy the block below into a fresh session to start Phase 4.

---

You are implementing **Phase 4 (Messaging + Crypto)** of ngWhatsapp — an Angular + Tauri 2 client
whose WhatsApp engine is a Rust port of `whatsmeow/` (Go, MPL-2.0). Phases 0–2 are merged; Phase 3
(`docs/PROMPT-PHASE-3.md` — `struct Client`, connection state machine, pairing, keepalive, QR
screen) is the direct prerequisite of this work. Read `README.md` → `ARCHITECTURE.md` →
`API-WHATSAPP.md` → `AGENTS.md` → `TODO.md` (Phase 4) first.

The engine is the source of truth: no protocol logic moves into the frontend, no keys leave Rust.
Every ported Rust file keeps the MPL-2.0 header + the three-line boilerplate. English only, no
i18n, no `unwrap`/`expect`/`panic!` outside `#[cfg(test)]`.

Your work is **seven tasks in two halves**:

* **Backend half** — Task A (`crypto/` utils), Task B (Signal layer + store adapter + pre-keys),
  Task C (incoming message path).
* **Sending half** — Task D (send / retry / receipts), Task E (media up/download), Task F (server
  endpoints + Tauri commands + `wa-store` wiring), Task G (Angular message UI).

Task ownership (§1.3 of the sending half + §2.4 of the backend half): never edit files outside the
ownership list of the task you are on. The two halves meet at the boundary contract in backend §2.3
(Signal API, pre-key API, `parse_message_info`, `send_ack`, single `dispatch_event` sink).

Protocol sources: `whatsmeow/send.go`, `retry.go`, `mediaretry.go`, `receipt.go`, `message.go`,
`upload.go`, `download.go`, `mediaconn.go`, `download-to-file.go`, `prekeys.go`, crypto utils
(`util/keys`, `hkdfutil`, `gcmutil`, `cbcutil`). Never modify `whatsmeow/`.

## Baseline (do not regress)

- `cd engine && cargo test --workspace` → green (was **183 tests** before Phase 3; Phase 3 grew it)
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
| 1 | Task A — `crypto/` utils | Task G — Angular message UI, coded against local interfaces + mocked `WhatsappService` | no shared files; G needs no engine code yet |
| 2 | Task B — Signal layer + store adapter + pre-keys | Task E — media up/download (uses A's `hkdf`/`cbc`) | both need only A |
| 3 | Task C — incoming message path | Task F, part 1 — `wa-store` write path design + media routes/Tauri commands on top of E | C owns `types/events.rs` this wave; F part 1 must not touch it |
| 4 | Task D — send / retry / receipts | Task F, part 2 — send/receipt routes + Tauri commands against the D signatures in §2.1 (sending half) | D owns `client/send.rs`, F only calls it |
| 5 | — (main thread runs every gate) | Task G, part 2 — switch G to the `specta` types, wire `message`/`receipt`/`media_*` events | after `npm run specta` |

Rules for every wave:

* **Self-contained briefs.** A member cannot see this document unless you tell it to read it. Each
  brief names: the task section to read in this file, the exact file-ownership list (§2.4 backend /
  §1.3 sending), the boundary contract it may rely on, the tests it must add, and a stop condition
  ("stop after `cargo test -p wa-core crypto::` is green and report the files changed").
* **Shared files are yours.** `src/client.rs` (`mod` lines/fields), `src/lib.rs`, workspace
  `Cargo.toml` and `types/events.rs` conflicts are applied by you between waves, never by two members
  at once. Tell members to list the edits they need there instead of making them, unless the wave
  table gives them that file.
* **Gate between waves.** After `task_wait` returns both results, run the §8 (backend) / §6
  (sending) gates yourself. Fix or re-task failures before you start the next wave.
* **Supervise.** Loop `task_wait` (60–150 s) → `task_status`. A child flagged looping/wandering →
  `task_cancel` → `task_report` (work log, changed files, last output) → re-task the same part with a
  tighter brief that builds on its partial work. Flagged a second time → finish that part yourself.
  An errored/aborted child reports no text: always `task_report` it before re-tasking.
* **Fallback.** If a member cannot run at all (see the environment notes at the end), give its
  wave slot to the other member on the next wave or do it yourself, and say so in the report. Never
  skip a task because delegation failed.
* **Report by member name**: which member did which task, which were re-tasked or taken over.

---

# Part 1 — backend half (Tasks A–C)

You are implementing the backend half of **Phase 4 (Messaging + Crypto)** of ngWhatsapp — an Angular +
Tauri 2 client whose WhatsApp engine is a Rust port of `whatsmeow/` (Go). Phases 0–2 are merged;
Phase 3 (`docs/PROMPT-PHASE-3.md` — `struct Client`, connection state machine, handler queue) is the
direct prerequisite of this work.

Your half is **three tasks**:

* **Task A** — `crypto/`: HKDF, AES-GCM, AES-CBC, identity/pre-key helpers (port of `util/hkdfutil`,
  `util/gcmutil`, `util/cbcutil`, `util/keys`).
* **Task B** — the Signal layer: session/sender-key records, `SessionBuilder`/`SessionCipher`,
  `GroupSessionBuilder`/`GroupCipher`, the async store adapter over `SqlStore`
  (`IsTrustedIdentity`/`PutIdentity`), and pre-key upload (`prekeys.go`).
* **Task C** — the incoming message path: `handleEncryptedMessage`, `handleUnencryptedMessage`,
  `parseMessage{Source,Info}`, `decryptMessages`/`decryptDM`/`decryptGroupMsg`, `UnwrapRaw`,
  `ParseWebMessage`, `FBMessage` (armadillo), plus the acks and the retry receipt those paths send.

Everything about **sending** (`send.go`, `mediaretry.go`, media upload/download, `receipt.go`,
`RecentMessages`, Tauri commands, `wa-store` insertion) belongs to the sibling half — see §2.3 for the
exact boundary contract you must expose.

The engine is the source of truth: no protocol logic moves into the frontend, no keys leave Rust.

---

## 1. Baseline — what exists and must be reused

Verify these before writing code (`cargo test -p wa-core` must be green *before* you start):

| Area | Symbol (verified path) | Notes |
|---|---|---|
| Handshake | `client::do_handshake_with`, `verify_server_cert_with`, `HandshakeOptions`, `CertError`, `ClientError` | `engine/crates/wa-core/src/client.rs` |
| Client | `struct Client` + `send_node`, `send_node_and_get_data`, `send_iq`, `send_iq_async`, `dispatch_event`, `generate_request_id`, `wait_for_connection`, `is_connected`, `is_logged_in`, `device()` | Phase 3 (`client.rs` + `client/*.rs`) |
| Node loop | `handle_frame`, `handler_queue_loop`, `handle_node`, `is_known_node` | Phase 3; `"message"`/`"status"`/`"appdata"` are `TODO(Phase 4)` today — you close them |
| Transport | `socket::{FrameSocket, NoiseSocket, NoiseHandshake}` | Phase 2 |
| Binary | `binary::{Node, NodeContent, AttrValue, Attrs, marshal, unmarshal, unpack}`; `Node::{new, attr, with_content, children, children_by_tag, optional_child_by_tag, child_by_tag, as_bytes, as_str}`; `Node::attr_getter() -> AttrGetter` | `binary/mod.rs`, `binary/attrs.rs` |
| `AttrGetter` | `jid`, `optional_jid`, `get_jid`, `string`, `optional_string`, `int`, `optional_int`, `int64`, `uint64`, `bool`, `unix_time`, `optional_unix_time`, `optional_unix_milli`, `error` | 1:1 with Go's `AttrGetter` |
| Keys | `crypto::keys::{KeyPair, PreKey, DJB_TYPE, clamp, djb_public, calculate_signature, verify_signature}`; `KeyPair::{generate, from_private, create_signed_pre_key, sign}`; `PreKey::{generate, from_private}` | `crypto/keys.rs` — **done in Phase 1**; extend, don't rewrite |
| Stores | traits `IdentityStore` (`put_identity`, `delete_identity`, `delete_all_identities`, `is_trusted_identity`), `SessionStore` (`get_session`, `has_session`, `get_many_sessions`, `put_session`, `put_many_sessions`, `delete_session`, `delete_all_sessions`, `migrate_pn_to_lid`), `PreKeyStore` (`get_or_gen_pre_keys`, `gen_one_pre_key`, `get_pre_key`, `remove_pre_key`, `mark_pre_keys_as_uploaded`, `uploaded_pre_key_count`), `SenderKeyStore` (`put_sender_key`, `get_sender_key`), `MsgSecretStore`, `EventBuffer` (`get_buffered_event`, `put_buffered_event`, `do_decryption_txn`, `clear_buffered_event_plaintext`, `delete_old_buffered_hashes`), `LidStore` | `store/traits.rs`; **all already implemented** for `SqlStore` in `store/sqlite/store.rs` |
| Device | `Device { noise_key, identity_key, signed_pre_key, registration_id, adv_secret_key, id, lid, account: Option<AdvSignedDeviceIdentity>, … }`; `Device::{get_jid, get_lid, get_alt_jid, save, delete, set_all_stores}`; `StoreError` | `store/device.rs` |
| Container | `Container::{new, open, open_memory, get_first_device, get_device, new_device, put_device}`, cached global LID map | `store/sqlite/container.rs` |
| Types | `Jid` (+ `signal_address_user()`, `to_non_ad`, `with_server`, `with_device`, `parse`), `MessageID`, `MessageSource`, `MessageInfo`, `WaEvent` | `types/{jid,message,events}.rs` |
| Protos | `wa_proto::wa_web_protobufs_e2e::Message`, `wa_proto::wa_web_protobufs_web::WebMessageInfo`, `wa_proto::wa_msg_transport`, `wa_proto::wa_msg_application`, `wa_proto::wa_armadillo_application`, `wa_proto::wa_bot_metadata`, `wa_proto::wa_adv::AdvSignedDeviceIdentity` | module name = proto package in snake_case; `engine/target/debug/build/wa-proto-*/out/_includes.rs` lists all |
| Test helpers | `client/tests.rs`: `spawn_server`, `serve`, `next_frame`, `seal`, `open`, `cert_chain`, `ca`, `deterministic_random`, `options`, `unhex32` | Phase 3 moves them to `client/testutil.rs`; reuse, don't duplicate |
| Local DB | `wa_store::Store::{insert_message, insert_messages, search, page}`, `StoredMessage` | **sibling half** owns writing to it |

Dependencies already in `engine/crates/wa-core/Cargo.toml`: `aes-gcm`, `hkdf`, `sha2`,
`curve25519-dalek`, `rand`, `hex`, `prost`, `wa-proto`, `serde`, `specta`, `tokio`, `async-trait`,
`flate2`. New for this half: `aes`, `cbc`, `hmac` (§4.3).

If `struct Client` from Phase 3 is not in the tree yet, **stop** and say so in the report — this half
cannot be built on `do_handshake` alone.

---

## 2. Scope

### 2.1 In scope

1. `crypto/{hkdf,gcm,cbc}.rs` + small extensions to `crypto/keys.rs` (Task A).
2. `crypto/signal/**` — X3DH + Double Ratchet + Sender Keys, wire- and record-compatible with
   WhatsApp (Task B).
3. `crypto/signal/store.rs` — async adapter from the Signal layer onto `Device`/`SqlStore`
   (`IsTrustedIdentity`, `PutIdentity`, sessions, pre-keys, sender keys).
4. `client/prekeys.rs` — `get_server_pre_key_count`, `upload_pre_keys`, pre-key node
   (de)serializers (Task B; closes the `TODO(Phase 4)` Phase 3 left in the connect-success path).
5. `client/message.rs` — the whole incoming path (Task C).
6. `client/ack.rs` — `Nack*` constants, `send_ack`, `maybe_deferred_ack`, `build_base_receipt`.
7. `client/retry_receipt.rs` — **outgoing** retry receipt only (`send_retry_receipt`,
   `message_retries`).
8. `client/web_message.rs` — `ParseWebMessage` (history sync / unavailable-request parsing).
9. `client/fb_message.rs` — armadillo (`FBMessage`) decoding.
10. `types/message.rs` + `types/events.rs` — `MessageInfo` extension and the `Message` /
    `FbMessage` / `UndecryptableMessage` / `IdentityChange` events.
11. `types/jid.rs` — `signal_address()` + `impl specta::Type for Jid`.

### 2.2 Explicitly out of scope (say so in the report, do not "helpfully" implement)

* `send.go`, `mediaretry.go`, `upload.go`, `download.go`, `mediaconn.go`, `download-to-file.go`.
* `receipt.go` (`handleReceipt`, the `Receipt` event, delivered/read/played receipts).
* `retry.go` incoming side: `handleRetryReceipt`, `getMessageForRetry`, `RecentMessages` (512),
  `delayedRequestMessageFromPhone`, `immediateRequestMessageFromPhone`, `cancelDelayedRequestFromPhone`.
* `client/src/app/**` (frontend), all Tauri commands and HTTP routes.
* `wa-store::insert_message` wiring for incoming events.
* `msgsecret.go` **bot** decryption (`encType == "msmsg"`) → `TODO(Phase 5)` + `tracing::warn!`,
  because message secrets are only populated by the history-sync/appstate paths of Phase 5.
* History-sync notifications, app-state key shares, LID-migration sync payloads and placeholder
  resends → `TODO(Phase 5)` log lines inside `handle_protocol_message`.

### 2.3 Boundary contract with the sibling half (must exist when you finish)

```rust
// crypto/signal/mod.rs
pub use address::{SignalAddress, SenderKeyName};
pub use session::{PreKeyBundle, SessionBuilder, SessionCipher, SignalMessage, PreKeySignalMessage};
pub use groups::{GroupCipher, GroupSessionBuilder, SenderKeyDistributionMessage, SenderKeyMessage};
pub use store::{SignalStore, DeviceStore};
pub use error::SignalError;

// client/prekeys.rs
impl Client {
    pub async fn get_server_pre_key_count(&self) -> Result<usize, ClientError>;
    pub async fn upload_pre_keys(&self, initial_upload: bool);
    pub async fn fetch_pre_keys(&self, users: &[Jid]) -> Result<HashMap<Jid, Vec<PreKeyBundle>>, ClientError>;
}
pub fn pre_key_to_node(key: &PreKey) -> Node;
pub fn pre_keys_to_nodes(keys: &[PreKey]) -> Vec<Node>;
pub fn node_to_pre_key(node: &Node) -> Result<PreKey, ClientError>;
pub fn node_to_pre_key_bundle(device_id: u16, node: &Node) -> Result<PreKeyBundle, ClientError>;

// client/ack.rs  (receipt.go:136-149)
pub const NACK_PARSING_ERROR: i32 = 487; // … all variants
impl Client { pub async fn send_ack(&self, node: &Node, error: Option<i32>); }

// client/message.rs
impl Client { pub async fn parse_message_info(&self, node: &Node) -> Result<MessageInfo, ClientError>; }
```

`Client::dispatch_event` is the single event sink (Phase 3); the sibling half, `bebok-server` and the
Tauri emitter subscribe to it. Do not add a second event path.

### 2.4 File ownership

| File | Owner | Status |
|---|---|---|
| `src/crypto/{hkdf,gcm,cbc}.rs`, `src/crypto/mod.rs` | Task A | new |
| `src/crypto/keys.rs` | Task A | extend (keep existing API) |
| `src/crypto/signal/{mod,proto,error,address,ecc,kdf,ratchet,session,groups,store}.rs` | Task B | new |
| `src/client/prekeys.rs` | Task B | new |
| `src/client/message.rs`, `src/client/fb_message.rs`, `src/client/web_message.rs` | Task C | new |
| `src/client/ack.rs`, `src/client/retry_receipt.rs` | Task C | new |
| `src/types/message.rs`, `src/types/events.rs` | Task C | edit (additive only) |
| `src/types/jid.rs` | Task B (`signal_address`) + Task C (`impl Type for Jid`) | edit, additive |
| `src/client.rs` | whoever needs a field/module — minimal edits only | edit |
| `src/lib.rs` | doc comment only | edit |
| `Cargo.toml` (workspace + `wa-core`) | Task A (deps), Task C (`pbjson` if taken) | edit |
| `THIRD_PARTY_NOTICES.md`, `API-WHATSAPP.md` | Task B / Task C | edit |
| `wa-proto/build.rs` | only if `pbjson` is taken (D7/§4.3) | maybe |
| `whatsmeow/**`, `docs/PROMPT-PHASE-*.md` | **nobody** | read-only |

---

## 3. Cross-cutting decisions

### D1 — Do not transliterate `go.mau.fi/libsignal` (GPL-3.0)

`whatsmeow` is MPL-2.0 and `THIRD_PARTY_NOTICES.md` relies on that. **`go.mau.fi/libsignal@v0.2.2`
is GPL-3.0** (`~/go/pkg/mod/go.mau.fi/libsignal@v0.2.2/LICENSE` reads "GNU GENERAL PUBLIC LICENSE
Version 3"). Copying it file-by-file into this MPL-2.0 workspace would relicense `wa-core`.

Therefore Task B is written **from the published Signal specifications** (X3DH, Double Ratchet,
Sender Keys) plus the *interoperability facts* that WhatsApp requires:

* the protobuf **field numbers** of `WhisperTextProtocol.proto` / `LocalStorageProtocol.proto`
  (message formats — needed for on-disk and on-wire compatibility),
* the framing details already written down in `ARCHITECTURE.md`,
* and the Go library **as a test oracle only**: run it to *generate fixtures* (`go run` in a temp
  directory with `GOFLAGS=-mod=mod GOPROXY=off`; the module is already in the local module cache),
  never to copy code.

State this in the report and add a paragraph to `THIRD_PARTY_NOTICES.md`:

> The Signal protocol implementation in `engine/crates/wa-core/src/crypto/signal` is an independent
> implementation of the public Signal specifications (X3DH, Double Ratchet, Sender Keys). Its wire and
> storage formats follow the protobuf definitions published in `libsignal-protocol-c`
> (`WhisperTextProtocol.proto`, `LocalStorageProtocol.proto`) so that it interoperates with WhatsApp.
> No GPL-3.0 source was copied.

If you conclude that a GPL-3.0 dependency is acceptable for this project, **stop and ask** — that is
the maintainer's licensing decision and it affects the whole crate, not just your task.

### D2 — The Signal layer is async, not sync-with-blocking-bridge

Go's stores are sync; ours are `#[async_trait]`. Because the protocol layer is ours, keep it async
throughout instead of calling `block_on`/`block_in_place` inside a Tokio worker:

```rust
#[async_trait]
pub trait SignalStore: Send + Sync {
    fn identity_key_pair(&self) -> &KeyPair;          // Device::identity_key
    fn registration_id(&self) -> u32;                 // Device::registration_id
    async fn load_session(&self, addr: &SignalAddress) -> Result<Option<Vec<u8>>>;
    async fn store_session(&self, addr: &SignalAddress, record: &[u8]) -> Result<()>;
    async fn load_pre_key(&self, id: u32) -> Result<Option<KeyPair>>;
    async fn remove_pre_key(&self, id: u32) -> Result<()>;
    async fn load_sender_key(&self, name: &SenderKeyName) -> Result<Option<Vec<u8>>>;
    async fn store_sender_key(&self, name: &SenderKeyName, record: &[u8]) -> Result<()>;
    async fn is_trusted_identity(&self, addr: &SignalAddress, key: &[u8; 32]) -> Result<bool>;
    async fn save_identity(&self, addr: &SignalAddress, key: &[u8; 32]) -> Result<bool>; // true = replaced
}
```

`DeviceStore { device: Device }` implements it by delegating to the `Arc<dyn …>` store fields on
`Device` (`sessions`, `identities`, `pre_keys`, `sender_keys`) and formatting addresses with
`SignalAddress::to_string()` = `"{user}:{device}"` (Go's `SignalAddress().String()`; `SqlStore`
already keys `wa_sessions` / `wa_identity_keys` / `wa_sender_keys` on exactly that string — see
`MIGRATE_SESSIONS` in `store/sqlite/store.rs`).

`IsTrustedIdentity` semantics are already correct on `SqlStore` (`None` → `true`, otherwise byte
equality with the stored key); the adapter must not re-implement trust, only translate types.
`PutIdentity` = `IdentityStore::put_identity`.

### D3 — Persisted formats must stay compatible with `whatsmeow`'s database

`wa_sessions.session`, `wa_sender_keys.sender_key`, `wa_pre_keys` and `wa_identity_keys.identity`
already store serialized libsignal records. Reproduce them exactly:

* `SessionRecord` → `textsecure.RecordStructure` protobuf bytes,
* `SenderKeyState` → `textsecure.SenderKeyStateStructure`,
* pre-keys → `PreKeyRecordStructure` / `SignedPreKeyRecordStructure`,
* identity keys → raw 32-byte public key (what `SqlStore` already binds as `[u8; 32]`).

Define those structs **by hand** in `crypto/signal/proto.rs` with `#[derive(prost::Message)]`, each
field carrying its tag number and the upstream message it mirrors. Do *not* add the two libsignal
`.proto` files to `wa-proto/build.rs`: both declare `package textsecure`, so prost would emit two
modules with the same name and the build breaks.

### D4 — Padding and the `v` attribute

Recovered plaintexts carry WhatsApp padding for `v < 3`: last byte = pad length, all pad bytes equal,
remainder stripped; `v == 3` skips padding entirely (`message.go:668-694`). Empty plaintext is an
error, never a panic on `plaintext[len - 1]`.

### D5 — No panics on hostile input

Every function that touches network bytes returns `Result`. `prost::decode`, slice indexing, UTF-8 and
integer conversions are all fallible here; a malformed `<message>` node must produce a
`tracing::warn!` plus a `Nack`/`UndecryptableMessage`, never a panic (the Phase 3 handler task must not
die). Go's `recover()` in `decryptMessages` maps to: one bad `<enc>` child must not abort the others.

### D6 — `MessageInfo` grows up (frontend-visible, therefore `specta`-typed)

`types/message.rs` currently carries a reduced `MessageInfo`. Extend it to the shape in
`API-WHATSAPP.md` §3.2 while keeping the existing fields:

```rust
pub struct MessageSource {
    pub chat: Jid, pub sender: Jid, pub sender_alt: Jid, pub recipient_alt: Jid,
    pub is_from_me: bool, pub is_group: bool,
    pub addressing_mode: String,          // "" | "pn" | "lid"
}

pub struct MessageInfo {
    // existing: chat, sender, sender_alt, is_from_me, is_group, timestamp (unix seconds), id
    pub recipient_alt: Jid,
    pub push_name: String,
    pub server_id: i64,
    pub category: String,
    pub edit: String,                     // EditAttribute: "" | "1" | "7" | "8"
    pub addressing_mode: String,
    pub device_sent_meta: Option<DeviceSentMeta>,   // { destination_jid, phash }
    pub msg_meta_info: MsgMetaInfo,                 // { target_id, target_sender,
                                                    //   thread_message_id, thread_message_sender_jid }
    pub is_newsletter_status: bool,
}
```

Derive `specta::Type` on `MessageInfo` / `MessageSource` / `DeviceSentMeta` / `MsgMetaInfo`, and add
`impl specta::Type for Jid` in `types/jid.rs` (delegate to `String`; `Jid` already has a custom
`Serialize`/`Deserialize`). `timestamp` stays `i64` unix seconds — update the `MessageInfo` block in
`API-WHATSAPP.md` §3.2 from `timestamp: string` to `timestamp: number` and justify it in the report
(no `chrono` in the workspace; the UI formats it).

### D7 — Events are additive and keep `serde(tag = "type")`

Add to `WaEvent`, following the existing `#[serde(tag="type", rename_all="snake_case",
rename_all_fields="camelCase")]` conventions:

```rust
Message { info: MessageInfo, message: String, raw: String, is_ephemeral: bool, is_view_once: bool,
          is_view_once_v2: bool, is_view_once_v2_extension: bool, is_document_with_caption: bool,
          is_lottie_sticker: bool, is_bot_invoke: bool, is_edit: bool, retry_count: i32 },
FbMessage { info: MessageInfo, message: String },
UndecryptableMessage { info: MessageInfo, is_unavailable: bool, unavailable_type: String,
                       decrypt_fail_mode: String },
IdentityChange { jid: Jid, timestamp: i64, implicit: bool },
```

`message` / `raw` / `FbMessage::message` are **JSON documents as strings** (the UI does `JSON.parse`),
because `waE2E::Message` is a `prost` type with no `Serialize`. Provide
`client/web_message.rs::message_to_json(&waE2E::Message) -> String` using the `pbjson` impls from §4.3.
If `pbjson` turns out to be more than a day of build plumbing, fall back to a hand-written mirror
struct for the message kinds the UI renders *and* keep `raw` as base64 protobuf bytes — and say so in
the report.

### D8 — Flags live on `Client`, not in globals

Add to `struct Client` (Phase 3): `message_retries: Mutex<HashMap<String, u32>>`,
`auto_trust_identity: bool` (default `false`), `enable_decrypted_event_buffer: bool` (default `true`),
`last_decrypted_buffer_clear: Mutex<Instant>`. Read them from the existing client config; do not invent
a new config file.

### D9 — LID/PN handling on the receive path is not optional

In `decrypt_messages`: when the sender is a PN JID and `sender_alt` is a LID, use the LID for Signal
and call `sessions.migrate_pn_to_lid(pn, lid)` (already on `SessionStore`); otherwise look the LID up
through `Device::get_alt_jid` / `LidStore`. Missing this is the classic "messages silently
undecryptable" bug called out in `AGENTS.md` §8.

### D10 — Licence headers

Every ported file starts with the same two-part header already used in `crypto/keys.rs`:

```rust
// Ported from whatsmeow (`message.go`, `prekeys.go`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.
```

`crypto/signal/**` gets: `// Signal protocol implementation following the public X3DH / Double
Ratchet / Sender Keys specifications — see THIRD_PARTY_NOTICES.md.` (no GPL attribution, see D1).

---

## 4. Task A — crypto utilities (`crypto/`)

### 4.1 Files and signatures

| Go (`whatsmeow/util/…`) | Rust | Signature / notes |
|---|---|---|
| `hkdfutil.SHA256(key, salt, info, length uint8)` | `crypto/hkdf.rs: pub fn sha256(key: &[u8], salt: &[u8], info: &[u8], length: u8) -> Vec<u8>` | HKDF-SHA256 (RFC 5869). Call sites pass a `nil` salt → `Hkdf::<Sha256>::new(None, ikm)` matches Go's zero salt; document it. Length ≤ 255 comes from the signature. |
| `gcmutil.Prepare(secretKey)` | `crypto/gcm.rs: pub fn prepare(key: &[u8]) -> Result<Aes256Gcm, CryptoError>` | 16/24/32-byte keys; error, not panic |
| `gcmutil.Encrypt(key, iv, plaintext, aad)` | `pub fn encrypt(key: &[u8], iv: &[u8], plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, CryptoError>` | `aes-gcm` `Payload { msg, aad }` with the caller's 12-byte IV — never generate the IV here |
| `gcmutil.Decrypt(key, iv, ciphertext, aad)` | `pub fn decrypt(...) -> Result<Vec<u8>, CryptoError>` | tag mismatch → `CryptoError::Aead` |
| `cbcutil.Encrypt(key, iv, plaintext)` | `crypto/cbc.rs: pub fn encrypt(key: &[u8], iv: Option<&[u8]>, plaintext: &[u8]) -> Result<Vec<u8>, CryptoError>` | padding exactly like Go (pad byte = pad length, always a full block); `None` IV → random 16-byte IV **prepended** to the ciphertext |
| `cbcutil.Decrypt(key, iv, ciphertext)` | `pub fn decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, CryptoError>` | reject non-block-multiple and pad > len (Go errors on both) |
| `cbcutil.DecryptFile(key, iv, file)` | `pub fn decrypt_in_place(key: &[u8], iv: &[u8], buf: &mut Vec<u8>) -> Result<(), CryptoError>` | media downloads truncate in place; bytes in, no `std::fs` in this crate |
| `cbcutil.EncryptStream(key, iv, macKey, reader, writer)` | `pub fn encrypt_stream(key: &[u8], mac_key: &[u8], plaintext: &[u8]) -> Result<StreamEncrypted, CryptoError>` with `StreamEncrypted { ciphertext, iv, mac, plaintext_len, ciphertext_len }` | used by media upload (sibling half); port now because the HMAC plumbing is Task A's |

New error enum `crypto::CryptoError` (`#[derive(Debug, thiserror::Error)]`) with `Aead`,
`InvalidKeyLength`, `InvalidIvLength`, `InvalidBlockLength`, `BadPadding`, `HkdfExpand`.

### 4.2 Extensions to `crypto/keys.rs`

Keep everything that exists. Add only:

* `impl KeyPair { pub fn public_serialized(&self) -> [u8; 33] }` (the `djb_public` shortcut — used
  everywhere in Signal),
* `pub struct IdentityKey(pub [u8; 32])` with `serialize() -> [u8; 33]`,
  `from_bytes(&[u8]) -> Result<Self>`, `from_pair(&KeyPair)`,
  `verify(&self, msg: &[u8], sig: &[u8; 64]) -> bool`,
* `impl PreKey { pub fn to_record(&self) -> Vec<u8>; pub fn from_record(bytes: &[u8]) -> Result<Self>; }`
  (`PreKeyRecordStructure`, see D3),
* `pub fn pad_message(plaintext: &[u8]) -> Vec<u8>` and
  `pub fn unpad_message(plaintext: &[u8], version: i32) -> Result<Vec<u8>, CryptoError>` (D4).

### 4.3 Cargo additions (`engine/crates/wa-core/Cargo.toml` + `engine/Cargo.toml`)

```toml
aes   = "0.8"   # CBC block cipher (already pulled in transitively by aes-gcm; declare it)
cbc   = "0.1"
hmac  = "0.12"
```

Only if D7's `pbjson` path is taken: `pbjson = "0.7"` + `pbjson-types = "0.7"` for `wa-core`, and
`pbjson-build = "0.7"` as a build-dependency of `wa-proto` (needs `Config::file_descriptor_set_path()`
in `build.rs` plus `pbjson_build::Builder::register_descriptors`). Check with `cargo tree -i aes` that
no second `aes` major appears; align instead of duplicating.

### 4.4 Required tests (Task A, all offline)

`#[cfg(test)]` in each module:

* `hkdf`: RFC 5869 test case 1; a Go-generated vector (`hkdfutil.SHA256`); the `salt = nil` case.
* `gcm`: an AES-256-GCM known-answer vector (12-byte IV, non-empty AAD); a Go-generated
  (`gcmutil.Encrypt`) vector; tampered-ciphertext and tampered-AAD rejection tests.
* `cbc`: a Go-generated `Encrypt` vector with a fixed IV (hex equality); `Encrypt(key, None, ..)`
  round-trip plus IV-prefix length check; `Decrypt` of a wrong pad → `BadPadding`; non-block-multiple
  → `InvalidBlockLength`; `encrypt_stream` compared against `cbcutil.EncryptStream` output
  (ciphertext, IV, MAC and both lengths).
* `keys`: `IdentityKey::serialize()` is `0x05 || pubkey`; `PreKey::to_record`/`from_record`
  round-trip; `unpad_message` rejects `pad > len`; `pad_message` → `unpad_message` round-trip;
  `unpad_message(v = 3)` returns its input untouched.

Vectors are produced by a throw-away `go run` program under `$TMPDIR` (never committed, never in the
repo) and pasted into the test as hex constants with a comment naming the Go function and arguments.

---

## 5. Task B — Signal layer + store adapter + pre-keys

### 5.1 Module layout (`engine/crates/wa-core/src/crypto/signal/`)

| File | Contents | Reference |
|---|---|---|
| `mod.rs` | re-exports (§2.3), module docs, the "independent implementation" notice | — |
| `proto.rs` | hand-written `prost` structs for `WhisperTextProtocol` (SignalMessage, PreKeySignalMessage, SenderKeyMessage, SenderKeyDistributionMessage) and `LocalStorageProtocol` (RecordStructure, SessionStructure, Chain/ChainKey/MessageKey, PendingPreKey, PendingKeyExchange, PreKeyRecordStructure, SignedPreKeyRecordStructure, IdentityKeyPairStructure, SenderKeyStateStructure) | field numbers from the public `libsignal-protocol-c` `.proto` (D1/D3) |
| `error.rs` | `SignalError { UntrustedIdentity, OldCounter, DuplicateMessage, InvalidMessage, InvalidKey, BadMac, NoSenderKeyForUser, NoSignedPreKey, NoPreKey, SessionNotFound }` + helpers `is_untrusted_identity()`, `is_old_counter()`, `is_no_sender_key_for_user()` used by the retry path | `signalerror/*.go` semantics |
| `address.rs` | `SignalAddress { name: String, device: u32 }` with `Display = "name:device"` and `From<&Jid>` (via `Jid::signal_address_user()`); `SenderKeyName { group_id: String, sender: SignalAddress }` with the serialized form used as the DB key | `types/jid.go:108`, `protocol/SenderKeyName.go` |
| `ecc.rs` | wrappers over `crypto::keys`: `calculate_agreement(their_pub: &[u8; 32], our_priv: &[u8; 32]) -> [u8; 32]` (X25519 via `curve25519-dalek`), `verify_djb(public: &[u8], msg: &[u8], sig: &[u8; 64]) -> bool` | `ecc/*`, `kdf.CalculateSharedSecret` |
| `kdf.rs` | `derive_secrets(ikm, salt, info, len)`, `root_and_chain_keys(...) -> (RootKey, ChainKey)`, `chain_key_message_keys(chain_key) -> MessageKeys { cipher_key, mac_key, iv }` | `kdf/HKDF.go`, `keys/{root,chain,message}` |
| `ratchet.rs` | `calculate_sender_session`, `calculate_receiver_session`, `calculate_symmetric_session`, `gen_discontinuity() -> [0xFF; 32]`, `is_sender(our_key, their_key)` | `ratchet/Ratchet.go` |
| `session.rs` | `SessionRecord`/`SessionState`/`Chain` (serialize ↔ `proto::RecordStructure`), `SessionBuilder::{process, process_bundle}`, `SessionCipher::{encrypt, decrypt, decrypt_message, decrypt_with_record, decrypt_with_state}`, `SignalMessage`, `PreKeySignalMessage`, `PreKeyBundle` | `session/{Session,SessionCipher}.go`, `state/record/*` |
| `groups.rs` | `SenderKeyState` (serialize ↔ `proto::SenderKeyStateStructure`), `GroupSessionBuilder::{process, create}`, `GroupCipher::{encrypt, decrypt}`, `SenderKeyMessage`, `SenderKeyDistributionMessage` | `groups/*`, `state/record/SenderKeyState.go` |
| `store.rs` | `SignalStore` trait (D2) + `DeviceStore` adapter + `SessionlessSignalStore` (a `DeviceStore` whose `store_session` is a no-op; Go `sessionlessSignalStore`, `message.go:580`) | `state/store/*.go` |

Message-key derivation, root/chain derivation and the MAC over
`senderIdentity ‖ receiverIdentity ‖ serializedMessage` must match WhatsApp byte for byte — test each
against a Go-generated vector *before* wiring the session state machine.

### 5.2 Session semantics to preserve

* `SessionBuilder::process_bundle` (X3DH): ephemeral base key, three or four DH operations, signed
  pre-key signature verification with `verify_djb`, `remote_registration_id` recorded,
  `PendingPreKey { pre_key_id, signed_pre_key_id, base_key }` stored for the first ciphertext.
* `SessionBuilder::process` (receiving a `PreKeySignalMessage`): no existing session → X3DH as Bob,
  store the session, remove the one-time pre-key (`remove_pre_key`), skip the session store entirely
  when the node carries `state="false"` (`SessionlessSignalStore`).
* `SessionCipher::decrypt` (normal `SignalMessage`): if the message's ratchet key differs from the
  current chain, derive a new receiver chain from the root key; keep skipped message keys
  (`Chain::message_keys`, capped like Go) for out-of-order delivery; unknown counter →
  `SignalError::OldCounter`; duplicate → `SignalError::DuplicateMessage`.
* Identity handling: before storing a session, `is_trusted_identity`; on `false` and
  `auto_trust_identity` set, `delete_identity` + `delete_session` + `WaEvent::IdentityChange
  { implicit: true }` and retry once (`clearUntrustedIdentity`, `message.go:500`); otherwise surface
  `SignalError::UntrustedIdentity`.
* Group side (`groups.rs`): sender key state per `SenderKeyName`; `GroupCipher::decrypt` verifies the
  message signature with the stored signing key and caches up to 2000 sender message keys; missing
  state → `SignalError::NoSenderKeyForUser` (drives `is_unavailable` in Task C).

### 5.3 Pre-key upload (`client/prekeys.rs`, port of `prekeys.go`)

| Go | Rust |
|---|---|
| `WantedPreKeyCount = 50`, `MinPreKeyCount = 5` | `pub const WANTED_PRE_KEY_COUNT: usize = 50; pub const MIN_PRE_KEY_COUNT: usize = 5;` |
| `getServerPreKeyCount` | `Client::get_server_pre_key_count` — `<iq xmlns="encrypt" type="get" to="s.whatsapp.net"><count/>` through `send_iq`, reads `count@value` |
| `uploadPreKeys(initialUpload)` | `Client::upload_pre_keys(initial_upload: bool)` — 10-minute race guard against `last_pre_key_upload`, `812` keys on the first upload, `registration` (big-endian `registration_id`), `type = DJB_TYPE`, `identity`, `list` of pre-keys, `skey` (signed pre-key), `_`/`v` attributes exactly as in Go; then `mark_pre_keys_as_uploaded` |
| `preKeyToNode`, `preKeysToNodes`, `nodeToPreKey`, `nodeToPreKeyBundle` | `pre_key_to_node`, `pre_keys_to_nodes`, `node_to_pre_key`, `node_to_pre_key_bundle` (§2.3) |
| `fetchPreKeys` / `fetchPreKeysNoError` | `Client::fetch_pre_keys` — same `<iq>` shape, per-device bundles keyed by JID |

Wire it up where Phase 3 left `TODO(Phase 4)`: after connect success / pairing, if
`uploaded_pre_key_count() < MIN_PRE_KEY_COUNT`, spawn `upload_pre_keys(false)`; also call
`upload_pre_keys(true)` when the server reports zero keys during pairing.

### 5.4 Required tests (Task B, all offline)

* `proto.rs`: each record round-trips and its encoded bytes equal a Go-generated fixture
  (`RecordStructure`, `SenderKeyStateStructure`, `PreKeyRecordStructure`,
  `SignedPreKeyRecordStructure`) — this proves DB compatibility (D3).
* `kdf.rs`/`ecc.rs`: `derive_secrets` and `calculate_agreement` against Go-generated vectors;
  `chain_key_message_keys` against a Go-generated `chain.Key` → `message.Keys` triple.
* `session.rs`:
  * round-trip Alice→Bob 20 messages each way, alternating, asserting exact plaintexts;
  * out-of-order delivery (receive #3 after #5) and a replayed message → `DuplicateMessage`;
  * **cross-implementation fixture**: a Go run writes Bob's session record, a pre-key bundle and one
    `PreKeySignalMessage`; the Rust test loads those bytes, decrypts, and asserts the plaintext.
    Store the fixture as `hex` constants inside the test, not as binary files;
  * Rust→Go direction as `#[ignore]`d test that shells out to the Go oracle when it is available;
  * `is_trusted_identity == false` → `UntrustedIdentity`, and the `auto_trust_identity` retry path
    emits one `IdentityChange { implicit: true }` and succeeds on the second attempt.
* `groups.rs`: Go-generated `SenderKeyDistributionMessage` + two `SenderKeyMessage`s decrypt in Rust;
  a tampered signature → `BadMac`; unknown sender key → `NoSenderKeyForUser`.
* `store.rs`: a `Device` backed by `Container::open_memory()` — `save_identity`/`is_trusted_identity`
  round-trip, `load_session`/`store_session` round-trip, `remove_pre_key` actually deletes the row,
  `SessionlessSignalStore::store_session` writes nothing.
* `client/prekeys.rs`: `pre_keys_to_nodes`/`node_to_pre_key` round-trip and hex equality of the
  encoded `<key>` node against `preKeyToNode` output; `upload_pre_keys` exercised against a synthetic
  server (§7.1) that answers the `encrypt` IQ, asserting the `<list>` count (`50` vs `812`).

---

## 6. Task C — incoming message handling (`client/message.rs` and friends)

Port target: `whatsmeow/message.go` (lines 44–710 receive path, 884–1199 protocol/dispatch),
`whatsmeow/armadillomessage.go`, plus `ParseWebMessage` (`client.go:1027`) and `UnwrapRaw`
(`types/events/events.go:376`).

### 6.1 Go → Rust function table

| Go | Rust | Notes |
|---|---|---|
| `handleEncryptedMessage` (44) | `Client::handle_encrypted_message(&self, node: &Node)` | parse info → `NackParsingError` on failure; then `decrypt_messages` |
| `handleUnencryptedMessage` (71) | `Client::handle_unencrypted_message(&self, node: &Node)` | `status@broadcast` and friends; dispatch `Message` without Signal |
| `parseMessageSource` (88) | `Client::parse_message_source(&self, node: &Node, require_participant: bool) -> Result<MessageSource, ClientError>` | `from`/`participant`/`recipient` + `sender_alt`/`recipient_alt` from `sender_pn`/`sender_lid`/`recipient_pn`/`recipient_lid`, `addressing_mode` from `addressing_mode` |
| `parseMessageInfo` (224) | `Client::parse_message_info` | `id`, `server_id`, `t` (`unix_time`), `notify` → `push_name`, `category`, `type`, `edit`; children `multicast`, `verified_name` (parse + log), `bot`, `meta` |
| `parseMsgMetaInfo`, `parseMsgBotInfo`, `parseVerifiedNameContent` | same names, `-> Result<_, ClientError>` | `wa_proto::wa_bot_metadata` for bot info; verified-name failures only warn (Go logs and continues) |
| `handlePlaintextMessage` (276) | `Client::handle_plaintext_message` | `<plaintext>` child → `waE2E::Message` → dispatch `Message`; `<meta>` → `NewsletterMessageMeta` is `TODO(Phase 5)` |
| `decryptMessages` (317) | `Client::decrypt_messages(&self, node: &Node)` | per-`<enc>` loop, `Nack`s, `RecentMessages` hook is the sibling half |
| `decryptDM` (586) | `Client::decrypt_dm(&self, node: &Node, info: &MessageInfo, enc: &Node) -> Result<Decrypted, SignalError>` | `type` attribute switch: `pkmsg` → `SessionBuilder::process` + `SessionCipher::decrypt_message`; `msg` → `SessionCipher::decrypt`; `msmsg` → `TODO(Phase 5)` (§2.2) |
| `decryptGroupMsg` (640) | `Client::decrypt_group_msg(&self, node, info) -> Result<Decrypted, SignalError>` | `skmsg` → `GroupCipher::decrypt`; `pkmsg`/`msg` in a group → sender-key bootstrap, then re-store the group session |
| `unpadMessage`/`padMessage`, `isValidPadding` | `crypto::keys::{unpad_message, pad_message}` (Task A) | — |
| `clearUntrustedIdentity` (500) | `Client::clear_untrusted_identity(&self, addr, info, is_group) -> bool` | deletes identity + session, emits `IdentityChange { implicit: true }` |
| `bufferedDecrypt` (518) | `Client::buffered_decrypt(&self, info, enc, decrypt: BufferedDecryptFn) -> Result<Vec<u8>, SignalError>` | `EventBuffer::do_decryption_txn`; the closure is `async move { … }` boxed as the `TxnFuture` the trait already expects; clears stale plaintext older than 10 minutes (`last_decrypted_buffer_clear`) |
| `getDecryptionFailureRetryCount` helpers | inline | `count` attribute of `<enc>` → `retry_count` |
| `handleDecryptedMessage` (1172) | `Client::handle_decrypted_message(&self, info, msg, retry_count)` | `process_protocol_parts` then dispatch `Message` (with `UnwrapRaw` applied) |
| `processProtocolParts` (938) | `Client::process_protocol_parts(&self, info, msg) -> bool` | `store_message_secret` (via `MsgSecretStore::put_message_secret`), `device_sent` unwrap, sender-key distribution handling, then `handle_protocol_message` |
| `handleSenderKeyDistributionMessage` (693) | `Client::handle_sender_key_distribution_message(&self, chat: &Jid, sender: &Jid, skdm: &[u8])` | `GroupSessionBuilder::process` with `SenderKeyName`; **no** error escape — log and continue like Go |
| `handleProtocolMessage` (884) | `Client::handle_protocol_message(&self, info, msg) -> bool` | only the `IsFromMe` early return and `TODO(Phase 5)` log lines for history-sync / app-state / placeholder resends; the peer-msg receipt is the sibling half |
| `storeMessageSecret` (966) | `Client::store_message_secret(&self, info, msg)` | `MessageContextInfo::message_secret` → `MsgSecretStore`; `RootSecretDistributeMessage` chat override |
| `ParseWebMessage` (client.go:1027) | `Client::parse_web_message(&self, chat: Jid, web_msg: &WebMessageInfo) -> Result<Message>` in `client/web_message.rs` | chat/sender resolution incl. `original_self_author_user_jid_string`, group `broadcast`/newsletter handling, comment metadata → `msg_meta_info.thread_*`, then `UnwrapRaw`; a `MESSAGE_EDIT` protocol message replaces `id` + message |
| `events.Message.UnwrapRaw` (events.go:376) | `impl Message { pub fn unwrap_raw(&mut self) }` in `types/message.rs` | order matters and is part of the protocol: DeviceSent → BotInvoke → Ephemeral → ViewOnce → ViewOnceV2 → ViewOnceV2Extension → LottieSticker → DocumentWithCaption → Edited; then `message_context_info` back-fill from the raw message |
| `handleDecryptedArmadillo` + `decodeArmadillo` + `decodeFBApplication`/`decodeIGArmadillo` (`armadillomessage.go`) | `client/fb_message.rs`: `Client::handle_decrypted_armadillo`, `decode_armadillo`, `decode_fb_application`, `decode_ig_armadillo` | transport `ancillary.skdm` → `handle_sender_key_distribution_message`; dispatch `WaEvent::FbMessage` only when a payload decoded; unsupported version → debug log |
| `sendAck`, `maybeDeferredAck`, `buildBaseReceipt`, `Nack*` (`receipt.go`) | `client/ack.rs` | `class`/`id`/`to`/`participant`/`recipient` (+ bot-JID hack), `type` for non-`message` tags, `error` on nack; `maybe_deferred_ack` unchanged (SynchronousAck = false default) |
| `sendRetryReceipt` (`retry.go:472`) | `client/retry_receipt.rs` | outgoing only: `message_retries` counter, `>= 5` stops, `<retry count id t v=1>`, `registration`; on `retryCount > 1` or `force_include_identity` also `keys` (`type`, `identity`, one pre-key, `signed_pre_key`, `device-identity` from `Device::account`). `delayedRequestMessageFromPhone` → `TODO(send half)` |
| `SendProtocolMessageReceipt` (1190) | `Client::send_protocol_message_receipt(&self, id, receipt_type)` | tiny; needed by `handle_protocol_message` |

### 6.2 Behaviour notes that must not be lost

* `NackParsingError = 487` when the `<message>` attributes do not parse; `NackUnrecognizedStanza = 488`
  when a `<enc>` child has an unknown `type`; `NackInvalidProtobuf = 491` when `prost::decode` fails;
  `NackMissingMessageSecret = 495`. Full constant list: `receipt.go:136-149`.
* Undecryptable messages: when no `<enc>` child exists at all the event is `is_unavailable = true` and
  `unavailable_type` is taken from the sibling `<unavailable>` child (`waE2E::UnavailableMessage`
  reason); otherwise `is_unavailable = false` and `decrypt_fail_mode` says why
  (`SignalError::UntrustedIdentity` → `"identity"`, `OldCounter`/`NoSenderKeyForUser` →
  `"sender_key"`, anything else → `"decrypt"`).
* A decryption failure must also send the retry receipt (`send_retry_receipt`) unless
  `retry_count >= 5`, and must **not** ack the node.
* Successful decryption acks the node (`send_ack(node, None)`) after dispatch, exactly like Go's
  `defer` ordering (Go acks in `handleEncryptedMessage` via `maybeDeferredAck` before dispatch —
  preserve the observable order: parse → ack → decrypt → dispatch).
* `Device::account` may be `None` (device not yet paired fully); the retry receipt then omits
  `device-identity` with a warning instead of failing.
* Never log keys, plaintext bodies or `adv_secret_key`. Log `info.id`, `info.chat`, `info.sender` only.

### 6.3 Required tests (Task C, all offline)

Everything below runs without network — see §7.

* `parse_message_info`: hex fixtures of `<message>` nodes taken from real traffic shapes
  (DM, group with `participant`, LID-addressed with `sender_pn`/`sender_lid`, newsletter, `status@`
  with `<plaintext>`): asserts every `MessageInfo` field, `addressing_mode`, and `sender_alt`.
* `parse_message_source`: `require_participant = true` without a participant → error; `from` with
  device (`user:2@s.whatsapp.net`) keeps the device.
* `unwrap_raw` (pure unit tests, no crypto): one fixture per wrapper (DeviceSent, Ephemeral, ViewOnce,
  ViewOnceV2, ViewOnceV2Extension, LottieSticker, DocumentWithCaption, Edited, BotInvoke, nested
  DeviceSent→Ephemeral) asserting the resulting inner message, each `is_*` flag, and
  `device_sent_meta`.
* `parse_web_message`: `WebMessageInfo` built in Rust → `Message`; `is_from_me` with and without
  `original_self_author_user_jid_string`; `MESSAGE_EDIT` id replacement; `comment_metadata`
  thread ids.
* `decrypt_messages` end-to-end with a real session: Rust Alice encrypts (Task B) → the resulting
  `<enc type="msg" v="2">` node is fed to `handle_encrypted_message` → assert one `Message` event on
  the broadcast channel with the expected text, plus that an `<ack>` was written to the socket.
* Group path: `SenderKeyDistributionMessage` + `skmsg` node → `Message` event; second message after
  the state is stored.
* Failure paths: tampered ciphertext → `UndecryptableMessage` with `decrypt_fail_mode = "decrypt"`
  **and** a `<receipt type="retry">` sent (assert its XML); untrusted identity with
  `auto_trust_identity = false` → `UndecryptableMessage` (`"identity"`); unknown `type` attribute →
  `<ack error="488">`; garbage attribute → `<ack error="487">`; missing `<enc>` altogether →
  `is_unavailable = true`.
* Retry receipt: 1st..5th failure produces `<retry count="1">` … `<retry count="4">` and no sixth
  (assert the counter map), and count `>= 2` includes the `<keys>` child with `identity`, `skey`,
  `device-identity`.
* `fb_message`: a Go/Rust-built `waMsgTransport::MessageTransport` protobuf with a consumer-application
  payload → `FbMessage` event; an SKDM inside `ancillary.skdm` → sender key stored; unsupported
  `applicationPayload.version` → no event, one warning, no panic.
* Flood guard: a `<message>` node with 3 `<enc>` children where the middle one is garbage must still
  decrypt the other two (Go's per-child `recover()` equivalent).

---

## 7. Offline test strategy (mandatory — no network, no wall-clock waits)

### 7.1 Synthetic server

Reuse the Phase 2/3 helper (`client/tests.rs::spawn_server`, relocated to `client/testutil.rs`):

```rust
let (url, handle) = spawn_server(ca(), |ws| async move { /* scripted frames */ }).await;
```

* bind `127.0.0.1:0` and read the actual port from the listener (`local_addr()`), never a fixed port;
* every `await` inside a test is wrapped in `tokio::time::timeout(Duration::from_secs(5), …)`, so a
  hung test fails fast instead of blocking CI;
* for Task C tests, the "server" only has to complete the Noise handshake and then push/pre-consume
  `<message>` frames; assert what the client writes back by decoding with `binary::unmarshal`;
* no `127.0.0.1` socket is opened if the test only needs `handle_encrypted_message`: build the
  `<message>` node in memory, hand it to the handler with a `Client` whose socket is a
  `tokio::io::duplex()` pair (or the test double Phase 3 introduced) and assert on the collected
  events plus the bytes written to the duplex end.
* `Container::open_memory()` + `tempfile::tempdir()` for every store-backed test; never touch
  `~/.local/share/ngwhatsapp`.
* fixtures live as `hex`/`&str` constants inside the test module — no binary files in the repo, no
  `include_bytes!` of generated data.

### 7.2 Explicitly forbidden in tests

`reqwest`/`hyper` calls, live WhatsApp connections, `tokio::time::sleep` longer than 50 ms, real
network sockets other than `127.0.0.1:0`, and assertions on vector/KNN search emptiness (see
`AGENTS.md` §5).

---

## 8. Gates — do not report PASS unless all of these pass

1. `cd engine && cargo test -p wa-core` — green, no `--ignored` needed (the Go oracle test is ignored).
2. `cd engine && cargo test` — the whole workspace still green (`wa-store`, `bebok-server` untouched
   but must compile).
3. `cargo fmt --all -- --check` and `cargo clippy --all-targets -- -D warnings`.
4. `cd engine && cargo build` — the `wa-proto` build script still generates every module (D3).
5. `cd client && npm run specta` (if D6/D7 changed `WaEvent`) and `cd client && npm run build` —
   the generated bindings must still type-check.
6. No new `unwrap()`/`expect()`/`panic!()` in non-test code that is reachable from a network path
   (`grep -n "unwrap()\|expect(" engine/crates/wa-core/src/client/message.rs` → only provably
   infallible cases, each with a comment).
7. `git status` shows no changes under `whatsmeow/`.

## 9. Rules

* **Do not modify `whatsmeow/`** and do not add files there — it is the reference checkout.
* Do not touch `docs/PROMPT-PHASE-3.md` or `docs/PROMPT-PHASE-4.md`; this file is your brief.
* Stay inside the files listed in §2.1. If a change is needed elsewhere (e.g. `client.rs` wiring),
  make the minimal edit and list it in the report.
* Keep the public API stable for the sibling half (§2.3) — if you must change a signature, say so
  loudly in the report.
* English only, no i18n, no new user-facing strings.
* Never implement GPL-3.0 code by transliteration (D1).
* Prefer `tracing::{debug, info, warn}` over `println!`; stdout belongs to `BEBOK_READY` only.
* Every new `pub` item gets a doc comment naming the Go counterpart (e.g. `/// Go: decryptGroupMsg`).

## 10. Report template

```
Status: PASS | PASS WITH NOTES | FAIL

Files written:
  engine/crates/wa-core/src/crypto/{mod,hkdf,gcm,cbc,keys}.rs
  engine/crates/wa-core/src/crypto/signal/{mod,proto,error,address,ecc,kdf,ratchet,session,groups,store}.rs
  engine/crates/wa-core/src/client/{message,ack,retry_receipt,prekeys,web_message,fb_message}.rs
  engine/crates/wa-core/src/types/{message,events,jid}.rs (edited)
  engine/crates/wa-core/src/client.rs (module wiring only)
  engine/crates/wa-core/Cargo.toml, engine/Cargo.toml (new deps)
  THIRD_PARTY_NOTICES.md, API-WHATSAPP.md (timestamp + new events)

Verification (commands actually run + result):
  cargo test -p wa-core            → N passed, 0 failed
  cargo test                       → N passed, 0 failed
  cargo clippy --all-targets       → clean
  cargo fmt --check                → clean
  npm run specta && npm run build  → clean / not needed (say which)

Not done / deferred (with the TODO markers you left):
  bot msmsg decryption, history sync, appstate key share, placeholder resend, RecentMessages …

Notes for the sibling half:
  exact signatures that changed, and which of §2.3 is ready to use
```

## 11. Honest-failure clause

Signal is the hardest part of this project. If `session.rs`/`groups.rs` is not green after a
reasonable effort, **stop and report FAIL with what works** (Task A and the store adapter are
independently useful) instead of leaving half-working crypto wired into the receive path. A
non-decrypting `decrypt_messages` must be `TODO`-marked and non-panicking, never silently wrong.

---

# Part 2 — sending half (Tasks D–G)

## Context (read first)

```
README.md → ARCHITECTURE.md → API-WHATSAPP.md → AGENTS.md → TODO.md (Phase 4)
docs/PROMPT-PHASE-3.md (Client struct, node loop, pairing, keepalive — done)
the backend half below (Tasks A–C: Signal, pre-keys, incoming path)
```

Current state (Phases 0–3 done, Tasks A–C of Phase 4 assumed done):

- `wa-core/src/client.rs` + `client/*.rs` — `struct Client` with `send_node`, `send_iq`,
  `response_waiters`, `handler_queue_loop`, `dispatch_event` (the single event sink), `Device`
  behind `Mutex`, `is_connected`/`is_logged_in`.
- `crypto/signal/` — `SessionBuilder`/`SessionCipher`/`GroupCipher`, `SignalStore` adapter,
  `PreKeyBundle` (Task B). `client/prekeys.rs` — `get_server_pre_key_count`, `upload_pre_keys`,
  `fetch_pre_keys`, pre-key node (de)serializers (Task B).
- `client/message.rs` — `parse_message_info`, incoming decrypt path, `Message`/`FbMessage`/
  `UndecryptableMessage`/`IdentityChange` events (Task C). `client/ack.rs` — `NACK_*` constants,
  `send_ack`, `maybe_deferred_ack` (Task C).
- `bebok-server/src/routes/mod.rs` — routing table; `/search`, `/chats`, `/chats/:jid/history/export`,
  `/chats/:jid/stats`, `/debug/seed` exist. No send/message routes yet.
- `wa-store` — `insert_message`/`insert_messages`/`search`/`page`, `StoredMessage` (search + export
  from day 0). Nothing writes incoming live messages into it yet.
- `client/src/app/core/services/whatsapp.service.ts` — `connectionState`, `qrCodes`, `handleEvent`,
  `connect`, `search`, `exportHistory`. No message state, no send methods.
- `client/src/app/core/services/tauri.service.ts` — `COMMAND_ROUTES` for
  `connect|disconnect|logout|is_connected|is_logged_in|version|…`; `WA_EVENT` via SSE.

## Boundary contract (provided by Tasks A–C — reuse, do not rewrite)

```rust
// crypto/signal
SessionBuilder::build_session(&store, &PreKeyBundle) -> Result<(), SignalError>
SessionCipher::{encrypt(&mut store, plaintext) -> SignalMessage, decrypt(&mut store, &msg)}
GroupCipher::{encrypt(&mut store, &SenderKeyName, plaintext), decrypt(...)}
// client/prekeys.rs
Client::{get_server_pre_key_count, upload_pre_keys, fetch_pre_keys(&[Jid]) -> HashMap<Jid, Vec<PreKeyBundle>>}
// client/message.rs — Client::parse_message_info(&Node) -> Result<MessageInfo, ClientError>
// client/ack.rs — send_ack(&Node, Option<i32>), NACK_* constants
// types/message.rs — MessageInfo (with Phase-4 extensions), MessageID, MessageSource
```

`Client::dispatch_event` stays the single event sink. `bebok-server` and the Tauri emitter subscribe
to it; do not add a second event path.

---

## 1. Scope

### 1.1 In scope

1. `client/send.rs` — `SendMessage`, `SendPeerMessage`, `RevokeMessage`, builders
   (`BuildMessageKey`, `BuildRevoke`, `BuildReaction`, `BuildUnavailableMessageRequest`,
   `BuildHistorySyncRequest`, `BuildEdit`, `SetDisappearingTimer`), group/newsletter/peer/DM
   dispatch, `prepareMessageNode`, `encryptMessageForDevices` (Task D).
2. `client/retry.rs` — `addRecentMessage`, `getRecentMessage`, `getMessageForRetry`,
   `tryHandleRetryReceipt`/`handleRetryReceipt`, `sendRetryReceipt`, delayed/immediate phone
   requests, `RecentMessages` ring buffer 512 (Task D).
3. `client/receipt.rs` — `handleReceipt`, `handleGroupedReceipt`, `parseReceipt`,
   `maybe_deferred_ack` wiring, `Receipt` event, `MessageIDs` ordering (Task D).
4. `client/mediaretry.rs` — `SendMediaRetryReceipt`, `handleMediaRetryNotification` (Task D).
5. `client/upload.rs` / `client/download.rs` / `client/mediaconn.rs` — `Upload`/`UploadReader`/
   `UploadNewsletter`, `rawUpload`, `DeleteMedia`, `DownloadAny`/`Download`/`DownloadThumbnail`/
   `FetchStickerPack`/`DownloadFB`, `refreshMediaConn`/`queryMediaConn`, `download-to-file.go`
   port (Task E).
6. `bebok-server` routes + Tauri commands: `send_message`, `send_media`, `fetch_history`,
   `get_message`, `mark_read`, `mark_played`, `upload_media`, `download_media` (Task F).
7. `wa-store::insert_message` wiring for every incoming *and* sent message (Task F).
8. Angular `features/messages/`: `message-view` (virtual scroll), `composer` (send, retry),
   `media-preview` + specs (Task G).

### 1.2 Explicitly out of scope (say so in the report, do not "helpfully" implement)

* `msgsecret.go` **bot** decryption (`encType == "msmsg"`) → `TODO(Phase 5)` + `tracing::warn!`.
* History-sync notifications, app-state key shares, LID-migration sync payloads → `TODO(Phase 5)`.
* `group.go`, `newsletter.go`, `presence.go`, `privacysettings.go`, `appstate/` → Phase 5.
* Playwright E2E → Phase 6 (unit + Karma specs only here).

### 1.3 File ownership

| File | Owner | Status |
|---|---|---|
| `src/client/send.rs` | Task D | new |
| `src/client/retry.rs` | Task D | new |
| `src/client/receipt.rs` | Task D | new |
| `src/client/mediaretry.rs` | Task D | new |
| `src/client/upload.rs`, `src/client/download.rs`, `src/client/mediaconn.rs` | Task E | new |
| `src/types/receipt.rs` (or extend `types/message.rs`) | Task D | new/edit, additive |
| `src/types/events.rs` | Task D (`Receipt`, `RetryReceipt` outcomes) + Task E (media events) | edit, additive |
| `src/client.rs` | whoever needs a field/module — minimal edits only | edit |
| `bebok-server/src/routes/{mod,messages,media}.rs`, `state.rs` | Task F | edit/new |
| `client/src-tauri/src/**` (Tauri commands) | Task F | edit |
| `client/src/app/features/messages/**` | Task G | new |
| `client/src/app/core/services/{whatsapp,tauri}.service.ts`, `app.routes.ts` | Task G | edit |
| `API-WHATSAPP.md` §§3–4, `THIRD_PARTY_NOTICES.md` | Task F | edit |
| `whatsmeow/**`, `docs/PROMPT-PHASE-*.md` | **nobody** | read-only |

---

## 2. Task D — send / retry / receipts (`client/send.rs`, `retry.rs`, `receipt.rs`, `mediaretry.rs`)

Port targets: `whatsmeow/send.go`, `retry.go`, `receipt.go`, `mediaretry.go`.

### 2.1 Go → Rust function table (send)

| Go (`send.go`) | Rust | Notes |
|---|---|---|
| `GenerateMessageID` | `Client::generate_message_id() -> MessageID` | random, same shape as Go (`<hex>-…`) |
| `SendMessage(to, message, extra...)` | `Client::send_message(&self, to: &Jid, message: &Message, extra: SendRequestExtra) -> Result<SendResponse, ClientError>` | the main entry; `extra` mirrors Go's `SendRequestExtra` (peer, status, edit, …) field for field |
| `sendDM` / `sendGroup` / `sendPeerMessage` / `sendNewsletter` | `Client::{send_dm, send_group, send_peer_message, send_newsletter}` | dispatch on chat server + `extra`; DM = the Signal-encrypt path via Task-B `SessionCipher` |
| `prepareMessageNode` | `Client::prepare_message_node(&self, chat, sender, id, message, extra) -> Result<Node, ClientError>` | builds the `<message>` node incl. `phash`, `edit`, `meta` children |
| `encryptMessageForDevices` / `…AndWrap` / `…ForDevice` | `Client::{encrypt_message_for_devices, encrypt_message_for_device_and_wrap, encrypt_message_for_device}` | fan-out over `fetch_pre_keys` (Task B) + `SessionCipher::encrypt`; failures per device must not abort the whole send — collect and continue like Go |
| `getMessageContent` | `fn get_message_content(message: &Message) -> &[u8]` | proto marshal of the payload actually encrypted |
| `makeDeviceIdentityNode` | `fn make_device_identity_node(&self) -> Node` | `device-identity` child from `Device::account` |
| `SendPeerMessage` / `sendPeerMessage` / `preparePeerMessageNode` | `Client::{send_peer_message, prepare_peer_message_node}` | `to = own JID`, `phash` handling |
| `RevokeMessage` / `BuildRevoke` / `BuildMessageKey` | `Client::{revoke_message, build_revoke, build_message_key}` | revoke = send with `protocolMessage type=REVOKE` |
| `BuildReaction` / `BuildEdit` / `SetDisappearingTimer` / `BuildHistorySyncRequest` / `BuildUnavailableMessageRequest` | same names in snake_case | pure builders, unit-test against Go golden bytes where available |
| `SendResponse` | `pub struct SendResponse { id: MessageID, timestamp: i64, debug_timeout: bool }` | `specta::Type` — the frontend shows it |

### 2.2 Retry (`retry.go` — incoming side lives here, outgoing receipt in `retry_receipt.rs` already)

| Go | Rust | Notes |
|---|---|---|
| `addRecentMessage` / `getRecentMessage` | `Client::{add_recent_message, get_recent_message}` | `RecentMessages` = `Mutex<LruCache<(Jid, MessageID), RecentMessage>>`, cap **512** (Go's `recentMessagesMap`); LRU via the `lru` crate (workspace dep) |
| `getMessageForRetry` | `Client::get_message_for_retry(&self, receipt: &Receipt, id: &MessageID) -> Result<RecentMessage, ClientError>` | store → recent-messages → `UseRetryMessageStore` hook (`TODO(Phase 5)` if the hook needs app-state) |
| `tryHandleRetryReceipt` / `handleRetryReceipt` | `Client::{try_handle_retry_receipt, handle_retry_receipt}` | decrypt-failure → resend path; `shouldRecreateSession` decides session reset |
| `delayedRequestMessageFromPhone` / `immediateRequestMessageFromPhone` / `cancelDelayedRequestFromPhone` / `clearDelayedMessageRequests` | same names in snake_case | timers via `tokio::time::sleep` under the connection token; cancel on logout |
| `sendRetryReceipt` (outgoing) | already in `retry_receipt.rs` (Task C) — call it, do not duplicate | — |

### 2.3 Receipts (`receipt.go`)

| Go | Rust | Notes |
|---|---|---|
| `handleReceipt` | `Client::handle_receipt(&self, &Node)` | dispatch: `read`/`read-self`/`played`/`played-self`/`delivered`/`sender`/`server-error`/`retry`/`mediaretry`; unknown types → debug log, never panic |
| `handleGroupedReceipt` | `Client::handle_grouped_receipt(&self, Receipt, &Node) -> bool` | `cancelled` return preserved |
| `parseReceipt` | `fn parse_receipt(&Node) -> Result<(Receipt, Vec<Node>), ClientError>` | `Receipt { chat, sender, message_ids, timestamp, type, … }` |
| `maybeDeferredAck` / `backgroundIfAsyncAck` | reuse `ack.rs` (Task C) | — |
| `MessageIDs` ordering | `fn sort_message_ids(ids: &mut [MessageID], is_ios: bool)` | Android vs iOS ordering differs — pin with a unit test, both branches |

`Receipt` event (`types/events.rs`, `serde(tag="type")`, `specta::Type`):
`Receipt { chat, sender, message_ids, timestamp, receipt_type }`. The frontend (Task G) renders
ticks from it.

### 2.4 Media retry (`mediaretry.go`)

| Go | Rust | Notes |
|---|---|---|
| `SendMediaRetryReceipt` | `Client::send_media_retry_receipt(&self, info: &MessageInfo, media_key: &[u8]) -> Result<(), ClientError>` | direct-media-download path |
| `handleMediaRetryNotification` | `Client::handle_media_retry_notification(&self, &Node)` | re-upload trigger |

### 2.5 Required tests (Task D, all offline)

| Test | Proves |
|---|---|
| `send_dm_builds_and_encrypts` | synthetic peer: `send_message` → server receives a `<message>` node with one `<enc>` child per device, decryptable with the Task-B test vectors |
| `send_group_fans_out` | group send → N `<enc>` children, one failing device does not abort the send |
| `send_requires_login` | `send_message` while logged out → `Err(NotLoggedIn)`, no frame sent |
| `revoke_and_reaction_builders` | `build_revoke`/`build_reaction` byte-match Go goldens (or the crate's own roundtrip if no golden exists) |
| `recent_messages_is_lru_512` | insert 600 → len stays 512, oldest evicted, newest present |
| `retry_receipt_resends_from_recent` | injected undecryptable node → `handle_retry_receipt` → server sees the resent message from `recent_messages` |
| `retry_gives_up_after_max` | `shouldRecreateSession` path: after N retries the session is rebuilt, event emitted |
| `receipt_parses_all_types` | table test over `read/read-self/played/played-self/delivered/sender/server-error/retry/mediaretry` → right `Receipt` values; unknown type → debug, no panic |
| `message_ids_ordering` | Android vs iOS order pinned by unit test |
| `media_retry_receipt_roundtrip` | `send_media_retry_receipt` → synthetic server ack, no panic on malformed notification |

---

## 3. Task E — media up/download (`client/upload.rs`, `download.rs`, `mediaconn.rs`)

Port targets: `whatsmeow/upload.go`, `download.go`, `mediaconn.go`, `download-to-file.go`.

### 3.1 Go → Rust function table

| Go | Rust | Notes |
|---|---|---|
| `Upload(plaintext, appInfo)` / `UploadReader` / `UploadNewsletter` / `UploadNewsletterReader` | `Client::{upload, upload_reader, upload_newsletter}` | encrypt (`mediaKey` HKDF per `MediaType`), hash, `rawUpload`; `MediaType` enum mirrors Go (`Image`, `Video`, `Audio`, `Document`, `Sticker`, …) with the same `mms_type` strings |
| `rawUpload` | `Client::raw_upload(&self, …) -> Result<UploadResponse, ClientError>` | `POST` to the media host from `MediaConn`; `reqwest` (already a workspace dep via `bebok-server` — add to `wa-core` if missing) |
| `DeleteMedia` | `Client::delete_media` | best-effort `DELETE`, error logged not propagated |
| `refreshMediaConn` / `queryMediaConn` | `Client::{refresh_media_conn, query_media_conn}` | `MediaConn` cache: `Mutex<Option<(MediaConn, Instant)>>`, refresh when older than `ttl` (Go: force flag); concurrent callers share one refresh (`tokio::sync::OnceCell` or a mutex-guarded future) |
| `DownloadAny` / `Download` / `DownloadThumbnail` / `DownloadMediaWithPath` / `DownloadFB` | `Client::{download_any, download, download_thumbnail, download_media_with_path, download_fb}` | `DownloadableMessage` trait for `&Message` (has `direct_path`, `media_key`, `file_sha256`, …); verify SHA-256 + decrypt, mismatch → `Err(MediaChecksumMismatch)` |
| `downloadAndDecrypt` / `downloadPossiblyEncryptedMediaWithRetries` / `doMediaDownloadRequest` | private helpers, same structure | retries = 3 with backoff, then give up with `ClientError` |
| `FetchStickerPack` | `Client::fetch_sticker_pack` | mock-HTTP test only |
| `download-to-file.go` | `Client::download_to_file(&self, msg, path) -> Result<u64, ClientError>` | streaming to disk, returns bytes written; used by the Tauri `download_media` command |

`UploadResponse` / `MediaConn` / `MediaType` are `specta::Type` — the frontend `media-preview`
(Task G) shows progress from them.

### 3.2 Required tests (Task E, all offline — mock HTTP, no WhatsApp)

| Test | Proves |
|---|---|
| `upload_roundtrip_against_mock` | `wiremock`/`axum` mock media host: `upload` → `UploadResponse { url, direct_path, media_key, file_sha256, … }`, ciphertext decrypts to plaintext with `media_key` |
| `upload_retries_then_gives_up` | mock returns 500 × N → 3 attempts, then `Err(UploadFailed)` |
| `media_conn_caches_until_ttl` | two `query_media_conn` calls → one HTTP hit; expired `ttl` → second hit |
| `download_verifies_hash` | tampered bytes from mock → `Err(MediaChecksumMismatch)`, no partial file left behind |
| `download_to_file_streams` | 5 MB via mock → file on disk, byte-identical, returns size |
| `thumbnail_path` | `download_thumbnail` returns the JPEG bytes without full download |

New dev-dependency `wiremock` (or reuse an `axum` test server like the Phase-3 harness) — one
choice, documented in the report.

---

## 4. Task F — server endpoints, Tauri commands, `wa-store` wiring

### 4.1 New routes (`bebok-server/src/routes/`)

New module `routes/messages.rs` (+ `routes/media.rs` if it grows past ~300 lines), registered in the
routing-table doc + `router()` **inside** the `auth::require_token` stack:

| Method | Path | Handler | Notes |
|---|---|---|---|
| POST | `/send` | `messages::send` | `{ to: Jid, text: string, … }` → `Client::send_message` → `SendResponse`; token guard; `400` on bad JID, `409` when not logged in |
| POST | `/send_media` | `media::send` | multipart or JSON `{ to, data_base64|path, media_type, caption? }` → upload + send; streams progress via `WaEvent` (reuse `history_export_progress` pattern or a new `media_upload_progress` event — one choice, documented) |
| GET | `/messages/:id` | `messages::get` | `getMessageForRetry`-backed lookup → `MessageInfo` + payload or `404` |
| POST | `/mark_read` | `messages::mark_read` | `{ chat, ids[] }` → read receipt |
| POST | `/mark_played` | `messages::mark_played` | `{ chat, ids[] }` → played receipt |
| GET | `/chats/:jid/history` | `messages::history` | `fetch_history` pagination (`before_id?`, `limit?`) → `wa-store::page` first, live fetch as fallback |
| POST | `/media/upload` | `media::upload` | raw upload → `UploadResponse` |
| GET | `/media/download` | `media::download` | `?direct_path=&media_key=&sha256=` → bytes with `Content-Disposition`; checksum verified engine-side |

Update `API-WHATSAPP.md` §§3–4 (request/response shapes, error codes) and the routing-table doc.
If any wire type changes, run `npm run specta` and commit the regenerated
`client/src/app/types/specta.ts`; `npx ng build` must stay clean.

### 4.2 Tauri commands (`client/src-tauri/src/**`)

Mirror commands: `send_message`, `send_media`, `fetch_history`, `get_message`, `mark_read`,
`mark_played`, `upload_media`, `download_media` — each a thin wrapper over the same handler the
HTTP route calls (like the existing `connect`/`disconnect` pattern). Add them to
`tauri.service.ts` `COMMAND_ROUTES` and to the `NATIVE_COMMANDS` set only if they stream to disk
(`download_media` does → native; the rest go over HTTP).

### 4.3 `wa-store` wiring (the search/export payoff)

* Every `WaEvent::Message`/`FbMessage` → `wa_store::insert_message` (transaction + FTS + embedding,
  `spawn_blocking` like the existing write path). Every *sent* message too (insert after ack, with
  `sender = own JID`).
* `fetch_history` backfill paginates into `insert_messages` in batches.
* No schema change → no migration; if you touch the schema, add a migration +
  `PutDevice`/`GetDevice`-style roundtrip test (per the PR checklist).

### 4.4 Required tests (Task F, all offline)

| Test | Proves |
|---|---|
| `post_send_returns_response` | `POST /send` with token → `SendResponse`, synthetic peer got the frame; `401` without token; `409` when logged out |
| `mark_read_and_played` | receipts observed by the synthetic peer; idempotent |
| `history_serves_store_first` | seeded `wa-store` row returned without touching the socket; empty store → live `fetch_history` fallback |
| `media_upload_download_roundtrip` | mock media host: `POST /media/upload` → `GET /media/download` byte-identical |
| `every_message_lands_in_store` | incoming + sent messages appear in `wa-store::search`, FTS-indexed |
| `tauri_commands_mirror_http` | each new command resolves to the same handler (unit test on the routing table, no WebView needed) |

---

## 5. Task G — Angular message UI (`features/messages/`)

### 5.1 New components (standalone, `OnPush`, signals only, English)

* `message-view.component.ts` — virtual scroll list (`@angular/cdk/scrolling`), renders text /
  image / video / audio / document / sticker / view-once / ephemeral / edited / revoked states from
  `Message` + `MessageInfo`; ticks from the `Receipt` event (`sent → delivered → read`, blue on
  `played` where applicable). `UndecryptableMessage` → "Couldn't decrypt — retry" row with a retry
  button (`send_retry_receipt` path).
* `composer.component.ts` — text input + send button + reply/edit state + error toast; calls
  `whatsapp.sendMessage(to, text)`; disables while `connectionState !== 'connected'`.
* `media-preview.component.ts` — thumbnail → full preview dialog, upload/download progress bars
  (from `media_upload_progress` / `history_export_progress`-style events), caption editor.
* `whatsapp.service.ts` additions: `messages = signal<Message[]>([])` per open chat,
  `sendMessage`, `sendMedia`, `markRead`/`markPlayed`, `fetchHistory`, `handleEvent` extensions for
  `message`/`receipt`/`media_*` events (append + tick-update, never full-list replace).
* `app.routes.ts` — route(s) for the message view if not already covered by `'chat/:jid'`; lazy
  `loadComponent` like the other features.

### 5.2 Specs (`*.spec.ts` next to each component, mock services like `whatsapp.service.spec.ts`)

* `message-view.spec.ts` — renders text/image/view-once/ephemeral/edited/revoked rows, ticks advance
  on `receipt`, undecryptable row shows retry button.
* `composer.spec.ts` — send calls `sendMessage` and clears input; error shows toast; disabled when
  offline; reply/edit prefixes handled.
* `media-preview.spec.ts` — progress bar follows events; caption included in `sendMedia` call.
* `whatsapp.service.spec.ts` (extend) — `message` event appends, `receipt` updates ticks,
  `sendMessage` → `invoke('send_message')`.

No redesign of `chat-list` / `chat` — these components are additive.

---

## 6. Gates (run after each task, fix before moving on)

```bash
cd engine && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
cd client/src-tauri && cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings
cd client && npx ng build && npx ng test --watch=false --browsers=ChromeHeadless
npm run specta   # no diff unless wire types changed (then commit the regeneration)
```

Baselines before Phase 4: engine tests = whatever Phase 3 left (was 183 before Phase 3),
`client/src-tauri` 4 tests, `ng test` 51 tests. All suites must grow; CI green on
Linux/Windows/macOS. `cargo test --workspace -- --test-threads=1` if SQLite contends.

---

## 7. Rules

* Never delete existing code or `whatsmeow/`. Never edit files outside the ownership list of the
  task you are on (§1.3 + backend draft §2.4).
* MPL-2.0 header on every ported Rust file. English only — UI strings, code, comments, tests, docs.
* Phase 5 (groups, AppState, presence, bot decryption, history sync) is out of scope — leave
  documented `TODO(Phase 5)` at the exact call sites.
* IPC change → regenerate `specta` + update `whatsapp.service.ts` / `tauri.service.ts`.
* Store change → migration + roundtrip test.
* Frontend change → ask before verifying in the browser (dev servers: `bash` with
  `background:true` + `ready_port`; engine + `ng serve`).
* If you need another Go golden vector (e.g. a `send`/`receipt` transcript), generate it the
  established way: throwaway module under `whatsmeow/zz-<name>/` via `go run ./zz-<name>`, paste the
  numbers as constants, then **delete the directory**. Never commit it.

---

## 8. Report (fill after Tasks D–G)

Per task: every file touched, the `Client` methods added, which Go function each Rust function ports,
the events emitted per path, test counts per suite, anything deferred with the reason, and the exact
commands to verify it independently.

| Task | Files touched | Key decisions / deferred |
|------|---------------|--------------------------|
| D — send/retry/receipts | `wa-core/src/client/{send,retry,receipt,mediaretry}.rs`, `types/{message,receipt,events}.rs` | LRU choice; `UseRetryMessageStore` hook status; iOS/Android ordering |
| E — media | `wa-core/src/client/{upload,download,mediaconn}.rs` | mock-HTTP choice; `MediaConn` cache strategy; checksum error shape |
| F — server + store | `bebok-server/src/routes/{messages,media}.rs`, `routes/mod.rs`, `client/src-tauri/src/**`, `API-WHATSAPP.md` | route shapes; native-vs-HTTP split; `wa-store` write path |
| G — frontend | `client/src/app/features/messages/**`, `core/services/*.ts`, `app.routes.ts` | virtual-scroll choice; progress event shape; redirect strategy |

Verification commands (from repo root):

```bash
cd engine && cargo test --workspace -- --nocapture
cd engine && cargo test -p wa-core client::send -- --nocapture
cd client && npx ng test --watch=false --browsers=ChromeHeadless
cd client && npx ng build
npm run specta && git diff --exit-code client/src/app/types/specta.ts
```

---

## Environment notes (save yourself the detour)

- Fleet health during Phase 3: `code1` failed with 402 `billing_not_configured`, the default
  `code` agent returned 429, and `code2` got aborted around 120 s (its partial work was often
  usable). Before wave 1, run a short probe task on each of `code1`/`code2` (e.g. "report
  `cargo --version`"). If a member still fails, apply the fallback rule in "Execution". Keep
  `code2` briefs small enough to finish within ~120 s, and **read `task_report` before re-tasking
  anything**.
- Crate fetching works (`wiremock`, `lru`, `reqwest`, etc. can be added this way) and Go is available
  for vector generation.
- `cargo test` on this workspace is fast; keep `--test-threads=1` in mind if SQLite contention shows up.
