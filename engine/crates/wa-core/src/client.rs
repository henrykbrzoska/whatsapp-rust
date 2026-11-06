// Ported from the Go client `handshake.go` and `client.go` (the handshake step),
// © Tulir Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The WhatsApp client — Phase 2 scope: the Noise handshake
//! (`Go handshake.go:doHandshake` + `verifyServerCert`), plus the Phase 3
//! session state machine (`Go client.go` connection machinery,
//! `request.go`, `connectionevents.go` — see [`Client`]).
//!
//! [`do_handshake`] drives the complete `Noise_XX_25519_AESGCM_SHA256`
//! transcript over an already-open [`FrameSocket`] and returns the
//! [`NoiseSocket`] that seals and opens every later frame:
//!
//! ```text
//! client                                  server
//!   -> ClientHello { ephemeral }              (unencrypted)
//!   <- ServerHello { ephemeral, static, payload }
//!   -> ClientFinish { static = E(noisePub), payload = E(ClientPayload) }
//! ```
//!
//! The order of the hash updates, key mixes and encryptions is Go's, line for
//! line — a single reordering produces different session keys and the server
//! closes the connection. See [`crate::socket::noise_handshake`] for why the
//! state machine is hand-rolled rather than delegated to a Noise library.
//!
//! [`Client`] owns the encrypted socket after the handshake, the
//! `responseWaiters` map, the handler queue (`mpsc::channel(2048)`, exactly
//! Go's `handlerQueueSize`) and the pairing/keepalive wiring in the
//! submodules below.

#[cfg(test)]
#[path = "client/client_tests.rs"]
pub(crate) mod client_tests;
pub(crate) mod connection_events;
pub(crate) mod session;

pub mod ack;
pub mod appstate;
pub mod download;
pub mod fb_message;
pub mod group;
pub mod history_blob;
pub mod history_sync;
pub mod mediaconn;
pub mod mediaretry;
pub mod message;
pub mod newsletter;
pub mod notification;
pub mod pair;
pub mod prekeys;
pub mod presence;
pub mod privacysettings;
pub mod receipt;
pub mod retry;
pub mod retry_receipt;
pub mod send;
pub mod upload;
pub mod user;
pub mod web_message;

pub use connection_events::reason;
pub use session::{InfoQuery, IqKind, is_auth_error_disconnect, is_disconnect_node};

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prost::Message;
use rand::RngCore;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use wa_proto::wa_cert::CertChain;
use wa_proto::wa_cert::cert_chain::noise_certificate::Details as CertDetails;
use wa_proto::wa_web_protobufs_wa6::ClientPayload;
use wa_proto::wa_web_protobufs_wa6::HandshakeMessage;
use wa_proto::wa_web_protobufs_wa6::handshake_message::{ClientFinish, ClientHello};

use crate::crypto::keys::{KeyPair, verify_signature};
use crate::socket::frame_socket::{FrameSocket, FrameSocketError, WA_CONN_HEADER};
use crate::socket::noise_handshake::{KEY_SIZE, NOISE_START_PATTERN, NoiseError, NoiseHandshake};
use crate::socket::noise_socket::{DisconnectHandler, FrameHandler, NoiseSocket};
use crate::store::device::Device;
use crate::store::sqlite::Container;
use crate::types::jid::Jid;

/// Go `types.ServerJID` — the bare `s.whatsapp.net` server JID.
pub fn server_jid() -> Jid {
    Jid::new("", crate::types::jid::DEFAULT_USER_SERVER)
}

/// `Go handshake.go:NoiseHandshakeResponseTimeout` — how long the server
/// may take to answer the `ClientHello`.
pub const NOISE_HANDSHAKE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(20);

/// Go `handlerQueueSize` — the handler queue holds at most 2048 nodes.
pub const HANDLER_QUEUE_SIZE: usize = 2048;

/// Go `defaultRequestTimeout` — how long `sendIQ` waits for a response.
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(75);

/// `Go handshake.go:WACertIssuerSerial` — the serial of the intermediate
/// certificate WhatsApp's CA issues.
pub const WACERT_ISSUER_SERIAL: u32 = 0;

/// `Go handshake.go:WACertPubKey` — the Curve25519 key that signs the
/// intermediate Noise certificate chain.
pub const WACERT_PUB_KEY: [u8; 32] = [
    0x14, 0x23, 0x75, 0x57, 0x4d, 0x0a, 0x58, 0x71, 0x66, 0xaa, 0xe7, 0x1e, 0xbe, 0x51, 0x64, 0x37, 0xc4, 0xa2, 0x8b,
    0x73, 0xe3, 0x69, 0x5c, 0x6c, 0xe1, 0xf7, 0xf9, 0x54, 0x5d, 0xa8, 0xee, 0x6b,
];

/// Which side of the certificate chain failed verification (Go reports it in
/// the error text: "intermediate cert …" / "leaf cert …").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertKind {
    Intermediate,
    Leaf,
}

impl std::fmt::Display for CertKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Intermediate => "intermediate",
            Self::Leaf => "leaf",
        })
    }
}

/// Failures of [`verify_server_cert`] — one variant per Go `return fmt.Errorf`.
#[derive(Debug, thiserror::Error)]
pub enum CertError {
    #[error("failed to unmarshal noise certificate: {0}")]
    UnmarshalChain(prost::DecodeError),
    #[error("missing parts of noise certificate")]
    MissingParts,
    #[error("unexpected length of intermediate cert signature {0} (expected 64)")]
    IntermediateSignatureLength(usize),
    #[error("unexpected length of leaf cert signature {0} (expected 64)")]
    LeafSignatureLength(usize),
    #[error("failed to verify intermediate cert signature")]
    IntermediateSignature,
    #[error("failed to verify leaf cert signature")]
    LeafSignature,
    #[error("failed to unmarshal noise certificate details: {0}")]
    UnmarshalDetails(prost::DecodeError),
    #[error("unexpected intermediate issuer serial {got} (expected {expected})")]
    IntermediateIssuerSerial { got: u32, expected: u32 },
    #[error("unexpected leaf issuer serial {got} (expected {expected})")]
    LeafIssuerSerial { got: u32, expected: u32 },
    #[error("unexpected length of intermediate cert key {0} (expected 32)")]
    IntermediateKeyLength(usize),
    #[error("cert key doesn't match decrypted static")]
    KeyMismatch,
    #[error("{kind} cert certificate not valid yet (current time {now} is before {not_before})")]
    NotValidYet { kind: CertKind, now: u64, not_before: u64 },
    #[error("{kind} cert certificate expired (current time {now} is after {not_after})")]
    Expired { kind: CertKind, now: u64, not_after: u64 },
}

