//! In-memory diagnostic log behind `GET /debug/log` / `DELETE /debug/log`
//! (`README.md`: only with `--diagnostic` / `BEBOK_DIAGNOSTIC=1`).
//!
//! A bounded ring buffer of what the engine did: every [`WaEvent`] published on
//! the event bus plus a few engine notes (connection attempts, logout, history
//! sync notifications). Secrets never land here: QR codes and pairing data are
//! replaced with `<redacted>` and the capability token is scrubbed from every
//! message (`ARCHITECTURE.md` §9).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use axum::Json;
use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use wa_core::WaEvent;

use crate::state::SharedState;

/// Global weak handle so panics/fatals before or outside a request can still
/// land in the ring buffer visible at `GET /debug/log`.
static GLOBAL: OnceLock<Weak<DebugLog>> = OnceLock::new();

pub(crate) fn set_global(log: &Arc<DebugLog>) {
    let _ = GLOBAL.set(Arc::downgrade(log));
}

/// Append a note to the diagnostic log even without a `SharedState` handle.
/// Falls back to stderr when the log is not yet initialized (early boot).
pub fn global_note(message: impl Into<String>) {
    let msg = message.into();
    if let Some(log) = GLOBAL.get().and_then(Weak::upgrade) {
        log.note(msg);
    } else {
        // Early boot / tests — still visible in the terminal.
        eprintln!("[debug-log early] {msg}");
        // Also keep it in a tiny fallback buffer so it is not lost if
        // `GET /debug/log` is polled right after boot.
        fallback_push(msg);
    }
}

static FALLBACK: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn fallback_push(msg: String) {
    if let Ok(mut g) = FALLBACK.lock() {
        if g.len() >= 50 {
            g.remove(0);
        }
        g.push(msg);
    }
}

pub(crate) fn drain_fallback_into(log: &DebugLog) {
    let msgs = FALLBACK.lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default();
    for m in msgs {
        log.note(m);
    }
}

/// How many entries the ring buffer keeps.
pub const DEBUG_LOG_CAPACITY: usize = 1000;

/// The marker that replaces a secret value.
pub const REDACTED: &str = "<redacted>";

/// One log line.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DebugLogEntry {
    /// Monotonic sequence number (survives `clear`, so a poller can resume).
    pub seq: u64,
    /// Unix milliseconds.
    pub ts: i64,
    /// `event` (a published `WaEvent`) or `engine` (an engine note).
    pub kind: &'static str,
    /// Short summary: the event type, or the note itself.
    pub message: String,
    /// The redacted event payload, for `kind == "event"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

struct Inner {
    entries: VecDeque<DebugLogEntry>,
    next_seq: u64,
}

/// The ring buffer. Recording is synchronous and never blocks for long, so it
/// is safe inside the `wa_core::client::EventSink` closure.
pub struct DebugLog {
    inner: Mutex<Inner>,
    capacity: usize,
    /// Strings scrubbed from every message (the capability token).
    secrets: Vec<String>,
}

impl DebugLog {
    pub fn new(capacity: usize, secrets: Vec<String>) -> Self {
        Self {
            inner: Mutex::new(Inner { entries: VecDeque::with_capacity(capacity.min(64)), next_seq: 1 }),
            capacity: capacity.max(1),
            secrets: secrets.into_iter().filter(|s| !s.is_empty()).collect(),
        }
    }

    /// Record a published event (payload redacted).
    pub fn record_event(&self, ev: &WaEvent) {
        let data = redact_event(ev);
        let message = data.get("type").and_then(Value::as_str).unwrap_or("event").to_owned();
        self.push("event", message, Some(data));
    }

    /// Record an engine note.
    pub fn note(&self, message: impl Into<String>) {
        self.push("engine", message.into(), None);
    }

    fn push(&self, kind: &'static str, message: String, data: Option<Value>) {
        let message = self.scrub(message);
        let data = data.map(|d| self.scrub_value(d));
        let Ok(mut inner) = self.inner.lock() else { return };
        let seq = inner.next_seq;
        inner.next_seq += 1;
        if inner.entries.len() >= self.capacity {
            inner.entries.pop_front();
        }
        inner.entries.push_back(DebugLogEntry { seq, ts: unix_millis(), kind, message, data });
    }

    /// The newest `limit` entries (oldest first), optionally only after `since`.
    pub fn entries(&self, limit: usize, since: Option<u64>) -> Vec<DebugLogEntry> {
        let Ok(inner) = self.inner.lock() else { return Vec::new() };
        let filtered: Vec<&DebugLogEntry> =
            inner.entries.iter().filter(|e| since.is_none_or(|since| e.seq > since)).collect();
        let skip = filtered.len().saturating_sub(limit);
        filtered.into_iter().skip(skip).cloned().collect()
    }

