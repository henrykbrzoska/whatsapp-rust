//! The connection handle behind `POST /connect`, `/disconnect` and `/logout`.
//!
//! Owns the engine's single protocol [`Client`] (Go: `waClient`). The client is
//! built lazily from the device container on the first `POST /connect` and
//! kept afterwards, so every handler reaches the live session through
//! [`Connection::client`] (see `routes::groups::session`). The dial and the
//! Noise handshake run in a background task, so the HTTP request returns the
//! current state immediately; the client's own events (`qr`, `pair_success`,
//! `connected`, `logged_out`, …) flow onto the engine event bus through the
//! [`EventSink`](wa_core::client::EventSink).

use std::sync::{Arc, OnceLock, RwLock, Weak};

use tokio::sync::Mutex;
use wa_core::WaEvent;
use wa_core::client::{Client, ClientConfig, ClientError, HandshakeOptions};
use wa_store::{Store, StoredMessage};

use crate::state::{AppState, SharedState};

/// Connection state, mirroring the `ConnectionState` union in
/// `client/src/app/core/services/whatsapp.service.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionState {
    /// No connection attempt in flight.
    Disconnected,
    /// The WebSocket is opening / the handshake is running.
    Connecting,
    /// The handshake completed; the encrypted socket is live.
    Connected,
    /// The device was logged out (`POST /logout`); the next connect pairs anew.
    LoggedOut,
}

impl ConnectionState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disconnected => "disconnected",
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::LoggedOut => "logged_out",
        }
    }
}

/// What `POST /connect` and `GET /connected` answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionStatus {
    /// One of `disconnected`, `connecting`, `connected`, `logged_out`.
    pub state: String,
    /// `true` while the encrypted socket is usable (`Client::IsConnected`).
    pub connected: bool,
    /// `true` once this device is paired (`Client::IsLoggedIn`).
    pub logged_in: bool,
    /// The last connection error, for the client to display.
    pub error: Option<String>,
    /// How many connection attempts were started (`0` before the first one).
    pub attempts: u64,
    /// WhatsApp closed the socket because *another* session logged in with this
    /// device's keys (`conflict type=replaced`). A client must not auto-reconnect
    /// then: two engines sharing one identity would kick each other in a loop.
    /// Cleared by an explicit `POST /connect`.
    pub replaced: bool,
}

/// Why a `replaced` disconnect happened — shown in the UI, because "disconnected"
/// alone cannot tell a takeover from a network blip.
pub const REPLACED_ERROR: &str = "another session took over this device's connection (WhatsApp closed it with \
                                  conflict type=replaced); only one engine can be online per device — sign out of \
                                  the other instance or give it a different data directory";

/// Why `POST /logout` failed.
#[derive(Debug)]
pub enum LogoutError {
    /// No paired device — nothing to log out.
    NotLoggedIn,
    /// The protocol client or the store failed.
    Client(ClientError),
}

struct Inner {
    /// The *requested* state; the live socket state comes from the client.
    state: ConnectionState,
    error: Option<String>,
    attempts: u64,
    /// The device was paired when last checked (`device.id.is_some()`).
    paired: bool,
    /// The session was taken over by another login (`conflict type=replaced`).
    replaced: bool,
}

/// The engine's single connection handle.
#[derive(Clone)]
pub struct Connection {
    inner: Arc<Mutex<Inner>>,
    /// The protocol client. A sync lock so `routes::groups::session` (a sync
    /// accessor used by every handler) can read it without awaiting.
    client: Arc<RwLock<Option<Arc<Client>>>>,
    /// Set once by [`AppState::assemble`]. Weak, so the state does not own
    /// itself through the connection.
    app: Arc<OnceLock<Weak<AppState>>>,
}

