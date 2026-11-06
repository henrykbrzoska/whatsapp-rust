// Ported from the Go client `mediaretry.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Media retry receipts — ask the primary phone to re-upload media we could
//! not download (`SendMediaRetryReceipt`), and handle the re-upload
//! notification (`handleMediaRetryNotification`).
//!
//! Go counterparts: `Go mediaretry.go` (`SendMediaRetryReceipt`,
//! `DecryptMediaRetryNotification`, `parseMediaRetryNotification`,
//! `handleMediaRetryNotification`, `encryptMediaRetryReceipt`,
//! `getMediaRetryKey`). The notification decryption needs the media key and
//! the `waMmsRetry` proto; the wire parse + event dispatch below are complete,
//! the AES-GCM decrypt helper is wired to `crypto::gcm` (Task E owns the
//! media-key derivation).

use tracing::warn;

use crate::binary::node::Node;
use crate::crypto::gcm;
use crate::types::message::MessageInfo;

use super::ack::attr_text;
use super::message::{attr_int, attr_jid};
use super::{Client, ClientError};

/// Go `getMediaRetryKey` — the AES-256 key for a media-retry notification is
/// `HKDF-SHA256(mediaKey, 32B, info="Media Retry Notification")`.
pub fn media_retry_key(media_key: &[u8]) -> Vec<u8> {
    crate::crypto::hkdf::hkdf_expand(media_key, b"Media Retry Notification", 32)
}

/// Go `encryptMediaRetryReceipt` — encrypt `message_id` with `media_key`
/// (AES-GCM, random 12-byte IV, `message_id` as AAD).
pub fn encrypt_media_retry_receipt(message_id: &str, media_key: &[u8]) -> Result<(Vec<u8>, Vec<u8>), ClientError> {
    let key = media_retry_key(media_key);
    let mut iv = [0u8; gcm::NONCE_LEN];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut iv);
    let ciphertext = gcm::encrypt(&key, &iv, message_id.as_bytes(), message_id.as_bytes())
        .map_err(|err| ClientError::Crypto(err.to_string()))?;
    Ok((ciphertext, iv.to_vec()))
}

/// Go `DecryptMediaRetryNotification` — decrypt `ciphertext` with `media_key`.
pub fn decrypt_media_retry_notification(
    ciphertext: &[u8],
    iv: &[u8],
    message_id: &str,
    media_key: &[u8],
) -> Result<Vec<u8>, ClientError> {
    let key = media_retry_key(media_key);
    let nonce: [u8; gcm::NONCE_LEN] =
        iv.try_into().map_err(|_| ClientError::Crypto("media retry IV is not 12 bytes".to_owned()))?;
    gcm::decrypt(&key, &nonce, ciphertext, message_id.as_bytes()).map_err(|err| ClientError::Crypto(err.to_string()))
}

impl Client {
    /// Go `SendMediaRetryReceipt` — ask the primary phone to re-upload the
    /// media of `message` (a `server-error` receipt to ourselves with the
    /// encrypted request).
    pub async fn send_media_retry_receipt(&self, message: &MessageInfo, media_key: &[u8]) -> Result<(), ClientError> {
        if !self.is_logged_in() {
            return Err(ClientError::NotLoggedIn);
        }
        let (ciphertext, iv) = encrypt_media_retry_receipt(message.id.as_str(), media_key)?;
        let own_id = self.own_id().await.to_non_ad();
        let mut rmr = Node::new("rmr").attr("jid", message.chat.clone()).attr("from_me", message.is_from_me);
        if message.is_group {
            rmr = rmr.attr("participant", message.sender.clone());
        }
        let node = Node::new("receipt")
            .attr("id", message.id.as_str())
            .attr("to", own_id)
            .attr("type", "server-error")
            .with_content(vec![
                Node::new("encrypt").with_content(vec![
                    Node::new("enc_p").with_content(ciphertext),
                    Node::new("enc_iv").with_content(iv),
                ]),
                rmr,
            ]);
        self.send_node(&node).await
    }

