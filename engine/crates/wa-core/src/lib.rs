//! `wa-core` — the WhatsApp protocol core of the engine.
//!
//! Contains the ported types, the binary (de)serializer, the WebSocket/Noise
//! transport and its keep-alive loop, the crypto primitives and the store
//! skeleton (traits + SQLite container with `wa_version` migrations). The client
//! state machine lands in Phase 3 — see `TODO.md`.

pub mod appstate;
pub mod binary;
pub mod client;
pub mod crypto;
pub mod keepalive;
pub mod request_log;
pub mod socket;
pub mod store;
pub mod types;

pub use types::events::WaEvent;
pub use types::jid::Jid;

/// Protocol revision this engine implements (filled in by the build later).
pub const PROTOCOL_REF: &str = "unknown";