impl Connection {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                state: ConnectionState::Disconnected,
                error: None,
                attempts: 0,
                paired: false,
                replaced: false,
            })),
            client: Arc::new(RwLock::new(None)),
            app: Arc::new(OnceLock::new()),
        }
    }

    /// Give the handle the shared state it reports to. Second calls are ignored.
    pub(crate) fn attach(&self, app: &SharedState) {
        let _ = self.app.set(Arc::downgrade(app));
    }

    fn app(&self) -> Option<SharedState> {
        self.app.get().and_then(Weak::upgrade)
    }

    /// The protocol client, once `POST /connect` created one. It may be
    /// disconnected: its calls then fail with `ClientError::NotConnected`.
    pub fn client(&self) -> Option<Arc<Client>> {
        self.client.read().ok().and_then(|c| c.clone())
    }

    /// Install a client (tests, and a future multi-device container).
    pub fn set_client(&self, client: Option<Arc<Client>>) {
        if let Ok(mut slot) = self.client.write() {
            *slot = client;
        }
    }

    fn take_client(&self) -> Option<Arc<Client>> {
        self.client.write().ok().and_then(|mut c| c.take())
    }

    pub async fn status(&self) -> ConnectionStatus {
        let client = self.client();
        let (live, paired_now) = match &client {
            Some(c) => (c.is_connected().await, Some(c.is_logged_in() || c.device().await.id.is_some())),
            // No session yet: the store still knows whether this engine is
            // paired, which is what the UI needs to tell "pair the device"
            // from "reconnecting" after a restart.
            None => (false, self.paired_in_store().await),
        };
        let mut inner = self.inner.lock().await;
        if let Some(paired) = paired_now {
            inner.paired = paired;
        }
        let state = match inner.state {
            ConnectionState::LoggedOut => ConnectionState::LoggedOut,
            _ if live => ConnectionState::Connected,
            ConnectionState::Connecting => ConnectionState::Connecting,
            _ => ConnectionState::Disconnected,
        };
        ConnectionStatus {
            state: state.as_str().to_string(),
            connected: live,
            logged_in: inner.paired,
            error: inner.error.clone(),
            attempts: inner.attempts,
            replaced: inner.replaced,
        }
    }

    /// Fold one client event into the reported session state.
    ///
    /// `error` is what the window displays, so a server-side kick must leave an
    /// explanation behind; and a `replaced` kick must clear auto-reconnect (see
    /// [`ConnectionStatus::replaced`]) or the loser of a two-engine setup would
    /// immediately steal the session back, forever.
    pub async fn apply_event(&self, ev: &WaEvent) {
        let mut inner = self.inner.lock().await;
        match ev {
            WaEvent::Connected => {
                inner.state = ConnectionState::Connected;
                inner.error = None;
                inner.replaced = false;
            }
            WaEvent::Disconnected => {
                if inner.state == ConnectionState::Connected {
                    inner.state = ConnectionState::Disconnected;
                }
            }
            WaEvent::StreamReplaced => {
                inner.state = ConnectionState::Disconnected;
                inner.error = Some(REPLACED_ERROR.to_owned());
                inner.replaced = true;
            }
            WaEvent::StreamError { code } => {
                inner.state = ConnectionState::Disconnected;
                inner.error = Some(format!("the server closed the connection with stream error {code}"));
            }
            WaEvent::LoggedOut { .. } => {
                inner.state = ConnectionState::LoggedOut;
                inner.error = None;
                inner.paired = false;
                inner.replaced = false;
            }
            WaEvent::ConnectFailure { message, .. } => {
                inner.state = ConnectionState::Disconnected;
                inner.error = Some(message.clone());
            }
            _ => {}
        }
    }

    /// Sync adapter for the client's [`EventSink`](wa_core::client::EventSink),
    /// which cannot await: the state update is handed to the runtime. Events
    /// that do not change the session state are dropped without a task.
    pub fn note_event(&self, ev: &WaEvent) {
        let ev = match ev {
            WaEvent::Connected
            | WaEvent::Disconnected
            | WaEvent::StreamReplaced
            | WaEvent::StreamError { .. }
            | WaEvent::LoggedOut { .. }
            | WaEvent::ConnectFailure { .. } => ev.clone(),
            _ => return,
        };
        let this = self.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move { this.apply_event(&ev).await });
            }
            Err(_) => tracing::debug!("no tokio runtime; the session state was not updated"),
        }
    }

    /// `true` when the device container holds a paired device (`GET /logged_in`
    /// without the HTTP layer). `None` when the store cannot be reached.
    async fn paired_in_store(&self) -> Option<bool> {
        let app = self.app()?;
        match app.container.device_jids().await {
            Ok(jids) => Some(!jids.is_empty()),
            Err(err) => {
                tracing::warn!("could not read the device container: {err}");
                None
            }
        }
    }

    pub async fn is_connected(&self) -> bool {
        match self.client() {
            Some(c) => c.is_connected().await,
            None => false,
        }
    }

    /// Start a connection attempt if none is running.
    ///
    /// Returns immediately with the *current* state; the dial and the Noise
    /// handshake run in a background task and report through [`WaEvent`]. A
    /// second call while a connection is up (or in flight) is a no-op returning
    /// the existing state, so the frontend cannot stack sessions.
    pub async fn connect(&self) -> ConnectionStatus {
        let live = self.is_connected().await;
        {
            let mut inner = self.inner.lock().await;
            if inner.state == ConnectionState::Connecting || live {
                drop(inner);
                return self.status().await;
            }
            inner.state = ConnectionState::Connecting;
            inner.error = None;
            inner.attempts += 1;
            // An explicit connect is the user's decision — it may well be the
            // session that takes the device back from another instance.
            inner.replaced = false;
        }

        let Some(app) = self.app() else {
            self.fail("the connection handle is not attached to the engine state").await;
            return self.status().await;
        };
        let this = self.clone();
        tokio::spawn(async move { this.run(app).await });
        self.status().await
    }

    /// Close the socket without emitting `disconnected` (Go: `Client::Disconnect`).
    /// The client is kept, so the next `connect` resumes the same device.
    pub async fn disconnect(&self) {
        if let Some(client) = self.client() {
            client.disconnect().await;
        }
        let mut inner = self.inner.lock().await;
        if inner.state == ConnectionState::Connected || inner.state == ConnectionState::Connecting {
            inner.state = ConnectionState::Disconnected;
            inner.error = None;
        }
    }

    /// Go `Client::Logout`: `remove-companion-device` (best effort when the
    /// socket is up), disconnect, delete the device from the store and drop the
    /// client. Without a client, a paired device is still deleted locally.
    pub async fn logout(&self) -> Result<(), LogoutError> {
        let app = self.app();
        match self.take_client() {
            Some(client) => match client.logout().await {
                Ok(()) => {}
                Err(ClientError::NotLoggedIn) => {
                    self.set_client(Some(client));
                    return Err(LogoutError::NotLoggedIn);
                }
                Err(err) => return Err(LogoutError::Client(err)),
            },
            None => {
                let Some(app) = &app else { return Err(LogoutError::NotLoggedIn) };
                let mut device = app.container.get_first_device().await.map_err(|e| LogoutError::Client(e.into()))?;
                if device.id.is_none() {
                    return Err(LogoutError::NotLoggedIn);
                }
                device.delete().await.map_err(|e| LogoutError::Client(e.into()))?;
            }
        }
        {
            let mut inner = self.inner.lock().await;
            inner.state = ConnectionState::LoggedOut;
            inner.paired = false;
            inner.error = None;
        }
        if let Some(app) = app {
            app.debug_log.note("logged out (device deleted)");
            app.emit(WaEvent::LoggedOut { on_connect: false, reason: 0 });
        }
        Ok(())
    }

    /// One connection attempt: build the client if needed, dial, handshake.
    async fn run(&self, app: SharedState) {
        let client = match self.ensure_client(&app).await {
            Ok(client) => client,
            Err(err) => return self.report_failure(&app, err.to_string()).await,
        };
        match client.connect().await {
            Ok(()) | Err(ClientError::AlreadyConnected) => {
                tracing::info!("noise handshake completed; the protocol client is running");
                app.debug_log.note("noise handshake completed");
                let paired = client.device().await.id.is_some();
                let mut inner = self.inner.lock().await;
                // A logout or disconnect that raced the handshake wins.
                if inner.state == ConnectionState::Connecting {
                    inner.state = ConnectionState::Connected;
                }
                inner.paired = paired;
            }
            Err(err) => self.report_failure(&app, describe(err)).await,
        }
    }

    async fn report_failure(&self, app: &SharedState, err: String) {
        tracing::warn!("connect failed: {err}");
        app.debug_log.note(format!("connect failed: {err}"));
        self.fail(&err).await;
        app.emit(WaEvent::ConnectFailure { reason: 0, message: err });
    }

    /// The existing client, or a new one over the container's first device
    /// (`get_first_device` returns a fresh, unpaired device when the store is
    /// empty — the QR flow's starting point).
    async fn ensure_client(&self, app: &SharedState) -> Result<Arc<Client>, ClientError> {
        if let Some(client) = self.client() {
            return Ok(client);
        }
        let config = ClientConfig {
            wa_url: app.config.wa_url.clone(),
            handshake: HandshakeOptions { response_timeout: app.config.handshake_timeout, ..Default::default() },
            ..Default::default()
        };
        let client = Client::from_container(&app.container, config, event_sink(app)).await?;
        spawn_history_sync_forwarder(&client, app);
        self.set_client(Some(Arc::clone(&client)));
        Ok(client)
    }

    async fn fail(&self, error: &str) {
        let mut inner = self.inner.lock().await;
        inner.state = ConnectionState::Disconnected;
        inner.error = Some(error.to_string());
    }
}