    /// Go `handleMediaRetryNotification` — parse a media-retry notification
    /// and emit `WaEvent::MediaRetry`; parse failures are logged, never fatal.
    pub async fn handle_media_retry_notification(&self, node: &Node) {
        match parse_media_retry_notification(node) {
            Ok(evt) => {
                self.dispatch_event(evt);
            }
            Err(err) => warn!("Failed to parse media retry notification: {err}"),
        }
    }
}

/// Go `parseMediaRetryNotification` — parse the notification into a
/// `WaEvent::MediaRetry`.
pub fn parse_media_retry_notification(node: &Node) -> Result<crate::WaEvent, ClientError> {
    let message_id =
        attr_text(node, "id").ok_or(ClientError::MissingAttribute { tag: node.tag.clone(), attr: "id" })?;
    let rmr = node
        .children()
        .iter()
        .find(|c| c.tag == "rmr")
        .ok_or(ClientError::MissingElement { tag: "rmr", parent: "retry notification" })?;
    let chat = attr_jid(rmr, "jid").ok_or(ClientError::MissingAttribute { tag: "rmr".to_owned(), attr: "jid" })?;
    let from_me = rmr.attrs.get("from_me").is_some_and(|v| v.to_string() == "true" || v.to_string() == "1");
    let sender = attr_jid(rmr, "participant").unwrap_or_default();
    if let Some(error) = node.children().iter().find(|c| c.tag == "error") {
        return Ok(crate::WaEvent::MediaRetry {
            message_id,
            chat: chat.to_string(),
            sender: sender.to_string(),
            is_from_me: from_me,
            timestamp: attr_int(node, "t").unwrap_or(0),
            ciphertext: None,
            iv: None,
            error_code: Some(attr_int(error, "code").unwrap_or(0) as i32),
        });
    }
    let encrypt = node
        .children()
        .iter()
        .find(|c| c.tag == "encrypt")
        .ok_or(ClientError::MissingElement { tag: "encrypt", parent: "retry notification" })?;
    let ciphertext = encrypt.children().iter().find(|c| c.tag == "enc_p").and_then(|c| c.as_bytes().map(hex::encode));
    let iv = encrypt.children().iter().find(|c| c.tag == "enc_iv").and_then(|c| c.as_bytes().map(hex::encode));
    if ciphertext.is_none() {
        return Err(ClientError::MissingElement { tag: "enc_p", parent: "retry notification" });
    }
    if iv.is_none() {
        return Err(ClientError::MissingElement { tag: "enc_iv", parent: "retry notification" });
    }
    Ok(crate::WaEvent::MediaRetry {
        message_id,
        chat: chat.to_string(),
        sender: sender.to_string(),
        is_from_me: from_me,
        timestamp: attr_int(node, "t").unwrap_or(0),
        ciphertext,
        iv,
        error_code: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_retry_receipt_roundtrip() {
        let media_key = vec![0x42u8; 32];
        let (ciphertext, iv) = encrypt_media_retry_receipt("MSGID123", &media_key).unwrap();
        assert_eq!(iv.len(), gcm::NONCE_LEN);
        let plaintext = decrypt_media_retry_notification(&ciphertext, &iv, "MSGID123", &media_key).unwrap();
        assert_eq!(plaintext, b"MSGID123");
        // Wrong key fails.
        assert!(decrypt_media_retry_notification(&ciphertext, &iv, "MSGID123", &[0x11u8; 32]).is_err());
    }

    #[test]
    fn media_retry_notification_parses() {
        let node = Node::new("receipt").attr("id", "MSG1").attr("t", 99_i64).with_content(vec![
            Node::new("encrypt").with_content(vec![
                Node::new("enc_p").with_content(vec![1u8, 2, 3]),
                Node::new("enc_iv").with_content(vec![4u8, 5, 6]),
            ]),
            Node::new("rmr").attr("jid", crate::types::jid::Jid::new("123", "s.whatsapp.net")).attr("from_me", true),
        ]);
        let evt = parse_media_retry_notification(&node).unwrap();
        match evt {
            crate::WaEvent::MediaRetry { message_id, error_code, .. } => {
                assert_eq!(message_id, "MSG1");
                assert_eq!(error_code, None);
            }
            _ => panic!("wrong event"),
        }
        // Malformed: no rmr.
        let bad = Node::new("receipt").attr("id", "MSG1");
        assert!(parse_media_retry_notification(&bad).is_err());
    }
}