/// Errors of [`do_handshake`] — one variant per Go error path.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("failed to marshal {what}: {source}")]
    Marshal { what: &'static str, source: prost::EncodeError },
    #[error("failed to unmarshal {what}: {source}")]
    Unmarshal { what: &'static str, source: prost::DecodeError },
    #[error("failed to send {what}: {source}")]
    Send { what: &'static str, source: FrameSocketError },
    #[error("timed out waiting for handshake response")]
    HandshakeTimeout,
    #[error("frame socket closed while waiting for handshake response")]
    SocketClosed,
    #[error("missing parts of handshake response")]
    MissingHandshakeParts,
    #[error("unexpected length of server static plaintext {0} (expected 32)")]
    UnexpectedStaticLength(usize),
    #[error("failed to decrypt {what}: {source}")]
    Decrypt { what: &'static str, source: NoiseError },
    #[error("failed to mix {what} into the handshake key: {source}")]
    Mix { what: &'static str, source: NoiseError },
    #[error("failed to verify server cert: {0}")]
    Cert(#[from] CertError),
    #[error("failed to create noise socket: {0}")]
    Noise(#[from] NoiseError),
    #[error("not connected")] // Go `ErrNotConnected`.
    NotConnected,
    #[error("already connected")] // Go `ErrAlreadyConnected`.
    AlreadyConnected,
    #[error("not logged in")] // Go `ErrNotLoggedIn`.
    NotLoggedIn,
    #[error("the device was deleted")] // Go `store.ErrDeviceDeleted`.
    DeviceDeleted,
    #[error("timed out waiting for {what} ({id})")] // Go `ErrIQTimedOut`.
    RequestTimeout { what: &'static str, id: String },
    #[error("connection lost while waiting for {what} ({id})")] // Go `DisconnectedError`.
    Disconnected { what: &'static str, id: String },
    #[error("unexpected iq response for {id}: {node}")] // Go `IQError`.
    Iq { id: String, node: String },
    #[error("failed to encode/decode a node: {0}")] // Go `waBinary.Marshal/Unmarshal`.
    Binary(#[from] crate::binary::BinaryError),
    #[error("store error: {0}")] // Go `PairDatabaseError`.
    Store(#[from] anyhow::Error),
    #[error("pairing failed: {0}")] // Go `PairError` event / `sendPairError` paths.
    Pair(String),
    #[error("failed to parse an incoming message node: {0}")] // Go `message.go` attribute errors.
    MessageParse(String),
    #[error("invalid pre-key response: {0}")] // Go `prekeys.go` node parsing errors.
    PreKey(String),
    #[error("recipient JID contains a device (use a peer message for own devices)")] // Go `ErrRecipientADJID`.
    RecipientAdjId,
    #[error("timed out waiting for the message send response")] // Go `ErrMessageTimedOut`.
    MessageTimedOut,
    #[error("no signal session with {0}")] // Go `ErrNoSession`.
    NoSession(String),
    #[error("server returned error code {0} for the message")] // Go `ErrServerReturnedError`.
    ServerReturnedError(i32),
    #[error("unknown server {0}")] // Go `ErrUnknownServer`.
    UnknownServer(String),
    #[error("unknown/invalid inline bot JID")] // Go `ErrInvalidInlineBotID`.
    InvalidInlineBotId,
    #[error("no permission to send this message in the group")] // Go `ErrNoPermissionInGroup`.
    NoPermissionInGroup,
    #[error("signal error: {0}")] // Signal-layer failures (`crypto/signal`).
    Signal(String),
    /// A per-device failure while fanning out a message. Never fatal for the
    /// whole send (Go logs and continues).
    #[error("failed to encrypt for {device}: {error}")]
    DeviceEncryption { device: String, error: String },
    #[error("missing <{tag}> in {parent}")] // Go `ElementMissingError`.
    MissingElement { tag: &'static str, parent: &'static str },
    #[error("missing attribute {attr} in <{tag}>")] // Go `AttrGetter` errors.
    MissingAttribute { tag: String, attr: &'static str },
    #[error("invalid argument: {0}")] // Go `fmt.Errorf` argument errors.
    InvalidArgument(&'static str),
    #[error("crypto error: {0}")] // `crypto::` failures surfaced to callers.
    Crypto(String),
    /// A media download (attachment or history blob) failed — the HTTP side,
    /// the integrity checks or the decode (`client/download.rs`).
    #[error("media: {0}")]
    Media(String),
}

/// Knobs of [`do_handshake_with`]. [`Default`] is production behaviour; tests
/// shorten the timeout and swap the certificate authority.
#[derive(Debug, Clone)]
pub struct HandshakeOptions {
    /// `NoiseHandshakeResponseTimeout` — how long to wait for the `ServerHello`.
    pub response_timeout: Duration,
    /// The CA that signs the intermediate certificate. Production always uses
    /// [`WACERT_PUB_KEY`]; tests and the Go golden transcript use their own key.
    pub cert_authority: [u8; 32],
    /// When `true`, also run `checkCertValidity` (the `notBefore`/`notAfter`
    /// window). Go always checks; the golden transcript from the reference
    /// implementation stays inside its window, so this is only a test escape
    /// hatch for otherwise deterministic fixtures.
    pub check_cert_validity: bool,
}

impl Default for HandshakeOptions {
    fn default() -> Self {
        Self {
            response_timeout: NOISE_HANDSHAKE_RESPONSE_TIMEOUT,
            cert_authority: WACERT_PUB_KEY,
            check_cert_validity: true,
        }
    }
}

/// `Client.doHandshake` — run the WhatsApp Noise handshake over `frames`.
///
/// `ephemeral` is the throwaway key pair of this handshake, `noise` the
/// device's persistent Noise key (Go: `cli.Store.NoiseKey`), and
/// `client_payload` what the `ClientFinish` carries (see
/// [`crate::types::version::build_client_payload`]).
///
/// Incoming binary frames after the handshake are dropped with a warning: the
/// handler queue that feeds `binary::unpack` belongs to Phase 3.
pub async fn do_handshake(
    frames: FrameSocket,
    ephemeral: &KeyPair,
    noise: &KeyPair,
    client_payload: ClientPayload,
) -> Result<NoiseSocket, ClientError> {
    do_handshake_with(
        frames,
        ephemeral,
        noise,
        client_payload,
        HandshakeOptions::default(),
        Box::new(|_frame| {
            // TODO(Phase 3): push into the handler queue and `binary::unpack`.
            tracing::debug!("received a frame before the client state machine exists; dropping it");
        }),
        None,
    )
    .await
}

/// [`do_handshake`] with explicit options and frame/disconnect handlers.
pub async fn do_handshake_with(
    frames: FrameSocket,
    ephemeral: &KeyPair,
    noise: &KeyPair,
    client_payload: ClientPayload,
    options: HandshakeOptions,
    on_frame: FrameHandler,
    on_disconnect: Option<DisconnectHandler>,
) -> Result<NoiseSocket, ClientError> {
    // 1. Start the state machine with the WA connection header as the prologue.
    let mut nh = NoiseHandshake::new();
    nh.start(NOISE_START_PATTERN, &WA_CONN_HEADER)?;
    nh.authenticate(&ephemeral.public);

    // 2. ClientHello { ephemeral }.
    let hello = HandshakeMessage {
        client_hello: Some(ClientHello { ephemeral: Some(ephemeral.public.to_vec()), ..Default::default() }),
        ..Default::default()
    };
    let data = hello.encode_to_vec();
    frames.send_frame(data).await.map_err(|source| ClientError::Send { what: "handshake message", source })?;

    // 3. Wait for the ServerHello (Go: select on fs.Frames / ctx.Done / timer).
    let response = match tokio::time::timeout(options.response_timeout, frames.recv()).await {
        Ok(Some(response)) => response,
        Ok(None) => return Err(ClientError::SocketClosed),
        Err(_) => return Err(ClientError::HandshakeTimeout),
    };
    let response = HandshakeMessage::decode(response.as_slice())
        .map_err(|source| ClientError::Unmarshal { what: "handshake response", source })?;

    // 4. Read and validate the three required parts.
    let server_hello = response.server_hello.unwrap_or_default();
    let server_ephemeral = server_hello.ephemeral.unwrap_or_default();
    let server_static_ciphertext = server_hello.r#static;
    let certificate_ciphertext = server_hello.payload;
    let server_ephemeral_key = <[u8; KEY_SIZE]>::try_from(server_ephemeral.as_slice());
    let (Ok(server_ephemeral_key), Some(server_static_ciphertext), Some(certificate_ciphertext)) =
        (server_ephemeral_key, server_static_ciphertext, certificate_ciphertext)
    else {
        return Err(ClientError::MissingHandshakeParts);
    };

    // 5. Authenticate the server ephemeral and mix its DH secret in.
    nh.authenticate(&server_ephemeral_key);
    nh.mix_shared_secret_into_key(&ephemeral.private, &server_ephemeral_key)
        .map_err(|source| ClientError::Mix { what: "server ephemeral key", source })?;

    // 6. Decrypt the server static key and mix its DH secret in.
    let static_decrypted = nh
        .decrypt(&server_static_ciphertext)
        .map_err(|source| ClientError::Decrypt { what: "server static ciphertext", source })?;
    let static_key = <[u8; KEY_SIZE]>::try_from(static_decrypted.as_slice())
        .map_err(|_| ClientError::UnexpectedStaticLength(static_decrypted.len()))?;
    nh.mix_shared_secret_into_key(&ephemeral.private, &static_key)
        .map_err(|source| ClientError::Mix { what: "server static key", source })?;

    // 7. Decrypt and verify the signed Noise certificate chain.
    let cert_decrypted = nh
        .decrypt(&certificate_ciphertext)
        .map_err(|source| ClientError::Decrypt { what: "noise certificate ciphertext", source })?;
    verify_server_cert_with(&options.cert_authority, &cert_decrypted, &static_decrypted, options.check_cert_validity)?;

    // 8. Encrypt our Noise public key, then mix the server ephemeral DH secret in.
    let encrypted_noise_pub = nh.encrypt(&noise.public)?;
    nh.mix_shared_secret_into_key(&noise.private, &server_ephemeral_key)
        .map_err(|source| ClientError::Mix { what: "noise private key", source })?;

    // 9. ClientFinish { static, payload }.
    let client_payload_bytes = client_payload.encode_to_vec();
    let encrypted_client_payload = nh.encrypt(&client_payload_bytes)?;
    let finish = HandshakeMessage {
        client_finish: Some(ClientFinish {
            r#static: Some(encrypted_noise_pub),
            payload: Some(encrypted_client_payload),
            ..Default::default()
        }),
        ..Default::default()
    };
    let data = finish.encode_to_vec();
    frames.send_frame(data).await.map_err(|source| ClientError::Send { what: "handshake finish message", source })?;

    // 10. Derive the session keys and wrap the frame socket.
    let (write_key, read_key) = nh.finish_keys()?;
    let socket = NoiseSocket::new(std::sync::Arc::new(frames), &write_key, &read_key, on_frame, on_disconnect).await?;
    Ok(socket)
}

/// `verifyServerCert` — validate the signed Noise certificate chain against
/// [`WACERT_PUB_KEY`] and the decrypted server static key.
pub fn verify_server_cert(cert_decrypted: &[u8], static_decrypted: &[u8]) -> Result<(), CertError> {
    verify_server_cert_with(&WACERT_PUB_KEY, cert_decrypted, static_decrypted, true)
}

/// [`verify_server_cert`] with an explicit CA key and an optional validity
/// window check (the CA is a test key in unit tests).
pub fn verify_server_cert_with(
    cert_authority: &[u8; KEY_SIZE],
    cert_decrypted: &[u8],
    static_decrypted: &[u8],
    check_validity: bool,
) -> Result<(), CertError> {
    let chain = CertChain::decode(cert_decrypted).map_err(CertError::UnmarshalChain)?;

    let intermediate = chain.intermediate.unwrap_or_default();
    let leaf = chain.leaf.unwrap_or_default();
    let (Some(intermediate_details_raw), Some(intermediate_signature), Some(leaf_details_raw), Some(leaf_signature)) =
        (intermediate.details, intermediate.signature, leaf.details, leaf.signature)
    else {
        return Err(CertError::MissingParts);
    };
    let intermediate_signature = signature64(&intermediate_signature, CertError::IntermediateSignatureLength)?;
    let leaf_signature = signature64(&leaf_signature, CertError::LeafSignatureLength)?;

    if !verify_signature(cert_authority, &intermediate_details_raw, &intermediate_signature) {
        return Err(CertError::IntermediateSignature);
    }
    let intermediate_details =
        CertDetails::decode(intermediate_details_raw.as_slice()).map_err(CertError::UnmarshalDetails)?;
    if intermediate_details.issuer_serial.unwrap_or_default() != WACERT_ISSUER_SERIAL {
        return Err(CertError::IntermediateIssuerSerial {
            got: intermediate_details.issuer_serial.unwrap_or_default(),
            expected: WACERT_ISSUER_SERIAL,
        });
    }
    let intermediate_key_bytes = intermediate_details.key.clone().unwrap_or_default();
    let intermediate_key = <[u8; KEY_SIZE]>::try_from(intermediate_key_bytes.as_slice())
        .map_err(|_| CertError::IntermediateKeyLength(intermediate_key_bytes.len()))?;

    if !verify_signature(&intermediate_key, &leaf_details_raw, &leaf_signature) {
        return Err(CertError::LeafSignature);
    }
    if check_validity {
        check_cert_validity(CertKind::Intermediate, &intermediate_details)?;
    }
    let leaf_details = CertDetails::decode(leaf_details_raw.as_slice()).map_err(CertError::UnmarshalDetails)?;
    if leaf_details.issuer_serial.unwrap_or_default() != intermediate_details.serial.unwrap_or_default() {
        return Err(CertError::LeafIssuerSerial {
            got: leaf_details.issuer_serial.unwrap_or_default(),
            expected: intermediate_details.serial.unwrap_or_default(),
        });
    }
    if leaf_details.key.as_deref() != Some(static_decrypted) {
        return Err(CertError::KeyMismatch);
    }
    if check_validity {
        check_cert_validity(CertKind::Leaf, &leaf_details)?;
    }
    Ok(())
}

fn signature64(bytes: &[u8], error: fn(usize) -> CertError) -> Result<[u8; 64], CertError> {
    <[u8; 64]>::try_from(bytes).map_err(|_| error(bytes.len()))
}

// ---------------------------------------------------------------------------
// `struct Client` — the session state machine (Phase 3)
// ---------------------------------------------------------------------------

/// The event sink: `wa-core` must not depend on `bebok-server`, so events flow
/// through a boxed closure. Emission is synchronous and must never block or
/// panic (Go's `dispatchEvent` `recover()` equivalent) — the closure is
/// responsible for that.
pub type EventSink = Arc<dyn Fn(crate::WaEvent) + Send + Sync + 'static>;

/// Knobs of the [`Client`] session (Go `Client` fields + `Config`).
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// `Config::wa_url` — `None` = the production URL.
    pub wa_url: Option<String>,
    /// Handshake knobs: timeout, CA, validity check.
    pub handshake: HandshakeOptions,
    /// Go `EnableAutoReconnect`.
    pub enable_auto_reconnect: bool,
    /// Go `AutoReconnectErrors`-driven backoff (see `ReconnectConfig`).
    pub reconnect: ReconnectConfig,
    /// Go `defaultRequestTimeout` (75 s) — how long `sendIQ` waits.
    pub request_timeout: Duration,
    /// Forwarded to `KeepAlive::new`.
    pub keepalive: crate::keepalive::KeepAliveConfig,
    /// Go `UserAgent` — the `FrameSocket` header.
    pub user_agent: Option<String>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            wa_url: None,
            handshake: HandshakeOptions::default(),
            enable_auto_reconnect: true,
            reconnect: ReconnectConfig::default(),
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            keepalive: crate::keepalive::KeepAliveConfig::default(),
            user_agent: None,
        }
    }
}

/// Capped exponential backoff for the auto-reconnect loop
/// (Go `autoReconnect`: `AutoReconnectErrors * 2 s`, capped here at 60 s so a
/// long outage does not sleep forever; 10 attempts, then `ConnectFailure`).
#[derive(Debug, Clone, Copy)]
pub struct ReconnectConfig {
    pub initial_delay: Duration,
    pub max_delay: Duration,
    pub max_attempts: u32,
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self { initial_delay: Duration::from_secs(2), max_delay: Duration::from_secs(60), max_attempts: 10 }
    }
}

impl ReconnectConfig {
    /// `min(initial_delay * 2^attempt, max_delay)`.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let shift = attempt.min(20);
        let delay = self.initial_delay.saturating_mul(1 << shift);
        delay.min(self.max_delay)
    }
}

/// `the Go client.Client` — the session state machine.
///
/// `device` sits behind a `Mutex` because pairing mutates it (`ID`/`LID`/
/// account/platform) and `Device::save` takes `&mut self`; every write holds
/// the same lock as the `save()` call, so a concurrent `is_logged_in()` never
/// sees a half-paired device. Readers use [`Client::device`] (a clone).
pub struct Client {
    device: Arc<Mutex<Device>>,
    socket: Mutex<Option<Arc<NoiseSocket>>>,
    response_waiters: Mutex<HashMap<String, oneshot::Sender<crate::binary::node::Node>>>,
    events: EventSink,
    is_logged_in: AtomicBool,
    paired: AtomicBool,
    expected_disconnect: Mutex<CancellationToken>,
    force_auto_reconnect: AtomicBool,
    reconnect_errors: AtomicU32,
    server_time_offset: AtomicI64,
    config: ClientConfig,
    unique_id: String,
    id_counter: AtomicU64,
    conn_cancel: Mutex<Option<CancellationToken>>,
    handler_queue_done: Mutex<Option<oneshot::Receiver<()>>>,
    /// Raw `ref`s of the last `pair-device` node, kept so
    /// `rotate_adv_secret` can rebuild the QR codes with the new secret.
    qr_refs: Mutex<Vec<Vec<u8>>>,
    /// The live `qr_channel()` sender, if any (Go `qrchan.go`).
    qr_tx: Mutex<Option<mpsc::Sender<pair::QRChannelItem>>>,
    /// Go `messageRetries` — retry-receipt counter per message ID.
    message_retries: Mutex<retry_receipt::MessageRetries>,
    /// Go `AutoTrustIdentity` — trust a new identity key without asking.
    auto_trust_identity: AtomicBool,
    /// Go `EnableDecryptedEventBuffer` — buffer decrypted plaintexts so a
    /// retry receipt can be answered from memory.
    enable_decrypted_event_buffer: bool,
    /// Go `uploadPreKeysLock` + `lastPreKeyUpload` (`prekeys.go`).
    last_pre_key_upload: Mutex<Option<std::time::Instant>>,
    /// Go `recentMessagesMap` — the last 512 sent messages, kept so a retry
    /// receipt can be answered by re-encrypting the same payload
    /// (`client/retry.rs`).
    recent_messages: Mutex<retry::RecentMessages>,
    /// Go `incomingRetryRequestCounter` — how often we re-sent one message.
    incoming_retry_counter: Mutex<HashMap<(String, String), i32>>,
    /// Go `pendingPhoneRerequests` — cancellable "ask the phone for the
    /// message" timers, keyed by message ID.
    pending_phone_rerequests: Mutex<HashMap<String, CancellationToken>>,
    /// Go `sessionRecreateHistory` — when a session was last dropped for a
    /// device, so `shouldRecreateSession` does not do it in a loop.
    session_recreate_history: Mutex<HashMap<Jid, std::time::Instant>>,
    /// Go `UseRetryMessageStore` — also keep sent messages in the store
    /// (`wa_retry_buffer`) so a retry receipt survives a restart.
    use_retry_message_store: AtomicBool,
    /// Go `lastRetryStoreClear` — when the retry store was last pruned.
    last_retry_store_clear: Mutex<Option<std::time::Instant>>,
    /// History-sync notifications (`client/history_sync.rs`). A `broadcast`
    /// channel rather than a `WaEvent` variant because the event enum is owned
    /// by another crate boundary — see the module docs.
    history_sync_tx: tokio::sync::broadcast::Sender<history_sync::HistorySyncNotificationEvent>,
    /// Go `groupCache` — per-group metadata (`client/group.rs`), with a TTL on
    /// top so a missed notification cannot pin a stale member list.
    group_cache: Mutex<group::GroupCache>,
    /// Go `mediaConnCache` — the CDN hosts attachments are fetched from,
    /// cached until their TTL expires (`client/mediaconn.rs`).
    media_conn_cache: Mutex<Option<mediaconn::MediaConn>>,
    /// The HTTP client media downloads share (connection pooling). Built on
    /// first use so a client that never downloads never creates one.
    media_http: std::sync::OnceLock<reqwest::Client>,
    // TODO(Phase 4+): userDevicesCache, appStateProc,
    // privacySettingsCache, tcTokenSenderTS, PreKeysUploaded.
}

impl Client {
    /// Go `NewClient` — `paired` is `device.id.is_some()`.
    pub fn new(device: Device, config: ClientConfig, events: EventSink) -> Arc<Self> {
        let paired = device.id.is_some();
        let mut id_bytes = [0u8; 2];
        rand::thread_rng().fill_bytes(&mut id_bytes);
        Arc::new(Self {
            device: Arc::new(Mutex::new(device)),
            socket: Mutex::new(None),
            response_waiters: Mutex::new(HashMap::new()),
            events,
            is_logged_in: AtomicBool::new(false),
            paired: AtomicBool::new(paired),
            expected_disconnect: Mutex::new(CancellationToken::new()),
            force_auto_reconnect: AtomicBool::new(false),
            reconnect_errors: AtomicU32::new(0),
            server_time_offset: AtomicI64::new(0),
            config,
            unique_id: format!("{}.{}-", id_bytes[0], id_bytes[1]),
            id_counter: AtomicU64::new(0),
            conn_cancel: Mutex::new(None),
            handler_queue_done: Mutex::new(None),
            qr_refs: Mutex::new(Vec::new()),
            qr_tx: Mutex::new(None),
            message_retries: Mutex::new(retry_receipt::MessageRetries::new()),
            // Go leaves both at their zero value (`AutoTrustIdentity == false`).
            auto_trust_identity: AtomicBool::new(false),
            enable_decrypted_event_buffer: false,
            last_pre_key_upload: Mutex::new(None),
            recent_messages: Mutex::new(retry::RecentMessages::new()),
            incoming_retry_counter: Mutex::new(HashMap::new()),
            pending_phone_rerequests: Mutex::new(HashMap::new()),
            session_recreate_history: Mutex::new(HashMap::new()),
            // Go leaves `UseRetryMessageStore` at its zero value.
            use_retry_message_store: AtomicBool::new(false),
            last_retry_store_clear: Mutex::new(None),
            history_sync_tx: tokio::sync::broadcast::channel(history_sync::HISTORY_SYNC_CHANNEL_CAPACITY).0,
            group_cache: Mutex::new(group::GroupCache::default()),
            media_conn_cache: Mutex::new(None),
            media_http: std::sync::OnceLock::new(),
        })
    }

    /// Like Go's `NewClient` over a store: the first device in the container,
    /// or a fresh unsaved device (→ the QR path) when the store is empty.
    pub async fn from_container(
        container: &Container,
        config: ClientConfig,
        events: EventSink,
    ) -> Result<Arc<Self>, ClientError> {
        Ok(Self::new(container.get_first_device().await?, config, events))
    }

    /// Go `dispatchEvent` — the single sink every handler emits through.
    /// The [`EventSink`] contract (synchronous, never blocks or panics) is the
    /// closure's responsibility.
    pub fn dispatch_event(&self, event: crate::WaEvent) {
        (self.events)(event);
    }

    /// Go `IsConnected` — a live socket exists and reports connected.
    pub async fn is_connected(&self) -> bool {
        match &*self.socket.lock().await {
            Some(socket) => socket.is_connected(),
            None => false,
        }
    }

    /// Go `IsLoggedIn` — true after `handleConnectSuccess`.
    pub fn is_logged_in(&self) -> bool {
        self.is_logged_in.load(Ordering::SeqCst)
    }

    /// A clone of the device, for readers (`GET /logged_in`).
    pub async fn device(&self) -> Device {
        self.device.lock().await.clone()
    }

    /// Go `WaitForConnection` — true once connected *and* logged in, false on
    /// timeout or an expected disconnect.
    pub async fn wait_for_connection(&self, timeout: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.is_connected().await && self.is_logged_in() {
                return true;
            }
            if self.is_expected_disconnect().await || tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Go `expectDisconnect` — mark the next close as local (silent).
    pub async fn expect_disconnect(&self) {
        self.force_auto_reconnect.store(false, Ordering::SeqCst);
        self.expected_disconnect.lock().await.cancel();
    }

    /// Go `isExpectedDisconnect`.
    pub async fn is_expected_disconnect(&self) -> bool {
        self.expected_disconnect.lock().await.is_cancelled()
    }

    /// Go `resetExpectedDisconnect` — a fresh token, no forced reconnect.
    pub async fn reset_expected_disconnect(&self) {
        self.force_auto_reconnect.store(false, Ordering::SeqCst);
        *self.expected_disconnect.lock().await = CancellationToken::new();
    }

    /// Go `Connect`/`ConnectContext`/`unlockedConnect` — dial, handshake,
    /// start the node loop and the keepalive. Returns once the session is up;
    /// pairing (`Qr`/`Connected`/`ConnectFailure`) arrives via the event sink.
    pub async fn connect(self: &Arc<Self>) -> Result<(), ClientError> {
        if self.device.lock().await.deleted {
            return Err(ClientError::DeviceDeleted);
        }
        {
            let mut socket = self.socket.lock().await;
            if let Some(existing) = socket.as_ref() {
                if existing.is_connected() {
                    return Err(ClientError::AlreadyConnected);
                }
                existing.stop(true, false).await;
                *socket = None;
            }
        }
        self.reset_expected_disconnect().await;

        let frames = match FrameSocket::connect(self.config.wa_url.as_deref()).await {
            Ok(frames) => frames,
            Err(err) => {
                self.record_connect_error();
                return Err(ClientError::Send { what: "websocket dial", source: err });
            }
        };
        let (queue_tx, queue_rx) = mpsc::channel::<crate::binary::node::Node>(HANDLER_QUEUE_SIZE);
        let conn_token = CancellationToken::new();
        *self.conn_cancel.lock().await = Some(conn_token.clone());
        let conn_token_keepalive = conn_token.clone();

        let (noise_key, payload) = {
            let device = self.device.lock().await;
            let payload =
                crate::types::version::build_client_payload_for_device(crate::types::version::WA_VERSION, &device);
            (device.noise_key.clone(), payload)
        };
        let handshake = self.config.handshake.clone();
        let ephemeral = KeyPair::generate();
        let on_frame = {
            let this = Arc::downgrade(self);
            let queue = queue_tx.clone();
            Box::new(move |data: Vec<u8>| {
                if let Some(this) = this.upgrade() {
                    this.handle_frame(&data, &queue);
                }
            }) as Box<dyn Fn(Vec<u8>) + Send + Sync + 'static>
        };
        let on_disconnect = {
            let this = Arc::downgrade(self);
            Box::new(move |remote: bool| {
                if let Some(this) = this.upgrade() {
                    let this = Arc::clone(&this);
                    tokio::spawn(async move { this.on_disconnect(remote).await });
                }
            }) as Box<dyn Fn(bool) + Send + Sync + 'static>
        };
        let socket =
            match do_handshake_with(frames, &ephemeral, &noise_key, payload, handshake, on_frame, Some(on_disconnect))
                .await
            {
                Ok(socket) => socket,
                Err(err) => {
                    self.record_connect_error();
                    if self.config.enable_auto_reconnect && is_retryable_connect_error(&err) {
                        let this = Arc::clone(self);
                        tokio::spawn(async move {
                            this.dispatch_event(crate::WaEvent::Disconnected);
                            let _ = this.auto_reconnect().await;
                        });
                    }
                    return Err(err);
                }
            };
        *self.socket.lock().await = Some(Arc::new(socket));
        self.paired.store(self.device.lock().await.id.is_some(), Ordering::SeqCst);

        let (done_tx, done_rx) = oneshot::channel();
        *self.handler_queue_done.lock().await = Some(done_rx);
        let this = Arc::clone(self);
        tokio::spawn(async move { this.handler_queue_loop(queue_rx, conn_token, done_tx).await });

        let ping = {
            let this = Arc::downgrade(self);
            move || {
                let this = this.clone();
                async move {
                    let Some(this) = this.upgrade() else {
                        return crate::keepalive::PingOutcome::Disconnected;
                    };
                    match this
                        .send_iq_async(InfoQuery {
                            namespace: "w:p".into(),
                            kind: IqKind::Get,
                            to: Some(server_jid()),
                            ..Default::default()
                        })
                        .await
                    {
                        Ok(rx) => match rx.await {
                            Ok(_) => crate::keepalive::PingOutcome::Answered,
                            Err(_) => crate::keepalive::PingOutcome::Disconnected,
                        },
                        Err(ClientError::NotConnected) => crate::keepalive::PingOutcome::Disconnected,
                        Err(_) => crate::keepalive::PingOutcome::TimedOut,
                    }
                }
            }
        };
        let emit = {
            let events = Arc::clone(&self.events);
            move |event: crate::WaEvent| (events)(event)
        };
        let force = {
            let this = Arc::downgrade(self);
            move || {
                let this = this.clone();
                tokio::spawn(async move {
                    if let Some(this) = this.upgrade() {
                        this.disconnect().await;
                        this.reset_expected_disconnect().await;
                        let this = Arc::clone(&this);
                        tokio::spawn(async move {
                            let _ = this.auto_reconnect().await;
                        });
                    }
                });
            }
        };
        let keepalive = crate::keepalive::KeepAlive::new(self.config.keepalive, ping, emit, force);
        tokio::spawn(async move { keepalive.run(conn_token_keepalive.cancelled()).await });
        Ok(())
    }

    fn record_connect_error(&self) {
        self.reconnect_errors.fetch_add(1, Ordering::SeqCst);
    }

    /// Go `autoReconnect` — capped exponential backoff; `DeviceDeleted` and
    /// `NotLoggedIn` are never retried; after `max_attempts` emit
    /// `ConnectFailure` + `Disconnected`.
    ///
    /// Boxed (rather than `async fn`) to break the `connect` ⇄
    /// `auto_reconnect` future-type recursion (`connect` spawns this, this
    /// calls `connect`).
    pub fn auto_reconnect(
        self: &Arc<Self>,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<(), ClientError>> + Send + '_>> {
        let this = Arc::clone(self);
        Box::pin(async move { this.auto_reconnect_inner().await })
    }

    async fn auto_reconnect_inner(self: &Arc<Self>) -> Result<(), ClientError> {
        if !self.config.enable_auto_reconnect {
            return Ok(());
        }
        if self.device.lock().await.deleted {
            return Err(ClientError::DeviceDeleted);
        }
        let mut attempt = 0u32;
        loop {
            if self.is_connected().await {
                return Ok(());
            }
            let delay = self.config.reconnect.delay_for(attempt);
            let cancel = self.conn_cancel.lock().await.clone();
            match cancel {
                Some(token) => {
                    tokio::select! {
                        () = token.cancelled() => return Ok(()),
                        () = tokio::time::sleep(delay) => {}
                    }
                }
                None => tokio::time::sleep(delay).await,
            }
            attempt += 1;
            self.reconnect_errors.fetch_add(1, Ordering::SeqCst);
            match self.connect().await {
                Ok(()) => return Ok(()),
                Err(ClientError::DeviceDeleted) | Err(ClientError::NotLoggedIn) => {
                    return Err(ClientError::DeviceDeleted);
                }
                Err(err) if attempt >= self.config.reconnect.max_attempts => {
                    let message = err.to_string();
                    self.dispatch_event(crate::WaEvent::ConnectFailure {
                        reason: reason::INTERNAL_SERVER_ERROR,
                        message,
                    });
                    self.dispatch_event(crate::WaEvent::Disconnected);
                    return Err(err);
                }
                Err(_) => continue,
            }
        }
    }

    /// Go `onDisconnect` — ignore a foreign socket; else clear the socket and
    /// the waiters, and spawn `auto_reconnect` when the close was unexpected.
    pub async fn on_disconnect(self: &Arc<Self>, remote: bool) {
        *self.socket.lock().await = None;
        self.clear_response_waiters(Some(session::xml_stream_end_node())).await;
        if !self.is_expected_disconnect().await && (self.force_auto_reconnect.swap(false, Ordering::SeqCst) || remote) {
            tracing::debug!("emitting Disconnected event");
            self.dispatch_event(crate::WaEvent::Disconnected);
            let this = Arc::clone(self);
            tokio::spawn(async move {
                let _ = this.auto_reconnect().await;
            });
        } else if remote {
            tracing::debug!("on_disconnect called, but it was expected, so not emitting an event");
        } else {
            tracing::debug!("on_disconnect called after manual disconnection");
        }
    }

    /// Go `Disconnect`/`unlockedDisconnect` — silent: no event is emitted.
    pub async fn disconnect(&self) {
        self.expect_disconnect().await;
        if let Some(socket) = self.socket.lock().await.take() {
            socket.stop(true, false).await;
        }
        self.clear_response_waiters(Some(session::xml_stream_end_node())).await;
        if let Some(cancel) = self.conn_cancel.lock().await.take() {
            cancel.cancel();
        }
        match tokio::time::timeout(Duration::from_secs(5), async {
            if let Some(done) = self.handler_queue_done.lock().await.take() {
                let _ = done.await;
            }
        })
        .await
        {
            Ok(()) => {}
            Err(_) => tracing::warn!("handler queue wait channel not closed after 5 seconds"),
        }
    }

    /// Go `ResetConnection` — force the next close to auto-reconnect.
    pub async fn reset_connection(&self) {
        self.force_auto_reconnect.store(true, Ordering::SeqCst);
        if let Some(socket) = self.socket.lock().await.as_ref() {
            socket.stop(true, true).await;
        }
        self.clear_response_waiters(Some(session::xml_stream_end_node())).await;
    }

    /// Go `Logout` — best-effort `remove-companion-device` iq, then
    /// `disconnect()`, then delete the device. Auto-reconnect stays disabled
    /// (the disconnect is expected).
    pub async fn logout(self: &Arc<Self>) -> Result<(), ClientError> {
        let own = self.device.lock().await.id.clone().unwrap_or_default();
        if own.is_empty() {
            return Err(ClientError::NotLoggedIn);
        }
        let query = InfoQuery {
            namespace: "md".into(),
            kind: IqKind::Set,
            to: Some(server_jid()),
            content: vec![
                crate::binary::node::Node::new("remove-companion-device")
                    .attr("jid", own)
                    .attr("reason", "user_initiated"),
            ],
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        // Best-effort: a failed iq must not stop the local logout.
        if let Err(err) = self.send_iq(query).await {
            tracing::warn!("logout iq failed (continuing with the local logout): {err}");
        }
        self.disconnect().await;
        self.device.lock().await.delete().await?;
        Ok(())
    }

    /// Go `makeFrameHandler`/`handleFrame` — unpack, unmarshal, resolve the
    /// waiter or push known nodes onto the handler queue (never panics).
    fn handle_frame(&self, data: &[u8], queue: &mpsc::Sender<crate::binary::node::Node>) {
        let decompressed = match crate::binary::unpack(data) {
            Ok(decompressed) => decompressed,
            Err(err) => {
                tracing::warn!("failed to decompress frame: {err}");
                tracing::debug!("errored frame hex: {}", hex::encode(data));
                return;
            }
        };
        let node = match crate::binary::unmarshal(&decompressed) {
            Ok(node) => node,
            Err(err) => {
                tracing::warn!("failed to decode node in frame: {err}");
                tracing::debug!("errored frame hex: {}", hex::encode(&decompressed));
                return;
            }
        };
        tracing::debug!("recv {}", node.to_xml_string());
        if node.tag == "xmlstreamend" {
            // `is_expected_disconnect` is async; the loop re-checks on close.
            tracing::warn!("received stream end frame");
        } else if self.try_receive_response(node.clone()) {
            // handled
        } else if is_known_node(&node.tag) {
            match queue.try_send(node) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(node)) => {
                    tracing::warn!("handler queue is full, message ordering is no longer guaranteed");
                    let queue = queue.clone();
                    tokio::spawn(async move {
                        let _ = queue.send(node).await;
                    });
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        } else if node.tag != "ack" {
            tracing::debug!("didn't handle WhatsApp node {}", node.tag);
        }
    }

    /// Synchronous half of `receive_response` for the frame callback
    /// (the callback is not async, so it cannot `.await` the mutex).
    fn try_receive_response(&self, node: crate::binary::node::Node) -> bool {
        if node.tag != "iq" && node.tag != "ack" {
            return false;
        }
        let id = match node.attrs.get("id").and_then(crate::binary::node::AttrValue::as_str) {
            Some(id) => id.to_owned(),
            None => return false,
        };
        match self.response_waiters.try_lock() {
            Ok(mut waiters) => match waiters.remove(&id) {
                Some(sender) => {
                    let _ = sender.send(node);
                    true
                }
                None => false,
            },
            Err(_) => false,
        }
    }

    /// Go `handlerQueueLoop` — one task drains the queue through `handle_node`.
    /// On exit, leftover `stream:error` nodes still go through
    /// `handle_stream_error`, then `handler_queue_done` resolves.
    ///
    /// Go's 30 s "node handling is taking long" watchdog is not ported —
    /// handlers here are fast and async (TODO(Phase 4) if a slow handler lands).
    async fn handler_queue_loop(
        self: Arc<Self>,
        mut queue: mpsc::Receiver<crate::binary::node::Node>,
        cancel: CancellationToken,
        done: oneshot::Sender<()>,
    ) {
        tracing::debug!("starting handler queue loop");
        loop {
            tokio::select! {
                node = queue.recv() => {
                    let Some(node) = node else { break };
                    if cancel.is_cancelled() {
                        tracing::debug!("closing handler queue loop before node handling");
                        break;
                    }
                    self.handle_node(&node).await;
                }
                () = cancel.cancelled() => {
                    tracing::debug!("closing handler queue loop");
                    break;
                }
            }
        }
        loop {
            match queue.try_recv() {
                Ok(node) if node.tag == "stream:error" => {
                    tracing::debug!("handling stream:error node after disconnect");
                    self.handle_stream_error(&node).await;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        let _ = done.send(());
    }

    /// Dispatch one queued node (Go `nodeHandlers[tag]`).
    async fn handle_node(self: &Arc<Self>, node: &crate::binary::node::Node) {
        // The other half of `request_log::log_outgoing`: what the server
        // answered is what explains a rejected stanza.
        crate::request_log::log_incoming(&node.to_xml_string());
        match node.tag.as_str() {
            "success" => self.handle_connect_success(node).await,
            "failure" => self.handle_connect_failure(node).await,
            "stream:error" => self.handle_stream_error(node).await,
            "ib" => self.handle_ib(node).await,
            "iq" => self.handle_iq(node).await,
            "message" | "status" | "appdata" => {
                self.handle_message_node(node).await;
            }
            "receipt" => self.handle_receipt(node).await,
            "presence" => {
                if let Some(presence) = self.handle_presence(node).await {
                    self.dispatch_event(crate::types::events::WaEvent::Presence { presence });
                }
            }
            "chatstate" => {
                if let Some(chat_presence) = self.handle_chat_state(node).await {
                    self.dispatch_event(crate::types::events::WaEvent::ChatPresence { chat_presence });
                }
            }
            "notification" => self.handle_notification(node).await,
            // TODO(Phase 5): call handler.
            _ => tracing::debug!("didn't handle WhatsApp node {}", node.tag),
        }
    }
}

/// Go `client.go:handleFrame`'s node table — the tags this phase implements
/// (`success`, `failure`, `stream:error`, `iq`, `ib`); the rest log at debug
/// with a `TODO(Phase 4/5)`.
pub fn is_known_node(tag: &str) -> bool {
    matches!(
        tag,
        "message"
            | "status"
            | "appdata"
            | "receipt"
            | "call"
            | "chatstate"
            | "presence"
            | "notification"
            | "success"
            | "failure"
            | "stream:error"
            | "iq"
            | "ib"
    )
}

/// Go `isRetryableConnectError` (from `connect()`): dial/handshake failures,
/// a closed socket, handshake timeouts and network errors are worth an
/// immediate auto-reconnect; auth errors are not.
pub fn is_retryable_connect_error(err: &ClientError) -> bool {
    matches!(
        err,
        ClientError::Send { .. }
            | ClientError::SocketClosed
            | ClientError::HandshakeTimeout
            | ClientError::Noise(_)
            | ClientError::MissingHandshakeParts
            | ClientError::UnexpectedStaticLength(_)
            | ClientError::Decrypt { .. }
            | ClientError::Mix { .. }
            | ClientError::Marshal { .. }
            | ClientError::Unmarshal { .. }
    )
}

/// `checkCertValidity` — the certificate must be inside its `notBefore`/`notAfter`
/// window (unix seconds).
fn check_cert_validity(kind: CertKind, details: &CertDetails) -> Result<(), CertError> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or_default();
    let not_before = details.not_before.unwrap_or_default();
    let not_after = details.not_after.unwrap_or_default();
    if now < not_before {
        return Err(CertError::NotValidYet { kind, now, not_before });
    }
    if now > not_after {
        return Err(CertError::Expired { kind, now, not_after });
    }
    Ok(())
}