impl Default for Connection {
    fn default() -> Self {
        Self::new()
    }
}

/// The client's event sink: the engine bus, the debug log and the local
/// message store (GAP A5). Holds only the sender, the log and the store (not
/// the state), so the client does not keep the state alive. Synchronous and
/// non-blocking, as `EventSink` requires: the SQLite write is handed to the
/// blocking pool.
pub fn event_sink(app: &SharedState) -> wa_core::client::EventSink {
    let events = app.events.clone();
    let log = Arc::clone(&app.debug_log);
    let store = Arc::clone(&app.store);
    // Weak, so the sink (owned by the client) does not keep the state alive.
    let weak = Arc::downgrade(app);
    Arc::new(move |ev: WaEvent| {
        log.record_event(&ev);
        if let Some(row) = stored_from_event(&ev) {
            persist_message(&store, row);
        }
        if let Some(app) = weak.upgrade() {
            // A takeover (`conflict type=replaced`) must leave a reason behind
            // and disable auto-reconnect — see `Connection::apply_event`.
            app.connection.note_event(&ev);
            // `<success>`: pull the address book and the chat flags, which is
            // what turns raw JIDs into names (see `crate::appstate_sync`).
            if matches!(ev, WaEvent::Connected) {
                crate::appstate_sync::spawn_after_connect(&app);
            }
        }
        let _ = events.send(ev);
    })
}

