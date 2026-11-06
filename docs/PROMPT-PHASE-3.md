# Phase 3 prompt — Client Core (session state machine, pairing, keepalive)

Copy the block below into a fresh session to start Phase 3.

---

Continue the ngWhatsapp project (Angular + Tauri 2 + Rust engine) in this repository.
Read `README.md` → `ARCHITECTURE.md` → `API-WHATSAPP.md` → `AGENTS.md` → `TODO.md` first.

## Baseline (do not regress — all green before Phase 3)

- `cd engine && cargo test --workspace` → **183 tests**
- `cd client/src-tauri && cargo test` → 4 tests
- `cd client && npx ng test --watch=false --browsers=ChromeHeadless` → 51 tests
- `cargo fmt --all --check` + `cargo clippy --workspace --all-targets -- -D warnings` clean (engine and `client/src-tauri`)
- `npx ng build` clean; `npm run specta` produces no diff

Product name is ngWhatsapp. Keep the engine naming: `wa-core`, `wa-store`, `wa-proto`, `wa_*` SQLite
tables, `wa-core.db`, the `bebok-server` sidecar binary, the `BEBOK_READY` stdout contract and the
"local WhatsApp client" wording in prose. English only, no i18n.

Phases 1–2 are done. What already exists and must be *reused*, not rewritten, is listed in the
tables below (`KeepAlive` is ported and tested but wired to nothing yet — this phase connects it).
Phase 4 (messaging, libsignal sessions, pre-key upload, app state, media) is **out of scope** —
leave documented TODOs.

Protocol source of truth: `whatsmeow/` (Go, MPL-2.0). Relevant for this phase: `client.go`
(`Connect`, `Disconnect`, `handleFrame`, `handlerQueueLoop`, `sendNode`, `IsConnected`, `IsLoggedIn`),
`request.go` (`generateRequestID`, `receiveResponse`, `sendIQ`, `retryFrame`, `clearResponseWaiters`),
`pair.go` (`handleIQ`, `handlePairDevice`, `handlePairSuccess`, `rotateADVSecret`), `pair-code.go`
(`PairClientType` constants only), `connectionevents.go` (`handleStreamError`, `handleIB`,
`handleConnectFailure`, `handleConnectSuccess`), `keepalive.go`, `qrchan.go` (rotation policy only).
Never modify `whatsmeow/`.

Task ownership: **A–C = `wa-core`, D = `bebok-server`, E = Angular.** Never edit files outside the
ownership list of the task you are on. Every new ported file keeps the MPL-2.0 header + the
three-line boilerplate (`// Ported from whatsmeow <go file>:<func>, © Tulir Asokan, MPL-2.0 —` /
`// see THIRD_PARTY_NOTICES.md.` / `// This Source Code Form ... http://mozilla.org/MPL/2.0/.`).
No `unwrap`/`expect`/`panic!` outside `#[cfg(test)]`.

## 2. Cross-cutting decisions (settle these first — they shape all three tasks)

### 2.1 File ownership

| File | Action | Task |
|---|---|---|
| `engine/crates/wa-core/src/client.rs` | **extend** — module root: `Client`, `ClientConfig`, connect/disconnect, node-loop wiring, `ClientError` additions | A |
| `engine/crates/wa-core/src/client/session.rs` | **new** — socket ownership, `response_waiters`, `send_node`, `generate_request_id`, `receive_response`, `send_iq*` | A |
| `engine/crates/wa-core/src/client/connection_events.rs` | **new** — `handleStreamError`, `handleIB`, `handleConnectFailure`, `handleConnectSuccess`, `SetPassive`, `ConnectFailureReason` | A |
| `engine/crates/wa-core/src/client/pair.rs` | **new** — the `pair.go` port | B |
| `engine/crates/wa-core/src/client/testutil.rs` | **new**, `#[cfg(test)]` — synthetic-server helpers shared by `client/tests.rs` and the new test modules | A |
| `engine/crates/wa-core/src/client/tests.rs` | **extend** — existing handshake tests stay; new tests live in `#[cfg(test)] mod` blocks next to the code they cover | A, B, C |
| `engine/crates/wa-core/src/types/events.rs` | **extend** — `PairError` fields, `StreamError`, `ManualLoginReconnect`, `QrScannedWithoutMultidevice` | A, B |
| `engine/crates/wa-core/src/types/jid.rs` | **extend** — `server_jid()` | A |
| `engine/crates/wa-core/src/keepalive.rs` | **no behavioural change** (docs only, if at all) | C |
| `engine/Cargo.toml`, `engine/crates/wa-core/Cargo.toml` | **extend** deps (see §2.2) | A, B |
| `API-WHATSAPP.md` §2 (events) | **update** if `WaEvent` gains variants | A, B |
| `TODO.md` Phase 3 checkboxes | **tick** what lands | A, B, C |

`client.rs` and `client/` coexist (as today: `client.rs` + `client/tests.rs`), so `client.rs` stays the
module root and the new files become `pub(crate) mod session; pub(crate) mod connection_events;
pub(crate) mod pair;` with `pub use` of what `bebok-server` needs.

### 2.2 New dependencies

```toml
# engine/Cargo.toml [workspace.dependencies]
hmac       = "0.12"                                  # pair.go: hmac.New(sha256.New, AdvSecretKey)
tokio-util = { version = "0.7", features = ["rt"] }  # CancellationToken for expectedDisconnect / connCtx
```

`sha2` (0.10), `rand` (0.8), `prost`, `hex` are already there. Crate fetching works in this environment.
If `tokio-util` is unwanted, the alternative is `tokio::sync::watch<bool>` + `Notify`; `CancellationToken`
is preferred because `expectedDisconnect` is exactly a cancellable/awaitable flag (Go `exsync.Event`).

### 2.3 Device mutability (important — the parent prompt got this wrong)

Go holds `*store.Device`, mutates it during pairing and calls `Store.Save(ctx)`. Rust's
`Device::save(&mut self)` needs unique access, so:

```rust
pub struct Client {
    /// Go: `cli.Store`. `Mutex` because pairing mutates the device (ID/LID/account/platform)
    /// and `Device::save` takes `&mut self`.
    device: Arc<Mutex<Device>>,
    // …
}
```

Expose `pub async fn device(&self) -> Device` (a clone for readers) and keep every *write* under the same
mutex lock as the `save()` call, so a concurrent `is_logged_in()` never sees a half-paired device.

### 2.4 Error additions to `ClientError`

One variant per new Go error path (Go names in doc comments), no `unwrap`:

```rust
    #[error("not connected")]
    NotConnected,                                      // ErrNotConnected
    #[error("already connected")]
    AlreadyConnected,                                  // ErrAlreadyConnected
    #[error("not logged in")]
    NotLoggedIn,                                       // ErrNotLoggedIn
    #[error("the device was deleted")]
    DeviceDeleted,                                     // store.ErrDeviceDeleted
    #[error("timed out waiting for {what} ({id})")]
    RequestTimeout { what: &'static str, id: String }, // ErrIQTimedOut
    #[error("connection lost while waiting for {what} ({id})")]
    Disconnected { what: &'static str, id: String },   // DisconnectedError
    #[error("unexpected iq response for {id}: {node}")]
    Iq { id: String, node: String },                   // IQError
    #[error("failed to encode/decode a node: {0}")]
    Binary(#[from] BinaryError),                       // waBinary.Marshal/Unmarshal
    #[error("store error: {0}")]
    Store(#[from] StoreError),                         // PairDatabaseError
```

### 2.5 Event sink