    /// Drop every entry; returns how many were removed.
    pub fn clear(&self) -> usize {
        let Ok(mut inner) = self.inner.lock() else { return 0 };
        let n = inner.entries.len();
        inner.entries.clear();
        n
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|i| i.entries.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn scrub(&self, mut text: String) -> String {
        for secret in &self.secrets {
            if text.contains(secret.as_str()) {
                text = text.replace(secret.as_str(), REDACTED);
            }
        }
        text
    }

    fn scrub_value(&self, value: Value) -> Value {
        match value {
            Value::String(s) => Value::String(self.scrub(s)),
            Value::Array(items) => Value::Array(items.into_iter().map(|v| self.scrub_value(v)).collect()),
            Value::Object(map) => Value::Object(map.into_iter().map(|(k, v)| (k, self.scrub_value(v))).collect()),
            other => other,
        }
    }
}

/// The event as JSON with the secret-bearing fields replaced: QR codes (they
/// pair a device), and anything under a key that looks like key material.
pub fn redact_event(ev: &WaEvent) -> Value {
    let mut value = serde_json::to_value(ev).unwrap_or_else(|_| json!({ "type": "unserializable" }));
    redact_keys(&mut value);
    value
}

fn is_secret_key(key: &str) -> bool {
    // `mediaKey`, `media_key`, `encToken`, … all end in a secret-ish word.
    let key = key.to_ascii_lowercase();
    matches!(key.as_str(), "codes" | "code" | "pairing_code" | "pairingcode")
        || ["key", "token", "secret", "password", "passkey", "signature"].iter().any(|s| key.ends_with(s))
}

fn redact_keys(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (k, v) in map.iter_mut() {
                if k != "type" && is_secret_key(k) {
                    *v = Value::String(REDACTED.to_owned());
                } else {
                    redact_keys(v);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_keys),
        _ => {}
    }
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Query of `GET /debug/log`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DebugLogParams {
    /// Max entries (default 200, capped at the buffer capacity).
    pub limit: Option<usize>,
    /// Only entries with `seq > since`.
    pub since: Option<u64>,
}

/// `GET /debug/log`.
pub async fn get(State(s): State<SharedState>, Query(p): Query<DebugLogParams>) -> Json<Value> {
    let limit = p.limit.unwrap_or(200).clamp(1, DEBUG_LOG_CAPACITY);
    let entries = s.debug_log.entries(limit, p.since);
    Json(json!({ "capacity": DEBUG_LOG_CAPACITY, "entries": entries }))
}

/// `DELETE /debug/log`.
pub async fn clear(State(s): State<SharedState>) -> Json<Value> {
    Json(json!({ "cleared": s.debug_log.clear() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_buffer_drops_the_oldest_entries() {
        let log = DebugLog::new(3, vec![]);
        for i in 0..5 {
            log.note(format!("n{i}"));
        }
        let entries = log.entries(10, None);
        assert_eq!(entries.iter().map(|e| e.message.as_str()).collect::<Vec<_>>(), ["n2", "n3", "n4"]);
        assert_eq!(entries[0].seq, 3);
        assert_eq!(log.entries(1, None)[0].message, "n4");
        assert_eq!(log.entries(10, Some(4)).len(), 1);
        assert_eq!(log.clear(), 3);
        assert!(log.is_empty());
        log.note("after");
        assert_eq!(log.entries(10, None)[0].seq, 6, "sequence numbers survive a clear");
    }

    #[test]
    fn qr_codes_and_the_token_are_redacted() {
        let log = DebugLog::new(10, vec!["tok123".into()]);
        log.record_event(&WaEvent::Qr { codes: vec!["2@secret-ref,abc".into()] });
        log.note("ready at http://127.0.0.1/?token=tok123");
        let entries = log.entries(10, None);
        let dump = serde_json::to_string(&entries).unwrap();
        assert!(!dump.contains("secret-ref"), "{dump}");
        assert!(!dump.contains("tok123"), "{dump}");
        assert_eq!(entries[0].message, "qr");
        assert_eq!(entries[0].data.as_ref().unwrap()["codes"], REDACTED);
    }

    #[test]
    fn harmless_events_keep_their_payload() {
        let v = redact_event(&WaEvent::ConnectFailure { reason: 401, message: "logged out".into() });
        assert_eq!(v["reason"], 401);
        assert_eq!(v["message"], "logged out");
        assert_eq!(redact_event(&WaEvent::Connected)["type"], "connected");
    }
}