/// Map a live `WaEvent::Message` onto a wa-store row. Other events yield `None`.
fn stored_from_event(ev: &WaEvent) -> Option<StoredMessage> {
    let WaEvent::Message { info, text, message, .. } = ev else {
        return None;
    };
    Some(StoredMessage {
        id: info.id.to_string(),
        chat_jid: info.chat.to_string(),
        sender: info.sender.to_string(),
        timestamp: info.timestamp,
        // The stanza only says `type="media"`; the payload says whether that
        // is a photo, a video, a GIF or a voice note — which is what decides
        // between an `<img>`, a `<video>` and a label in the thread.
        kind: media_kind_of_payload(message.as_deref())
            .unwrap_or(if info.message_type.is_empty() { "text" } else { info.message_type.as_str() })
            .to_owned(),
        text: text.clone(),
        // The hex-encoded `waE2E.Message`: an attachment needs its
        // `directPath`/`mediaKey`, so the payload is kept, not just its text.
        raw_json: message.clone(),
        from_me: info.is_from_me,
        // Our own `notify` name says nothing about the chat.
        push_name: (!info.is_from_me && !info.push_name.trim().is_empty()).then(|| info.push_name.clone()),
    })
}

/// The attachment kind of a hex-encoded `waE2E.Message`, or `None` when the
/// payload is missing, not hex, or carries no attachment.
fn media_kind_of_payload(raw_hex: Option<&str>) -> Option<&'static str> {
    let bytes = hex::decode(raw_hex?).ok()?;
    wa_core::client::history_blob::media_kind_from_bytes(&bytes)
}

/// Write one message on the blocking pool when a runtime is available, otherwise
/// inline. Failures are logged, never propagated: the live event still goes out.
fn persist_message(store: &Arc<Store>, row: StoredMessage) {
    let store = Arc::clone(store);
    let write = move || {
        if let Err(err) = store.insert_message(&row) {
            tracing::warn!("failed to persist message {}: {err}", row.id);
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(write);
        }
        Err(_) => write(),
    }
}

