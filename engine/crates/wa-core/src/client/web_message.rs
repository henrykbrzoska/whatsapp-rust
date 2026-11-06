// Ported from the Go client `message.go` (`ParseWebMessage`) and the wire shape of
// `proto/waWeb.proto`, © Tulir Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! `WebMessageInfo` → [`Message`] (history sync, unavailable-message responses)
//! and the JSON-shape decision for protobuf payloads.
//!
//! ## Decision: no `prost` JSON mapping in Phase 4
//!
//! Go marshals `waE2E.Message` with `protojson` and the UI consumes that shape.
//! `prost` ships no JSON mapping (and `serde_json` is only a dev-dependency of
//! this crate), so Phase 4 Task C sends **hex-encoded protobufs** on
//! [`crate::WaEvent::Message`] (`message`/`raw`) plus the plain-text body
//! (`text`) — everything the chat list needs today. A faithful `protojson`
//! port (field names, enums as strings, 64-bit ints as strings) is
//! TODO(Phase 5) and lands behind [`message_to_json`] so the wire shape changes
//! in one place.

use wa_proto::wa_web_protobufs_e2e::Message as WaMessage;
use wa_proto::wa_web_protobufs_web::WebMessageInfo;

use crate::types::jid::Jid;
use crate::types::message::{Message, MessageID, MessageInfo};

use super::Client;

/// Escape a string for a JSON document (RFC 8259).
fn json_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The Phase 4 JSON view of a protobuf message: `{"text": …, "hex": …}`.
///
/// This is deliberately *not* `protojson` — see the module docs. `text` is the
/// conversation/extended-text body, `hex` the full protobuf so the frontend can
/// decode the rest once the mapping lands.
pub fn message_to_json(message: &WaMessage) -> String {
    use prost::Message as ProstMessage;
    let text = message
        .conversation
        .clone()
        .or_else(|| message.extended_text_message.as_ref().and_then(|extended| extended.text.clone()))
        .unwrap_or_default();
    let hex = hex::encode(message.encode_to_vec());
    format!("{{\"text\":{},\"hex\":{}}}", json_escape(&text), json_escape(&hex))
}

impl Client {
    /// Go `ParseWebMessage` — turn a stored/history-sync `WebMessageInfo` into a
    /// [`Message`] event payload.
    ///
    /// Go's full version also handles bot metadata, `messageStubType` and the
    /// available/unavailable split. Phase 4 Task C maps the key, the timestamp
    /// and the message body; the rest is TODO(Phase 5) with the history sync.
    pub async fn parse_web_message(&self, web: &WebMessageInfo) -> Message {
        let own = {
            let device = self.device.lock().await;
            device.get_jid()
        };
        let chat = web.key.remote_jid.as_deref().and_then(|jid| Jid::parse(jid).ok()).unwrap_or_default();
        let from_me = web.key.from_me.unwrap_or(false);
        let participant = web.key.participant.as_deref().and_then(|jid| Jid::parse(jid).ok());
        let sender = if from_me { own.clone() } else { participant.clone().unwrap_or_else(|| chat.clone()) };
        let sender_alt = if from_me { chat.clone() } else { participant.unwrap_or_default() };
        let info = MessageInfo {
            is_from_me: from_me,
            is_group: chat.is_group(),
            chat,
            sender,
            sender_alt,
            id: MessageID::new(web.key.id.clone().unwrap_or_default()),
            timestamp: web.message_timestamp.unwrap_or(0) as i64,
            ..Default::default()
        };
        let mut message = Message { info, raw_message: web.message.clone(), ..Default::default() };
        message.unwrap_raw();
        message
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_to_json_carries_text_and_hex() {
        let msg = WaMessage { conversation: Some("hi \"there\"".into()), ..Default::default() };
        let json = message_to_json(&msg);
        assert!(json.starts_with("{\"text\":\"hi \\\"there\\\"\""), "{json}");
        assert!(json.contains("\"hex\":\"0a0a68692022746865726522"), "{json}");
    }

    #[test]
    fn message_to_json_without_text_is_empty_and_still_has_hex() {
        let json = message_to_json(&WaMessage::default());
        assert_eq!(json, "{\"text\":\"\",\"hex\":\"\"}");
    }

    #[test]
    fn json_escape_handles_control_characters() {
        assert_eq!(json_escape("a\nb\tc"), "\"a\\nb\\tc\"");
        assert_eq!(json_escape("\u{1}"), "\"\\u0001\"");
    }
}