`wa-core` must not depend on `bebok-server`, so the sink is a boxed closure:

```rust
pub type EventSink = Arc<dyn Fn(WaEvent) + Send + Sync + 'static>;
```

`bebok-server` passes `Arc::new(move |ev| { let _ = events.send(ev); })` (the same
`broadcast::Sender<WaEvent>` that `AppState::emit` writes to); tests pass a collector closure. Emission is
synchronous and must never block or panic (Go's `dispatchEvent` `recover()` equivalent) — say so on the type.

### 2.6 `ClientConfig`

```rust
pub struct ClientConfig {
    pub wa_url: Option<String>,        // Config::wa_url — None = production URL
    pub handshake: HandshakeOptions,   // response_timeout + cert_authority + check_cert_validity
    pub enable_auto_reconnect: bool,   // default true
    pub reconnect: ReconnectConfig,    // §5.2
    pub request_timeout: Duration,     // Go defaultRequestTimeout = 75 s
    pub keepalive: KeepAliveConfig,    // forwarded to KeepAlive::new
    pub user_agent: Option<String>,    // FrameSocket header, Go getUserAgent()
}
impl Default for ClientConfig { /* … */ }
```

Tests only override `wa_url` (a `ws://127.0.0.1:<port>` from the synthetic server),
`handshake.cert_authority` / `handshake.check_cert_validity`, and shorten `request_timeout`, the
`reconnect` delays and the `keepalive` intervals so nothing waits 20–75 s.

---

## 3. Task A — `struct Client` + the node loop

Port `whatsmeow/client.go` (connection/session machinery) + `whatsmeow/request.go`, nothing more.

### 3.1 Struct

```rust
pub struct Client {
    device: Arc<Mutex<Device>>,                       // Go: cli.Store
    socket: Mutex<Option<Arc<NoiseSocket>>>,          // Go: cli.socket (socketLock)
    response_waiters: Mutex<HashMap<String, oneshot::Sender<Node>>>, // Go: responseWaiters
    events: EventSink,                                // Go: eventHandlers + dispatchEvent
    is_logged_in: AtomicBool,                         // Go: cli.isLoggedIn
    paired: AtomicBool,                               // Go: cli.paired (Store.ID != nil)
    expected_disconnect: Mutex<CancellationToken>,    // Go: cli.expectedDisconnect (exsync.Event)
    force_auto_reconnect: AtomicBool,                 // Go: cli.forceAutoReconnect
    reconnect_errors: AtomicU32,                      // Go: cli.AutoReconnectErrors
    server_time_offset: AtomicI64,                    // Go: cli.serverTimeOffset
    config: ClientConfig,
    unique_id: String,                                // Go: cli.uniqueID "<a>.<b>-"
    id_counter: AtomicU64,                            // Go: cli.idCounter
    conn_cancel: Mutex<Option<CancellationToken>>,    // Go: connCtx / fs.Context()
    handler_queue_done: Mutex<Option<oneshot::Receiver<()>>>, // Go: cli.handlerQueueWait
    // TODO(Phase 4+): messageRetries, recentMessages (512), groupCache, userDevicesCache,
    // appStateProc, mediaConnCache, privacySettingsCache, tcTokenSenderTS, PreKeysUploaded,
    // historySyncNotifications, pendingPhoneRerequests.
}
```

`Client::new(device: Device, config: ClientConfig, events: EventSink) -> Arc<Self>` and
`Client::from_container(container: &Container, config: ClientConfig, events: EventSink) -> Result<Arc<Self>, ClientError>`
(`container.get_first_device()` → fresh unsaved device when the store is empty → the QR path, exactly
what Go's `NewClient` receives). The constructor sets `paired = device.id.is_some()`.

### 3.2 Public API (what `bebok-server` Task D calls)

```rust
impl Client {
    pub fn new(device: Device, config: ClientConfig, events: EventSink) -> Arc<Self>;
    pub async fn from_container(container: &Container, config: ClientConfig, events: EventSink)
        -> Result<Arc<Self>, ClientError>;

    pub async fn connect(self: &Arc<Self>) -> Result<(), ClientError>;  // Go: Connect/ConnectContext
    pub async fn disconnect(&self);                                     // Go: Disconnect (silent)
    pub async fn reset_connection(self: &Arc<Self>);                    // Go: ResetConnection
    pub async fn logout(self: &Arc<Self>) -> Result<(), ClientError>;   // Go: Logout (Task D uses it)
    pub async fn wait_for_connection(&self, timeout: Duration) -> bool; // Go: WaitForConnection

    pub async fn is_connected(&self) -> bool;                           // Go: IsConnected
    pub fn is_logged_in(&self) -> bool;                                 // Go: IsLoggedIn
    pub async fn device(&self) -> Device;                               // clone, for GET /logged_in

    pub async fn send_node(&self, node: &Node) -> Result<(), ClientError>;
    pub async fn send_node_and_get_data(&self, node: &Node) -> Result<Vec<u8>, ClientError>;
    pub async fn send_iq(&self, query: InfoQuery) -> Result<Node, ClientError>;
    pub async fn send_iq_async(&self, query: InfoQuery) -> Result<oneshot::Receiver<Node>, ClientError>;
    pub async fn set_passive(&self, passive: bool) -> Result<(), ClientError>;
}
```

`InfoQuery { namespace: String, kind: IqKind::{Get, Set}, to: Jid, target: Option<Jid>, id: Option<String>,
smax_id: Option<String>, content: Vec<Node>, timeout: Option<Duration>, no_retry: bool }` — field for field
Go's `infoQuery`.

### 3.3 `connect()` — the exact Go order

```text
connect()                                        // Go: Client.connect → unlockedConnect
 1. device.deleted                    → Err(DeviceDeleted)
 2. socket exists & is_connected      → Err(AlreadyConnected); exists but dead → unlocked_disconnect()
 3. reset_expected_disconnect()       // force_auto_reconnect = false, expected_disconnect = fresh token
 4. frames = FrameSocket::connect(config.wa_url.as_deref())
 5. payload = build_client_payload(WA_VERSION)    // Phase 4 fills device_pairing_data
 6. queue   = mpsc::channel::<Node>(2048)         // Go: handlerQueueSize
 7. socket  = do_handshake_with(frames, &KeyPair::generate(), &device.noise_key, payload,
                                config.handshake, on_frame, Some(on_disconnect))
 8. socket ← Some(Arc::new(socket)); paired = device.id.is_some()
 9. spawn handler_queue_loop(queue, conn_token)
10. spawn KeepAlive::new(config.keepalive, ping, emit, force_reconnect).run(conn_token.cancelled())  // Task C
11. is_retryable_connect_error(err)? if enable_auto_reconnect && initial_auto_reconnect →
        emit Disconnected + spawn auto_reconnect
```

`connect()` must **not** block on the pairing outcome: it returns after step 10 and the node loop emits
`Qr` / `Connected` / `ConnectFailure` — that keeps the `POST /connect` "return before the session settles"
contract Task D depends on. `is_retryable_connect_error` = Go's list: dial/handshake failures, socket
closed, handshake timeout, network errors.

### 3.4 `handle_frame` + `handler_queue_loop`

```rust
fn handle_frame(&self, data: &[u8], queue: &mpsc::Sender<Node>)   // Go: client.go:handleFrame
    unpack(data)             → Err: warn "failed to decompress frame" + hex dump; return (never panic)
    unmarshal(&decompressed) → Err: warn "failed to decode node in frame" + hex dump; return
    if node.tag == "xmlstreamend" → warn if !expected_disconnect; return
    if receive_response(node)     → return                      // id match resolves the waiter
    if is_known_node(&node.tag)   → queue.try_send(node); Full → warn "Handler queue is full" + blocking send
    else if node.tag != "ack"     → debug "didn't handle node"
```

`handler_queue_loop` mirrors Go: one task, `while let Some(node) = rx.recv()`, bail when the connection token
is cancelled, dispatch through `handle_node(tag, &Node)`, and on exit drain the queue through
`handle_stream_error` for leftover `stream:error` nodes, then resolve `handler_queue_done`.
Go's 30 s "node handling is taking long" watchdog is **not** ported (handlers here are fast and async) —
leave a `TODO(Phase 4)` note instead of faking the goroutine dance.

`is_known_node` = Go's map: `message`, `status`, `appdata`, `receipt`, `call`, `chatstate`, `presence`,
`notification`, `success`, `failure`, `stream:error`, `iq`, `ib`. This phase implements `success`,
`failure`, `stream:error`, `iq` and `ib` (→ `pair.rs`); the rest log at debug with `TODO(Phase 4/5)`
(do not invent handlers).

### 3.5 Requests (`client/session.rs`)

| Go (`request.go`) | Rust |
|---|---|
| `generateRequestID` = `uniqueID + idCounter.Add(1)` | `Client::generate_request_id() -> String` |
| `waitResponse(reqID)` | `register_waiter(&self, id) -> oneshot::Receiver<Node>` |
| `cancelResponse(reqID, ch)` | `cancel_waiter(&self, id)` (drops the sender) |
| `receiveResponse(ctx, node)` | `receive_response(&self, &Node) -> bool` — `iq`/`ack` with a matching `id`, removes the waiter |
| `clearResponseWaiters(node)` | `clear_response_waiters(&self, node: Option<&Node>)` — resolves every waiter with the disconnect node, or drops it |
| `sendIQAsyncAndGetData` | `send_iq_async_and_get_data(&self, &InfoQuery) -> Result<(oneshot::Receiver<Node>, Vec<u8>), ClientError>` |
| `sendIQAsync` / `sendIQ` | `send_iq_async` / `send_iq` (timeout `config.request_timeout`, Go 75 s) |
| `retryFrame` | Minimal path only: `wait_for_connection(5 s)` → re-register waiter → resend the marshalled bytes. If it drags in more plumbing, defer with a documented `TODO(Phase 5)` and say so in the report |
| `isDisconnectNode` / `isAuthErrorDisconnect` | `is_disconnect_node(&Node)`, `is_auth_error_disconnect(&Node)` (`401`, `conflict type=replaced`, `device_removed`) |

`unique_id` = Go's `random.Bytes(2)` → `format!("{}.{}-", bytes[0], bytes[1])`.

### 3.6 Connection events (`client/connection_events.rs`)

| Go (`connectionevents.go`) | Rust | Emits |
|---|---|---|
| `handleConnectSuccess` | `Client::handle_connect_success(&self, &Node)` | `Connected` |
| `handleConnectFailure` | `Client::handle_connect_failure(&self, &Node)` | `ConnectFailure { reason, message }`, `LoggedOut { on_connect: true, reason }`, `TemporaryBan { code, expire_ms }`, `ClientOutdated` |
| `handleStreamError` | `Client::handle_stream_error(&self, &Node)` | `LoggedOut { on_connect: false, reason: 401 }` + device delete, `StreamReplaced`, `StreamError { code }` (new variant); `515` → silent `disconnect()` + `connect()`; `503` → let auto-reconnect handle it |
| `handleIB` | `Client::handle_ib(&self, &Node)` | `QRScannedWithoutMultidevice` (new). `OfflineSyncPreview` / `OfflineSyncCompleted` → `TODO(Phase 4)` |
| `SetPassive` | `Client::set_passive(bool)` | — (iq `xmlns=passive`, child `active` / `passive`) |

`ConnectFailureReason` (Go `types/events`) as constants: `400 Generic, 401 LoggedOut, 402 TempBanned,
403 MainDeviceGone, 405 ClientOutdated, 406 UnknownLogout, 409 BadUserAgent, 413 CATExpired,
414 CATInvalid, 415 NotFound, 418 ClientUnknown, 500 InternalServerError, 501 Experimental,
503 ServiceUnavailable`, plus `is_logged_out(401|403|406)` and the auto-reconnect set (`500|501|503`).
CAT (`413`/`414`) has no Rust equivalent yet → `TODO(Phase 6)`, treat as non-retryable.

`handle_connect_success` details that must not be skipped:

* `server_time_offset = node.t − now` → `AtomicI64` (nothing reads it yet; `TODO(Phase 4)` retry receipts).
* LID handling: stored `LID != node LID` → warn + clear; stored empty + node has one → set, `Device::save`.
  `TODO(Phase 4)`: `StoreLIDPNMapping` (needs the LID map + contact stores).
* `if !paired` → warn "received connect success without pairing" and return (Go behaviour).
* On success: `reconnect_errors = 0`, `is_logged_in = true`.
* Post-success work: **`set_passive(false)` + emit `Connected` only**. `uploadPreKeys` /
  `getServerPreKeyCount` are `TODO(Phase 4)` at that exact line (`whatsmeow/prekeys.go`, `MinPreKeyCount`).

### 3.7 Disconnect / reconnect plumbing

| Go | Rust |
|---|---|
| `onDisconnect(ctx, ns, remote)` | `Client::on_disconnect(self: Arc<Self>, ns: Weak<NoiseSocket>, remote: bool)` — ignore a foreign socket; else clear `socket`, `clear_response_waiters`, and if `!expected_disconnect && (force_auto_reconnect.swap(false) \|\| remote)` emit `Disconnected` + spawn `auto_reconnect` |
| `expectDisconnect` / `isExpectedDisconnect` / `resetExpectedDisconnect` | `expect_disconnect()` (cancel the token, clear `force_auto_reconnect`), `is_expected_disconnect()`, `reset_expected_disconnect()` (fresh token) |
| `Disconnect` / `unlockedDisconnect` | `disconnect()` = `expect_disconnect()` + `socket.stop(true, false)` + drop socket + `clear_response_waiters` + await `handler_queue_done` (5 s cap, warn on timeout) |
| `ResetConnection` | `reset_connection()` = `force_auto_reconnect = true` + `socket.stop(true, true)` + clear waiters |
| `Logout` | `logout()` = `send_iq(xmlns=md, type=set, to=server, <remove-companion-device jid=own reason=user_initiated/>)` → `disconnect()` → `device.delete()`; auto-reconnect stays disabled |
| `WaitForConnection` | `wait_for_connection(timeout)` = wait until `is_connected() && is_logged_in()` or the timeout/`expected_disconnect` fires |

The `auto_reconnect` loop itself belongs to Task C (§5).

---

## 4. Task B — pairing: `qr` → `success` → persisted device

Port `whatsmeow/pair.go` into `client/pair.rs`, plus the `PairClientType` constants from
`whatsmeow/pair-code.go` (constants only — the *code pairing flow* is out of scope).

### 4.1 Go → Rust mapping

| Go | Rust | Notes |
|---|---|---|
| `handleIQ` | `Client::handle_iq(&self, &Node)` | only `from == server_jid` and exactly one child; dispatch `pair-device` / `pair-success` |
| `handlePairDevice` | `Client::handle_pair_device(&self, &Node) -> Result<(), ClientError>` | `paired = false`; ack the iq (`type=result`, same `to`/`id`); for each `ref` child (`NodeContent::Bytes`) build a code via `make_qr_data`; emit **one** `WaEvent::Qr { codes }` |
| `makeQRData` | `fn make_qr_data(&self, reference: &[u8], client_type: PairClientType) -> String` | `https://wa.me/settings/linked_devices#{ref},{noise_pub_b64},{identity_pub_b64},{adv_secret_b64},{client_type}`, base64 **standard** encoding of the 32-byte public keys and of `adv_secret_key` |
| `getQRClientType` | `fn qr_client_type(&self) -> PairClientType` | `DeviceProps.platform_type` → `Chrome/Firefox/Edge/IE/Opera/Safari/UWP/Android`; `ClientPayload.user_agent.platform == WEB` → `OtherWebClient`; `MACOS` → `MacOS`; else `Unknown`. Our `device_props()` reports `PlatformType::Unknown` + a web platform, so the result is `PairClientOtherWebClient` (`"9"`) |
| `PairClientType` consts (`pair-code.go`) | `#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub enum PairClientType` with `as_str()` (`"0".."9"`, `"c"`, `"e"`) and `Display` | `Unknown=0, Chrome=1, Edge=2, Firefox=3, IE=4, Opera=5, Safari=6, Electron=7, UWP=8, OtherWebClient=9, MacOS=c, Android=e` |
| `handlePairSuccess` | `Client::handle_pair_success(self: &Arc<Self>, &Node)` | parse `pair-success`: `device-identity` bytes, `biz@name`, `device@jid`, `device@lid`, `platform@name`, `client-props` → `ClientPairingProps` (log-only); set `server_time_offset`; spawn `handle_pair`; on error `disconnect()` + `PairError`; on success `PairSuccess` |
| `handlePair` | `Client::handle_pair(&self, identity: &[u8], req_id: &str, business_name: &str, platform: &str, jid: Jid, lid: Jid) -> Result<(), PairError>` | the chain in §4.2 |
| `concatBytes` | `fn concat_bytes(parts: &[&[u8]]) -> Vec<u8>` | |
| `verifyAccountSignature` | `fn verify_account_signature(&AdvSignedDeviceIdentity, &KeyPair, hosted: bool) -> bool` | prefixes `ADV_ACCOUNT_SIGNATURE_PREFIX = [6, 0]` / `ADV_HOSTED_ACCOUNT_SIGNATURE_PREFIX = [6, 5]`; lengths must be 32/64; `crypto::keys::verify_signature(account_signature_key, details ‖ ikp.pub, account_signature)` |
| `generateDeviceSignature` | `fn generate_device_signature(&AdvSignedDeviceIdentity, &KeyPair) -> [u8; 64]` | prefix `[6, 1]`; message `details ‖ ikp.pub ‖ account_signature_key`; `calculate_signature` |
| `sendPairError` | `Client::send_pair_error(&self, id, code, text)` | best-effort iq `type=error` |
| `rotateADVSecret` | `Client::rotate_adv_secret(&self)` | new 32 random bytes into `device.adv_secret_key`; Go does **not** save it there — **do** `Device::save` (our store is the only copy) and note the difference in the doc comment |
| `handleCodePairNotification`, `PairPhone`, `generateCompanionEphemeralKey`, passkey linking | **out of scope** | keep `POST /pair/code` → `501` and a `TODO(Phase 7)` in `client/pair.rs` |

Error type: `pub enum PairError` (thiserror) with `InvalidHmac`, `Proto { what, source }`,
`InvalidSignature`, `Rejected`, `Database(#[from] StoreError)`, `Send(#[from] ClientError)`,
`Proto(…decode/encode)`. `WaEvent::PairError { id, lid, business_name, platform, error }` — extend the
existing variant with the four Go fields (camelCase on the wire) so the UI can say *which* device failed.

### 4.2 `handle_pair` step by step (must match Go order)

1. `AdvSignedDeviceIdentityHmac::decode(identity)` → `PairError::Proto` on failure, then `send_pair_error(500, "internal-error")`.
2. HMAC: `Hmac<Sha256>::new_from_slice(&adv_secret_key)`, prefix `[6, 0]` (`[6, 5]` when
   `account_type == Hosted`), write `details`; constant-time compare with `hmac` field →
   `send_pair_error(401, "hmac-mismatch")` + `PairError::InvalidHmac`.
3. `AdvSignedDeviceIdentity::decode(details)`, `AdvDeviceIdentity::decode(details.details)`.
4. `verify_account_signature(...)`, `hosted = device_type == Hosted` → `send_pair_error(401, "signature-mismatch")`.
5. `device_identity.device_signature = generate_device_signature(...)[..]` (64 bytes).
6. `main_device_lid = lid` with `device = 0`; `main_device_identity = account_signature_key` (32 bytes);
   clear `account_signature_key`; `self_signed = device_identity.encode_to_vec()`.
7. **Persist** (this is the "persisted device" of the task title):
   ```rust
   let mut device = self.device.lock().await;
   device.account = Some(account);          // the clone with account_signature_key intact
   device.id = Some(jid);
   device.lid = lid;
   device.business_name = business_name.to_string();
   device.platform = platform.to_string();
   device.save().await?;                    // → pair error 500 + PairError::Database on failure
   drop(device);
   self.paired.store(true, SeqCst);
   ```
   Afterwards `is_logged_in()` must still be `false` until the next `connect()` sees `success`
   (Go sets `isLoggedIn` in `handleConnectSuccess`, not here) — assert exactly that in a test.
8. `TODO(Phase 4)`: `StoreLIDPNMapping(lid, jid)` and `Identities::put_identity(main_device_lid.signal_address_user(), main_device_identity)`.
   Both need stores this phase does not own; leave them as documented TODOs **after** the save, and make
   the save the last thing that can fail (Go deletes the device when the identity put fails — mirror that
   ordering comment).
9. `expect_disconnect()` (the server drops us right after pairing, and that must **not** emit `Disconnected`
   or trigger a reconnect).
10. Send the confirmation iq: `to=server, type=result, id=req_id`,
    `<pair-device-sign><device-identity key-index="...">self_signed</device-identity></pair-device-sign>`.
    On send failure: `device.delete()` + `PairError`.
11. `handle_pair_success` then emits `WaEvent::PairSuccess { id, lid, business_name, platform }` (already
    exists) and logs. **No pre-key upload** — `TODO(Phase 4)` pointing at `whatsmeow/prekeys.go`.

### 4.3 QR rotation — deliberately *not* ported

Go's `qrchan.go` (`GetQRChannel`, `emitQRs`, 60 s for the first code and 20 s for the rest) is a
**client-side convenience**: the engine receives the whole `ref` list at once and emits it as
`WaEvent::Qr { codes }`. Rotation/expiry is rendered by the Angular QR screen (Task E), which already
models `qrCodes: string[]`. Document that decision in `API-WHATSAPP.md` §2 next to `qr`:

> The engine emits the full code list once per `pair-device` node; the client rotates it (first code 60 s,
> the rest 20 s, Go `qrchan.go:emitQRs`). `RotateADVSecret` is applied engine-side by re-emitting a new
> `qr` event with the rewritten codes.

`rotateADVSecret` therefore must **re-emit** the current codes with the old secret replaced by the new one
(Go's `qrChannel.handleEvent` does the rewrite), i.e. keep the last `ref` list (`Mutex<Vec<String>>` of raw
refs) so `rotate_adv_secret()` can rebuild the codes. That is the only QR-channel behaviour worth keeping.

---

## 5. Task C — keepalive + autoReconnect

### 5.1 Keepalive wiring (the crate is already written — only closures)

```rust
let ping = {
    let client = Arc::downgrade(self);
    move || {
        let client = client.clone();
        async move {
            let Some(client) = client.upgrade() else { return PingOutcome::Disconnected };
            match client.send_iq_async(InfoQuery {
                namespace: "w:p".into(), kind: IqKind::Get, to: server_jid(), ..Default::default()
            }).await {
                Ok(rx) => match rx.await { Ok(_) => PingOutcome::Answered, Err(_) => PingOutcome::Disconnected },
                Err(ClientError::NotConnected) => PingOutcome::Disconnected,
                Err(_) => PingOutcome::TimedOut,
            }
        }
    }
};
let emit = { let events = self.events.clone(); move |ev| (events)(ev) };  // KeepAliveTimeout / KeepAliveRestored
let force = { let client = Arc::downgrade(self); move || { /* §5.2 forced path */ } };
tokio::spawn(KeepAlive::new(config.keepalive, ping, emit, force).run(conn_token.cancelled()));
```

* The ping is a real `Node`: `<iq id=… type="get" to="s.whatsapp.net" xmlns="w:p"/>` (Go sends no child —
  do not invent a `<ping/>`, `sendIQAsync` with an empty `Content` is the exact wire form).
* `KeepAlive` already applies `response_deadline` (10 s) around `ping`, emits `KeepAliveTimeout` /
  `KeepAliveRestored`, and calls `force_reconnect` after `max_fail_time` (3 min) — do not duplicate that logic.
* `config.keepalive.auto_reconnect` must be `false` when `enable_auto_reconnect == false` or after
  `logout()`, so a failing ping can never resurrect a logged-out session.

### 5.2 `auto_reconnect` (Go `client.go:autoReconnect` + `handleDisconnected`)

```rust
pub struct ReconnectConfig {
    pub initial_delay: Duration,  // 2 s   (Go: AutoReconnectErrors * 2 * time.Second)
    pub max_delay: Duration,      // 60 s
    pub max_attempts: u32,        // 10    (after that: give up + ConnectFailure)
}
```

```text
async fn auto_reconnect(self: Arc<Self>) -> Result<(), ClientError>
  if !enable_auto_reconnect                     → return Ok(())          // no reconnect, no event
  if self.device().await.deleted                → return Err(DeviceDeleted)
  attempt = 0
  loop {
      if is_connected()                         → return Ok(())
      delay = min(initial_delay * 2^attempt, max_delay)                    // capped exponential
      tokio::time::sleep(delay) under the shutdown token
      attempt += 1; reconnect_errors.fetch_add(1)
      match connect() {
          Ok(())  → return Ok(())
          Err(DeviceDeleted) | Err(NotLoggedIn) → return Err(..)           // never retry these
          Err(e) if attempt >= max_attempts     → emit ConnectFailure { reason: 500, message: e }
                                                  + Disconnected; return Err(e)
          Err(_)  → continue
      }
  }
```

Facts to preserve (assert them in tests):

* `reset_expected_disconnect()` at the top of `connect()` is what stops an infinite reconnect loop after a
  local `disconnect()` (Go sets `expectedDisconnect` before reconnecting).
* `on_disconnect` only spawns `auto_reconnect` when the close was **not** expected
  (`!expected_disconnect`) *and* `force_auto_reconnect || remote` — a local `disconnect()`/`logout()` is silent.
* `keepalive`'s `force_reconnect` path = `disconnect()` + `reset_expected_disconnect()` + spawn `auto_reconnect`.
* The device is reused (same `Arc<Mutex<Device>>`, same keys), never re-created; a *logged-in* device takes
  the `success` path, a fresh one re-enters the QR path and re-emits `Qr`.

### 5.3 Interaction with Task D (hand-off note)

`bebok-server` only needs: `Client::connect()`, `.disconnect()`, `.logout()`, `.is_connected()`,
`.is_logged_in()`, `.device()` and the `EventSink` it already owns. Anything `connection.rs` needs beyond
that list must be requested from this half, not bolted on in Task D.

---

## 6. Offline tests (mandatory for every task)

**Hard rules**

* No network, no WhatsApp. Every socket test binds `127.0.0.1:0`, reads the real port from
  `listener.local_addr()`, and wraps its body in `tokio::time::timeout(STEP, …)` (`STEP = 5 s`), so a broken
  test fails instead of hanging. Enforce it in the harness, not per assertion.
* Build the peer with the **existing** helpers in `client/tests.rs` (`spawn_server`, `serve`, `next_frame`,
  `seal`/`open`, `cert_chain`, `deterministic_random`). Task A first moves them into
  `client/testutil.rs` (`#[cfg(test)] pub(crate) mod testutil`) so `client.rs`, `client/pair.rs` and
  `client/session.rs` tests share them — **move, do not copy**. The existing handshake tests must keep
  passing unchanged (same 12 tests).
* The synthetic server gets its own CA, and the client under test uses
  `ClientConfig { wa_url: Some(format!("ws://{addr}")), handshake: HandshakeOptions { cert_authority: ca.public, check_cert_validity: false, .. }, .. }`.
* Nodes on the wire are produced with the crate's own `binary::marshal` and sealed with the server-side
  `NoiseSocket` keys from `ServerOutcome` (write/read keys are already returned) — no hand-written hex
  unless it is a Go golden vector.
* Timeouts under test: `request_timeout: 200 ms`, `reconnect.initial_delay: 20 ms`,
  `reconnect.max_delay: 100 ms`, `keepalive: KeepAliveConfig { interval_min: 20 ms, interval_max: 40 ms,
  response_deadline: 100 ms, max_fail_time: 300 ms, .. }`.
* SQLite tests use `Container::open(tempfile::tempdir()/wa-core.db)` (`tempfile` is already a dev-dep).

### 6.1 Task A tests (`client.rs`, `client/session.rs`, `client/connection_events.rs`)

| Test | Proves |
|---|---|
| `connect_completes_and_emits_connected` | handshake + handler queue + `success` node → `WaEvent::Connected`, `is_connected()`, `is_logged_in()` |
| `connect_with_failure_node_emits_connect_failure` | `<failure><reason>403</reason><message>…</message></failure>` → `ConnectFailure { reason: 403, message }` |
| `stream_error_401_logs_out_and_deletes_the_device` | `stream:error code=401` → `LoggedOut`, `container.get_first_device()` is a *fresh* device |
| `stream_error_replaced_emits_stream_replaced` | `<conflict type="replaced"/>` → `StreamReplaced`, no reconnect |
| `connect_twice_is_already_connected` | second `connect()` → `Err(AlreadyConnected)`, no second socket |
| `disconnect_is_silent_and_idempotent` | `disconnect()` closes without `Disconnected`; a *server-side* close does emit `Disconnected` |
| `handler_queue_stays_bounded_at_2048` | the synthetic server floods 2100 `ack`-less nodes while the consumer is blocked; `queue.capacity() + queue.len() == 2048` and `try_send` reports `Full`, no panic, no unbounded growth |
| `garbage_and_truncated_frames_are_skipped` | frames `[0x00]`, `[0x02, 0x00]`, random bytes → no panic, the following valid node still arrives, `tracing` warn path taken |
| `response_waiter_resolves_on_matching_id` | `send_iq_async` ⇒ server answers `iq id=…` ⇒ receiver yields the node; the map is empty afterwards |
| `response_waiter_times_out_and_is_removed` | no answer, `request_timeout` 200 ms → `Err(RequestTimeout)`, and `response_waiters.lock().await.is_empty()` |
| `clear_response_waiters_on_disconnect` | server closes with 3 waiters outstanding → all resolve/err, map empty |
| `request_ids_are_unique_and_ordered` | 100 × `generate_request_id()` → distinct, numeric suffix increments |
| `send_node_requires_a_socket` | `send_node` before `connect()` → `Err(NotConnected)`, no panic |

### 6.2 Task B tests (`client/pair.rs`)

| Test | Proves |
|---|---|
| `pair_device_emits_qr_codes` | server sends `<ib><pair-device><ref>AAAA</ref><ref>BBBB</ref></pair-device></ib>` → one `Qr { codes }` with two `https://wa.me/settings/linked_devices#…` strings carrying the device's noise/identity public keys and `adv_secret_key` (decode the base64 in the assertion) |
| `pair_device_is_acked` | the synthetic server asserts it received `iq type=result` with the same `id` |
| `pair_success_persists_the_device` | full `pair-success` (a `wa_adv::AdvSignedDeviceIdentity` built in the test with a real HMAC under a known `adv_secret_key` and a real device signature) → `put_device`/`get_first_device` roundtrip in a `tempfile` DB: `id`, `lid`, `business_name`, `platform`, `account.device_signature.len() == 64`, `adv_secret_key` rotation applied |
| `pair_success_emits_pair_success_event` | `WaEvent::PairSuccess { id, lid, business_name, platform }` with the parsed values; `is_logged_in()` still `false` until the next `success` |
| `pair_hmac_mismatch_pairs_nothing` | wrong HMAC → `PairError`, device **not** saved, `pair-device-sign` never sent, server receives the `401 hmac-mismatch` error iq |
| `pair_signature_mismatch_pairs_nothing` | tampered `account_signature` → `PairError::InvalidSignature` + `401 signature-mismatch` |
| `pair_error_on_garbage_identity` | `device-identity` = random bytes → `PairError::Proto`, no panic, session disconnected |
| `qr_codes_are_rebuilt_after_adv_rotation` | `rotate_adv_secret()` → the next `Qr` event carries the same refs with the new secret embedded (assert the old secret is absent) |
| `pair_client_type_mapping` | a table test over `DeviceProps.platform_type` → `PairClientType` (Go's `getQRClientType`) |

### 6.3 Task C tests (`client.rs`, keepalive integration)

| Test | Proves |
|---|---|
| `keepalive_pings_and_survives` | short intervals; server answers every `xmlns=w:p` iq → ≥ 2 pings sent, no `KeepAliveTimeout`, session still connected |
| `keepalive_timeout_emits_and_restores` | server ignores 2 pings then answers → `KeepAliveTimeout { error_count: 1 }`, `… 2`, then `KeepAliveRestored` |
| `keepalive_forces_reconnect` | server never answers, `max_fail_time` 300 ms → the client re-dials past the deadline (assert a **second** TCP accept on the listener) |
| `auto_reconnect_redials_after_a_drop` | server drops the socket → second connection accepted, `Connected` emitted twice, device row reused (same `noise_key`) |
| `auto_reconnect_gives_up_after_max_attempts` | server refuses (`failure` 500) → `ConnectFailure` after `max_attempts`, `reconnect_errors == max_attempts` |
| `auto_reconnect_disabled_does_not_redial` | `enable_auto_reconnect = false` → no second accept, no `Disconnected` event |
| `local_disconnect_never_reconnects` | `disconnect()` after a drop-triggered reconnect → still exactly one extra accept total, `expected_disconnect` suppresses the loop |
| `logout_does_not_reconnect` | `logout()` → device deleted, zero further accepts, no `Disconnected` |

### 6.4 Test-count expectations

Engine baseline **183**. This half should add roughly **+38** (13 A + 9 B + 8 C + a few helpers) — report the
real number; `cargo test --workspace` must stay green on Linux/Windows/macOS.

### 6.5 Go golden vectors (only if a transcript is truly needed)

`pair-device` / `pair-success` bytes can be generated exactly as the handshake vector was: a throwaway
module `whatsmeow/zz-phase3-vector/` run with `go run ./zz-phase3-vector`, paste the numbers as constants,
then **delete the directory**. Never commit it, never modify `whatsmeow/` otherwise. Prefer building the
nodes with `binary::marshal` in the test over adding another golden blob.

---

## 7. Gates (run after each task, fix before moving on)

```bash
cd engine && cargo fmt --all --check \
  && cargo clippy --workspace --all-targets -- -D warnings \
  && cargo test --workspace
cd client/src-tauri && cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings
cd client && npx ng build && npx ng test --watch=false --browsers=ChromeHeadless
npm run specta   # no diff (only if WaEvent changed)
```

`cargo test --workspace -- --test-threads=1` when SQLite contention shows up.

---

## 8. Rules for this half

* Never delete existing code, never touch `whatsmeow/`, never edit files outside the §2.1 ownership list.
* MPL-2.0 header + the three-line boilerplate on every ported Rust file.
* English only — code, comments, log lines, tests, docs. No i18n.
* No `unwrap`/`expect`/`panic!` outside `#[cfg(test)]`; `ClientError` carries every failure path.
* Phase 4 (messaging, libsignal, pre-key upload, app state, media) is out of scope — leave
  `TODO(Phase 4)` at the exact call sites named above (`prekeys.go`, `StoreLIDPNMapping`, identities).
* `WaEvent` change → `API-WHATSAPP.md` §2 + `npm run specta` + coordinate with the frontend half
  (`whatsapp.service.ts` `handleEvent()`), see Tasks D–E below.
* Store change → nothing here changes the schema; `Device` fields are already persisted.

---

## 9. Report template (fill after Tasks A–C)

| Task | Files touched | Key decisions / deferred |
|------|---------------|--------------------------|
| A — `Client` + node loop | `wa-core/src/client.rs`, `client/session.rs`, `client/connection_events.rs`, `client/testutil.rs`, `client/tests.rs`, `types/jid.rs`, `types/events.rs` | device behind `Mutex`; `EventSink` closure; `CancellationToken` for `expectedDisconnect`; retryFrame deferral if any |
| B — pairing | `wa-core/src/client/pair.rs`, `types/events.rs`, `API-WHATSAPP.md` §2 | QR emitted as a full code list (rotation in the UI); `rotate_adv_secret` persists; prekeys/identity TODOs |
| C — keepalive + reconnect | `wa-core/src/client.rs` (spawn sites), `Cargo.toml` | capped exponential backoff numbers; `auto_reconnect` disabled on logout/local disconnect |

Also report: the final `Client` API (signature list), the Go function each Rust function ports, the events
emitted per failure path (a table), the test counts per suite (engine was 183), everything deferred with the
reason, and the exact commands to verify independently:

```bash
cd engine && cargo test --workspace -- --nocapture
cd engine && cargo test -p wa-core client:: -- --nocapture
cd engine && cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings
```

---

# Part 2 — server + frontend (Tasks D + E)

## Current state — server + frontend (complements the wa-core table above)

```
README.md → ARCHITECTURE.md → API-WHATSAPP.md → AGENTS.md → TODO.md (Phase 3)
```

Current baseline (Phase 2 done, Phase 3 not started):

- `engine/crates/bebok-server/src/connection.rs` — `Connection` / `ConnectionState` (`disconnected|connecting|connected|logged_out`) / `ConnectionStatus { state, connected, loggedIn, error, attempts }`. `POST /connect` dials + Noise handshakes in a background `tokio::spawn` and returns the current `ConnectionStatus` immediately; progress via `WaEvent::Connected` / `WaEvent::ConnectFailure` on `GET /events` (SSE). Holds the `NoiseSocket` and drops every incoming frame (frame-dropping closure). `disconnect()` is local-only. `/disconnect` and `/logout` still `501`.
- `engine/crates/bebok-server/src/routes/mod.rs` — routing table doc + `router()` with `GET /`, `/version`, `/config` (redacted), `/events` (SSE), `/connected`, `/logged_in`, `POST /connect`, `POST /disconnect|/logout → not_yet()`, `GET /search`, `/chats*`, `/debug/seed`. `auth::require_token` (`Authorization: Bearer` or `?token=`, `OPTIONS` bypass, constant-time compare) applied as middleware + `CorsLayer`.
- `engine/crates/bebok-server/src/state.rs` — `AppState { config, store: Arc<Store>, container: Container, events: broadcast::Sender<WaEvent>(2048), connection: Connection }`, `attach()` via `OnceLock<SharedState>`.
- `client/src/app/core/services/whatsapp.service.ts` — `ConnectionState = 'disconnected'|'qr'|'connected'|'logged_out'`, `qrCodes = signal<string[]>([])`, `connectionState`, `engineStatus`, `handleEvent()` for `qr`/`pair_success`/`connected`/`disconnected`/`logged_out`/`connect_failure`/`history_export_progress`, `connect()` → `invoke('connect')`, `start()` pings `/version` + subscribes to `wa-event` (SSE in browser, Tauri `listen` on desktop).
- `client/src/app/core/services/tauri.service.ts` — `COMMAND_ROUTES` maps `connect|disconnect|logout|is_connected|is_logged_in|version|get_config|list_chats|search_messages|get_chat_stats|delete_chat_messages` to HTTP; `NATIVE_COMMANDS = {engine_ready, export_chat_history}` go to Tauri IPC; `WA_EVENT = 'wa-event'` via `EventSource` (`?token=` query) with `sseState`.
- `client/src/app/app.routes.ts` — `''` (chat-list), `'chat/:jid'`, `'settings'`, `'about'`, `'browser-view'` (bare), `** → ''`. No `auth` route yet.
- `client/src/app/features/` — `about/`, `browser-view/`, `chats/` (`chat-list`, `chat`), `search/` (`search-bar`), `settings/` — **no `auth/`** component.

---

## Task D — `bebok-server`: real session endpoints delegating to `wa-core::Client`

### D1 — Refactor `connection.rs` to delegate to `Client`

- Replace the hand-rolled dial/handshake/background-task in `Connection` with a thin wrapper around `wa_core::client::Client` (introduced in Task A). `Connection` keeps:
  - the `ConnectionStatus` JSON shape exactly (`state: string`, `connected: bool`, `loggedIn: bool`, `error: string|null`, `attempts: number` — camelCase via `#[serde(rename_all="camelCase")]`);
  - the "return before the session settles" contract: `POST /connect` returns the current `ConnectionStatus` synchronously; the dial + handshake + node loop run in the background and report via `WaEvent` on `GET /events`;
  - the `attempts` counter and `error` text.
- Drive `state`/`connected`/`logged_in` from `Client::is_connected()` / `Client::is_logged_in()` and from `WaEvent` (`Connected`, `Qr`, `PairSuccess`, `ConnectFailure`, `Disconnected`, `LoggedOut`). Delete the frame-dropping closure — the real `handler_queue_loop` (Task A) handles frames.
- Keep `Connection::new()` / `attach(SharedState)` / `status()` / `is_connected()` signatures so `routes/mod.rs` and `state.rs` need minimal churn. Internally `Connection` owns `Option<Arc<Client>>` (or `Arc<Mutex<Option<Client>>>`) behind the existing `Mutex<Inner>`.
- `Client` construction: `Client::from_container(&container)` or `Client::new(device, config)` using `container.get_first_device()` (fresh unsaved device when empty → QR path). Pass `WaVersion`/`build_client_payload`, `wa_url`, `handshake_timeout`, and the `broadcast::Sender<WaEvent>` from `AppState`.

### D2 — `POST /disconnect`

- Handler `disconnect(State(s))` → `s.connection.disconnect().await` → `Json<ConnectionStatus>`.
- Semantics: Go `Client::Disconnect` — close the `NoiseSocket` (`stop(true, false)`), clear `responseWaiters`, state → `disconnected`, **do not emit** `WaEvent::Disconnected`. Idempotent.
- Token guard required (via `auth::require_token` middleware — already applied globally, just ensure the route is inside the guarded router).

### D3 — `POST /logout`

- Handler `logout(State(s))` → `s.connection.logout().await` → `Json<ConnectionStatus>`.
- Semantics: Go `Client::Logout` — send `remove-companion-device` IQ (best-effort; ignore timeout), `container.delete_device()` / `Device::delete()`, state → `logged_out`, `enable_auto_reconnect = false`, emit `WaEvent::LoggedOut { on_connect: false, reason: 401 }` (or the Go reason code). Subsequent `POST /connect` starts a fresh QR flow.
- Token guard required.

### D4 — `GET /logged_in`

- Must report persisted state: `!container.device_jids().await?.is_empty()` (or `Client::is_logged_in()` after delegation). Currently it does, but after D1 it must delegate to `Client` so the in-memory and persisted views agree. Keep the `Result<Json<bool>, Response>` error mapping (`500` on store error).

### D5 — Routing table + docs + specta

- Update the doc comment at the top of `routes/mod.rs` (the `| Method | Path | Handler |` table) to list `POST /disconnect → disconnect` and `POST /logout → logout` (no longer `not_yet`).
- Ensure `router()` registers `.route("/disconnect", post(disconnect))` and `.route("/logout", post(logout))` inside the `auth::require_token` + `CorsLayer` stack.
- Update `API-WHATSAPP.md` §1.2 (transport table) and §3.1 (auth/connection table): mark `disconnect` and `logout` as ✅, document `ConnectionStatus` shape, and note the token guard (`Authorization: Bearer` or `?token=`) on every new route.
- If any wire type changes (`WaEvent` already has the needed variants; `ConnectionStatus` is unchanged), run `npm run specta` (`engine/crates/bebok-server/src/bin/export-types.rs` → `client/src/app/types/specta.ts`) and commit the regenerated file. `npx ng build` must stay clean.

### D6 — Error handling

- Extend `ClientError` (thiserror) if needed; no `unwrap`/`panic!`/`expect` outside tests. Map `ClientError` to `ConnectionStatus.error` text via the existing `describe()` helper (special-case `CertError::Expired|NotValidYet` with the clock hint).

---

## Task E — Frontend: the QR screen

### E1 — New standalone component `client/src/app/features/auth/qr.component.ts`

- Standalone, `ChangeDetectionStrategy.OnPush`, signals only. Selector `app-qr` (or `ngwhatsapp-qr` — match the existing `app-*` prefix).
- Inputs from `WhatsappService` (inject, do not duplicate state):
  - `connectionState = whatsapp.connectionState` (`'disconnected'|'qr'|'connected'|'logged_out'`);
  - `qrCodes = whatsapp.qrCodes`;
  - `engineStatus` / `engineError` for the failure message;
  - `exportProgress` not needed here.
- Rendering:
  - `disconnected` / `connecting` → spinner + "Connecting…" + retry button (`whatsapp.connect()`).
  - `qr` → render the first code as QR (use the `qrcode` npm lib already in `client/package.json` or `qrcode-generator`; generate a `data:image/png;base64,…` URL in an `effect`/`computed` and bind to `<img>`). Show rotation: codes rotate every 60 s (first) / 20 s (rest) — the service already holds the array, the component just advances the index on a timer or when `qrCodes` changes. Show an expiry/progress bar (countdown to next rotation) and a "Scan with WhatsApp → Linked devices" hint. English only.
  - `connected` → success checkmark + "Connected" + link to `''` (chats).
  - `logged_out` → "Logged out" + `error` text + "Connect again" button.
  - Any `connect_failure` → show `engineError()` / `ConnectionStatus.error` text.
- No redesign of `chat-list` / `chat` — this component is isolated.

### E2 — Route + wiring

- Add to `client/src/app/app.routes.ts`:
  ```ts
  { path: 'auth', title: 'Pair device', loadComponent: () => import('./features/auth/qr.component').then(m => m.QrComponent) }
  ```
  Keep `** → ''` last. Lazy `loadComponent` like the other features.
- Wire auto-redirect: when `connectionState() === 'qr'` the app should show the QR screen. Simplest: in `app.ts` (root component) `effect(() => { if (whatsapp.connectionState() === 'qr') router.navigateByUrl('/auth'); })` or a `canActivate` guard on `''` that redirects to `/auth` when `qr`. Do not break the existing `browser-view` bare route. Document the chosen approach in the PR description.
- Ensure `TauriService.COMMAND_ROUTES` already maps `connect|disconnect|logout|is_connected|is_logged_in` — no change needed unless the command names change (then update `whatsapp.service.ts` and `COMMAND_ROUTES` together).

### E3 — Specs `client/src/app/features/auth/qr.component.spec.ts`

- Next to the component, like `chat.component.spec.ts` / `search-bar.component.spec.ts`. Mock `WhatsappService` (and `TauriService` if needed) the same way `whatsapp.service.spec.ts` does (`fakeDeps`, `FakeEventSource`, `TestBed`).
- Cases:
  - renders the QR image when `qrCodes = ['code-a']` and `connectionState = 'qr'`;
  - advances to the next code on rotation (timer or input change);
  - shows the expiry/progress indicator;
  - shows the failure message when `engineError` is set / `connect_failure` emitted;
  - hides the QR and shows "Connected" when `connectionState` becomes `'connected'`;
  - "Connect again" button calls `whatsapp.connect()`.

---

## Tests for server / frontend (all offline, no WhatsApp network)

### Server (`engine/crates/bebok-server`)

- `POST /connect` — still returns `ConnectionStatus` immediately (before the session settles), requires the token (`401` without it), second call while `connecting|connected` is a no-op (same `attempts`), emits `connected` / `connect_failure` on `GET /events`.
- `POST /disconnect` — drops the session: `GET /connected → false`, `GET /status` → `disconnected`, no `WaEvent::Disconnected` emitted, idempotent, token guard (`401` without token).
- `POST /logout` — state → `logged_out`, `GET /logged_in → false`, device row deleted (`container.device_jids().is_empty()`), auto-reconnect disabled (no re-dial after a drop), token guard.
- `GET /logged_in` — `false` when the store is empty, `true` after a `PairSuccess`-persisted device (roundtrip via `put_device` + `get_first_device`), `500` on store error is mapped to `{ error }`.
- `GET /config` — still redacts `token` as `"<redacted>"`.
- Every socket test binds `127.0.0.1:0` and is wrapped in `tokio::time::timeout` so a broken test fails fast. No network.

Existing suites to keep green: `bebok-server/tests/http.rs` (`/connect` token/early-return/`connect_failure` on `/events`/no stacking), `connection.rs` unit tests (`states_match_the_frontend_union`, `status_uses_the_documented_json_keys`, `a_detached_handle_fails_instead_of_hanging`, `disconnect_is_idempotent`).

### Frontend (`client`)

- `qr.component.spec.ts` — as listed in E3 (render QR, rotation, expiry, failure message, hide on `connected`).
- `whatsapp.service.spec.ts` — extend the existing `drives connectionState and qrCodes from wa-event` case to cover `pair_success` → `connected` + `qrCodes` cleared, `keep_alive_restored` → `connected`, `stream_replaced` → `disconnected`, and `connect()` → `invoke('connect')` (mock `TauriService.invoke`).
- `tauri.service.spec.ts` — `COMMAND_ROUTES` for `disconnect|logout|is_connected|is_logged_in` map to the correct `method`/`path`/`query`, `WA_EVENT` via SSE uses `?token=` query, `NATIVE_COMMANDS` still only `{engine_ready, export_chat_history}`.
- `app.routes.spec.ts` / `app.spec.ts` — `'/auth'` lazy-loads `QrComponent`, `**` still redirects to `''`, `browser-view` still `data.bare`.

---

## Rules — Tasks D + E (in addition to the backend rules)

- Never delete existing code or the `whatsmeow/` reference directory. Never edit files outside the ownership list of the task you are on.
- Keep the MPL-2.0 header on every ported Rust file.
- English only — UI strings, code, comments, tests, docs. No i18n.
- Phase 4 (messaging, libsignal sessions, pre-key upload, app state, media) is out of scope — leave documented `TODO(Phase 4)` comments where the wiring will go.
- IPC change → regenerate `specta` + update `whatsapp.service.ts` / `tauri.service.ts` (`COMMAND_ROUTES`).
- Store change → migration + `PutDevice`/`GetDevice` test.
- Frontend change → ask before verifying in the browser (dev servers: `bash` with `background:true` + `ready_port`; engine + `ng serve`).

---

## Report (fill after Tasks D + E)

Per task: every file touched, the `Client` API you settled on, which Go functions each Rust function ports, the events emitted per failure path, test counts per suite (engine was 183, client 51 before this phase), anything deferred with the reason, and the exact commands to verify it independently.

| Task | Files touched | Key decisions / deferred |
|------|---------------|--------------------------|
| D — bebok-server | `engine/crates/bebok-server/src/connection.rs`, `routes/mod.rs`, `state.rs`, `auth.rs` (if needed), `API-WHATSAPP.md`, `client/src/app/types/specta.ts` (if regenerated) | Delegation to `wa_core::Client`; `ConnectionStatus` shape unchanged; `POST /disconnect|/logout` semantics; token guard on new routes |
| E — frontend QR | `client/src/app/features/auth/qr.component.ts`, `qr.component.spec.ts`, `app.routes.ts`, `app.ts` (redirect wiring), `core/services/whatsapp.service.ts` (if extended), `core/services/tauri.service.ts` (if `COMMAND_ROUTES` changed) | QR lib choice; rotation interval; redirect strategy (`effect` vs guard) |

Verification commands (run from repo root):

```bash
cd engine && cargo test --workspace -- --nocapture
cd client && npx ng test --watch=false --browsers=ChromeHeadless
cd client && npx ng build
npm run specta && git diff --exit-code client/src/app/types/specta.ts
```

## Environment notes (save yourself the detour)

- Delegated sub-agents are unreliable here: `code1` fails with 402 `billing_not_configured`, the
  default `code` agent returns 429, and `code2` gets aborted around 120 s — but its partial work is
  often usable, so **read `task_report` before re-tasking anything**. Prefer doing the work yourself
  in small verified steps.
- Crate fetching works (`tokio-util`, `futures`, etc. can be added this way) and Go is available for
  vector generation.
- `cargo test` on this workspace is fast; keep `--test-threads=1` in mind if SQLite contention shows up.