/// Forward `Client::history_sync_events()` onto the engine bus (SSE) and the
/// debug log. The task ends when the client (and with it the sender) is dropped.
fn spawn_history_sync_forwarder(client: &Arc<Client>, app: &SharedState) {
    let mut rx = client.history_sync_events();
    let events = app.events.clone();
    let log = Arc::clone(&app.debug_log);
    // Weak: the task must not keep the engine state alive after a logout.
    let weak = Arc::downgrade(app);
    tokio::spawn(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let note =
                        format!("history sync notification {} ({:?}, chat {})", ev.message_id, ev.sync_type(), ev.chat);
                    tracing::info!("{note}");
                    log.note(note);
                    let _ = events.send(WaEvent::HistorySync {
                        message_id: ev.message_id.to_string(),
                        chat: ev.chat.to_string(),
                        sender: ev.sender.to_string(),
                        conversation_timestamp: ev.conversation_timestamp,
                        sync_type: format!("{:?}", ev.sync_type()),
                    });
                    // The notification only points at a blob — download it and
                    // write the messages it carries into the local database.
                    let Some(app) = weak.upgrade() else { break };
                    match crate::history_import::import(&app, &ev).await {
                        Ok(report) => {
                            if report.stored > 0 || report.named > 0 {
                                let _ = app.events.send(WaEvent::HistoryImported {
                                    messages: report.messages as u64,
                                    stored: report.stored as u64,
                                    conversations: report.conversations as u64,
                                    named: report.named as u64,
                                    end_of_history: report.end_of_history,
                                });
                            }
                        }
                        Err(err) => {
                            let note = format!("history sync import failed: {err:#}");
                            tracing::warn!("{note}");
                            app.debug_log.note(note);
                        }
                    }
                }
                Err(RecvError::Lagged(n)) => log.note(format!("history sync forwarder lagged by {n}")),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// The `error` text (`connect_failure.message`), with a clearer hint for the
/// one failure users actually hit: a stale device clock or an expired mirror.
fn describe(err: ClientError) -> String {
    use wa_core::client::CertError;
    match &err {
        ClientError::Cert(CertError::Expired { .. } | CertError::NotValidYet { .. }) => {
            format!("the server certificate is not valid, check the system clock: {err}")
        }
        _ => err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_match_the_frontend_union() {
        assert_eq!(ConnectionState::Disconnected.as_str(), "disconnected");
        assert_eq!(ConnectionState::Connecting.as_str(), "connecting");
        assert_eq!(ConnectionState::Connected.as_str(), "connected");
        assert_eq!(ConnectionState::LoggedOut.as_str(), "logged_out");
    }

    #[test]
    fn status_uses_the_documented_json_keys() {
        let status = ConnectionStatus {
            state: "connected".into(),
            connected: true,
            logged_in: false,
            error: None,
            attempts: 2,
            replaced: false,
        };
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            serde_json::json!({
                "state": "connected",
                "connected": true,
                "loggedIn": false,
                "error": null,
                "attempts": 2,
                "replaced": false,
            })
        );
    }

    #[tokio::test]
    async fn a_takeover_by_another_session_is_reported_and_not_retried() {
        let connection = Connection::new();
        connection.apply_event(&WaEvent::Connected).await;
        let status = connection.status().await;
        assert!(!status.replaced && status.error.is_none(), "got {status:?}");

        // A second engine logged in with the same keys.
        connection.apply_event(&WaEvent::StreamReplaced).await;
        let status = connection.status().await;
        assert_eq!(status.state, "disconnected");
        assert!(status.replaced, "the UI must not auto-reconnect after a takeover");
        assert!(status.error.as_deref().is_some_and(|e| e.contains("replaced")), "got {status:?}");

        // An explicit connect is allowed to take the device back.
        connection.apply_event(&WaEvent::Connected).await;
        let status = connection.status().await;
        assert!(!status.replaced);
        assert_eq!(status.error, None);
    }

    #[tokio::test]
    async fn stream_errors_and_logout_update_the_state() {
        let connection = Connection::new();
        connection.apply_event(&WaEvent::StreamError { code: "500".into() }).await;
        let status = connection.status().await;
        assert_eq!(status.state, "disconnected");
        assert!(status.error.as_deref().is_some_and(|e| e.contains("500")), "got {status:?}");

        connection.apply_event(&WaEvent::LoggedOut { on_connect: false, reason: 401 }).await;
        assert_eq!(connection.status().await.state, "logged_out");
    }

    #[tokio::test]
    async fn a_plain_disconnect_keeps_the_last_error() {
        let connection = Connection::new();
        connection.apply_event(&WaEvent::ConnectFailure { reason: 0, message: "boom".into() }).await;
        connection.apply_event(&WaEvent::Disconnected).await;
        let status = connection.status().await;
        assert_eq!(status.state, "disconnected");
        assert_eq!(status.error.as_deref(), Some("boom"));
    }

    #[test]
    fn note_event_outside_a_runtime_does_not_panic() {
        let connection = Connection::new();
        // The `EventSink` contract is sync; without a runtime the update is
        // dropped rather than panicking on `Handle::current`.
        connection.note_event(&WaEvent::StreamReplaced);
        // An event that cannot change the session state is a no-op as well.
        connection.note_event(&WaEvent::KeepAliveRestored);
    }

    #[tokio::test]
    async fn a_detached_handle_fails_instead_of_hanging() {
        let connection = Connection::new();
        // An unattached handle knows it cannot connect, so it fails right away —
        // the important part is that it *returns* instead of awaiting forever.
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), connection.connect())
            .await
            .expect("connect must return promptly");
        assert_eq!(status.attempts, 1);
        assert_eq!(status.state, "disconnected");
        assert!(status.error.as_deref().is_some_and(|e| e.contains("not attached")), "got {status:?}");
        assert!(!status.connected);
        assert!(!connection.is_connected().await);
        assert!(connection.client().is_none());
    }

    #[tokio::test]
    async fn disconnect_is_idempotent() {
        let connection = Connection::new();
        connection.disconnect().await;
        connection.disconnect().await;
        assert_eq!(connection.status().await.state, "disconnected");
    }

    #[tokio::test]
    async fn logout_without_a_paired_device_is_rejected() {
        let connection = Connection::new();
        assert!(matches!(connection.logout().await, Err(LogoutError::NotLoggedIn)));
    }

    fn message_event(text: Option<&str>, message_type: &str) -> WaEvent {
        use wa_core::types::jid::Jid;
        use wa_core::types::message::{MessageID, MessageInfo};
        let info = MessageInfo {
            chat: Jid::parse("15550002222@s.whatsapp.net").unwrap(),
            sender: Jid::parse("15550002222@s.whatsapp.net").unwrap(),
            id: MessageID::new("LIVE1".to_owned()),
            timestamp: 1_700_000_500,
            message_type: message_type.to_owned(),
            ..Default::default()
        };
        WaEvent::Message {
            info,
            message: None,
            raw: None,
            text: text.map(str::to_owned),
            is_ephemeral: false,
            is_view_once: false,
            is_edit: false,
            retry_count: 0,
        }
    }

    #[test]
    fn live_text_message_maps_to_a_store_row() {
        let row = stored_from_event(&message_event(Some("hello"), "text")).expect("a row");
        assert_eq!(row.id, "LIVE1");
        assert_eq!(row.chat_jid, "15550002222@s.whatsapp.net");
        assert_eq!(row.sender, "15550002222@s.whatsapp.net");
        assert_eq!(row.timestamp, 1_700_000_500);
        assert_eq!(row.kind, "text");
        assert_eq!(row.text.as_deref(), Some("hello"));
        assert!(!row.from_me, "an incoming message is not from us");
    }

    #[test]
    fn own_message_keeps_the_from_me_flag() {
        let WaEvent::Message { mut info, .. } = message_event(Some("mine"), "text") else { unreachable!() };
        info.is_from_me = true;
        let row = stored_from_event(&WaEvent::Message {
            info,
            message: None,
            raw: None,
            text: Some("mine".to_owned()),
            is_ephemeral: false,
            is_view_once: false,
            is_edit: false,
            retry_count: 0,
        })
        .expect("a row");
        assert!(row.from_me);
    }

    #[test]
    fn empty_message_type_defaults_to_text_and_non_message_events_are_skipped() {
        let row = stored_from_event(&message_event(None, "")).expect("a row");
        assert_eq!(row.kind, "text");
        assert_eq!(row.text, None);
        assert!(stored_from_event(&WaEvent::Connected).is_none());
    }

    #[test]
    fn live_message_is_readable_from_the_store() {
        let store = Store::open_memory(None).unwrap();
        let row = stored_from_event(&message_event(Some("persisted"), "text")).unwrap();
        store.insert_message(&row).unwrap();
        let back = store.get_message("LIVE1").unwrap().expect("stored");
        assert_eq!(back.text.as_deref(), Some("persisted"));
    }
}
