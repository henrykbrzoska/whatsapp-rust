// Ported from the Go client `armadillomessage.go` (`handleDecryptedArmadillo`),
// © Tulir Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Facebook/Instagram ("armadillo") messages — the `v=3` decrypted envelope.
//!
//! Go's `handleDecryptedArmadillo` unwraps
//! `MessageTransport → SubProtocol(ConsumerApplication)` and walks the
//! consumer-application protobuf tree. The walker is not ported yet (Phase 5);
//! this module decodes the two envelopes so [`WaEvent::FbMessage`] already
//! carries usable hex payloads, and reports the same
//! `(handler_failed, protobuf_failed)` pair Go's caller switches on.

use prost::Message as ProstMessage;
use wa_proto::wa_consumer_application::ConsumerApplication;
use wa_proto::wa_msg_transport::MessageTransport;

use crate::WaEvent;
use crate::types::message::MessageInfo;

use super::Client;

/// Decode the armadillo envelopes of one decrypted `v=3` payload.
///
/// Returns the hex-encoded `MessageTransport` and, when the transport carried
/// an application payload that decodes, the hex-encoded
/// `ConsumerApplication`.
pub fn decode_armadillo(decrypted: &[u8]) -> Result<(String, Option<String>), prost::DecodeError> {
    let transport = MessageTransport::decode(decrypted)?;
    let application = transport
        .payload
        .as_ref()
        .and_then(|payload| payload.application_payload.as_ref())
        .and_then(|sub| sub.payload.as_deref())
        .and_then(|bytes| ConsumerApplication::decode(bytes).ok());
    Ok((hex::encode(transport.encode_to_vec()), application.map(|app| hex::encode(app.encode_to_vec()))))
}

impl Client {
    /// Go `handleDecryptedArmadillo` — returns
    /// `(handler_failed, protobuf_failed)` like the Go function, so the caller in
    /// [`super::message`] keeps Go's ack switching.
    ///
    /// The consumer-application walker (message extraction, edits, delete
    /// tombstones, …) is TODO(Phase 5); the event is emitted with the raw
    /// envelopes so nothing is lost on the way.
    pub fn handle_decrypted_armadillo(&self, info: &MessageInfo, decrypted: &[u8], retry_count: i32) -> (bool, bool) {
        match decode_armadillo(decrypted) {
            Ok((transport, application)) => {
                self.dispatch_event(WaEvent::FbMessage {
                    info: info.clone(),
                    transport: Some(transport),
                    application,
                    retry_count,
                });
                (false, false)
            }
            Err(err) => {
                tracing::warn!("error unmarshaling armadillo message from {}: {err}", info.source_string());
                (false, true)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wa_proto::wa_common::SubProtocol;
    use wa_proto::wa_msg_transport::message_transport::Payload;

    fn transport_with_application(application: Option<Vec<u8>>) -> Vec<u8> {
        let payload = Payload {
            application_payload: application.map(|bytes| SubProtocol { payload: Some(bytes), version: Some(1) }),
            future_proof: None,
        };
        MessageTransport { payload: Some(payload), protocol: None }.encode_to_vec()
    }

    #[test]
    fn decodes_transport_without_application() {
        let (transport, application) = decode_armadillo(&transport_with_application(None)).expect("decode");
        assert!(!transport.is_empty());
        assert!(application.is_none());
    }

    #[test]
    fn decodes_transport_with_consumer_application() {
        let app = ConsumerApplication::default().encode_to_vec();
        let (_, application) = decode_armadillo(&transport_with_application(Some(app))).expect("decode");
        assert!(application.is_some());
    }

    #[test]
    fn invalid_bytes_are_a_decode_error() {
        assert!(decode_armadillo(&[0xff, 0xff, 0xff]).is_err());
    }
}
