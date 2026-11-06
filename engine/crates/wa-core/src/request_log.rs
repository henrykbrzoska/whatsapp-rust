//! WhatsApp frame logger, both directions.
//!
//! `bebok-server` installs a sink that writes every node exchanged with WA
//! into the diagnostic ring buffer (`GET /debug/log`). `wa-core` itself only
//! knows the `tracing` targets `wa_outgoing` / `wa_incoming` — no dependency
//! on `bebok-server`.
//!
//! Both directions matter: a stanza the server rejects is only explained by
//! the `<ack>`/`<error>` that comes back, and an outgoing-only log turns every
//! rejection into guesswork.

use std::sync::{Arc, OnceLock};

/// Which way a logged frame went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Sent to WhatsApp.
    Out,
    /// Received from WhatsApp.
    In,
}

impl Direction {
    /// The marker the debug log shows (`wa>>` / `wa<<`).
    pub fn marker(self) -> &'static str {
        match self {
            Self::Out => "wa>>",
            Self::In => "wa<<",
        }
    }
}

type Sink = Arc<dyn Fn(Direction, String) + Send + Sync + 'static>;

static SINK: OnceLock<Sink> = OnceLock::new();

/// Install the global outgoing-request sink. Only the first call wins (the
/// engine has a single `AppState`).
pub fn set_sink(sink: Sink) {
    let _ = SINK.set(sink);
}

/// Log one outgoing node (XML). Always emits a `tracing::info!` on target
/// `wa_outgoing`; additionally forwards to the installed sink (the debug log).
pub fn log_outgoing(xml: &str) {
    tracing::info!(target: "wa_outgoing", ">> {}", xml);
    if let Some(sink) = SINK.get() {
        sink(Direction::Out, xml.to_owned());
    }
}

/// Log one node received from WhatsApp.
pub fn log_incoming(xml: &str) {
    tracing::info!(target: "wa_incoming", "<< {}", xml);
    if let Some(sink) = SINK.get() {
        sink(Direction::In, xml.to_owned());
    }
}

/// Log one outgoing IQ with its generated id (convenience).
pub fn log_outgoing_with_id(id: &str, xml: &str) {
    tracing::info!(target: "wa_outgoing", ">> [{}] {}", id, xml);
    if let Some(sink) = SINK.get() {
        sink(Direction::Out, format!("[{id}] {xml}"));
    }
}
