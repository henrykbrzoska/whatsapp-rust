// Ported from the Go client `prekeys.go`, © Tulir Asokan, MPL-2.0 — see
// THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Pre-key management: count query, upload and fetch.
//!
//! Go counterpart: `Go prekeys.go` (`getServerPreKeyCount`,
//! `uploadPreKeys`, `fetchPreKeys`, `preKeyToNode`, `nodeToPreKeyBundle`).
//!
//! Node layouts (Go's exactly):
//!
//! ```text
//! <iq xmlns="encrypt" type="get" to="s.whatsapp.net"><count/></iq>
//!   → <count value="N"/>
//! <iq xmlns="encrypt" type="set" to="s.whatsapp.net">
//!   <registration>4 bytes BE</registration>
//!   <type>05</type>
//!   <identity>32 bytes</identity>
//!   <list><key><id>3 bytes BE</id><value>32 bytes</value></key>…</list>
//!   <skey><id/><value/><signature>64 bytes</signature></skey>
//! </iq>
//! <iq xmlns="encrypt" type="get" to="s.whatsapp.net">
//!   <key><user jid="…" reason="identity"/>…</key>
//! </iq>
//!   → <list><user jid="…"><registration/><type/><identity/><key/><skey/></user>…</list>
//! ```

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::binary::node::{Node, NodeContent};
use crate::crypto::keys::{KeyPair, PreKey};
use crate::crypto::signal::PreKeyBundle;
use crate::types::jid::Jid;

use super::message::{attr_int, attr_jid};
use super::retry_receipt::CURVE25519_KEY_TYPE;
use super::session::{InfoQuery, IqKind};
use super::{Client, ClientError, server_jid};

/// Number of pre-keys uploaded in one regular batch.
/// Go: `the Go client.WantedPreKeyCount`.
pub const WANTED_PRE_KEY_COUNT: u32 = 50;

/// Batch size of the very first upload (empty server and database).
/// Go: the literal `812` in `uploadPreKeys`.
pub const INITIAL_PRE_KEY_COUNT: u32 = 812;

/// Below this many pre-keys (server or database) a new batch is uploaded.
/// Go: `the Go client.MinPreKeyCount`.
pub const MIN_PRE_KEY_COUNT: usize = 5;

/// Go: `cli.lastPreKeyUpload.Add(10 * time.Minute)` race guard.
const PRE_KEY_UPLOAD_RACE_WINDOW: Duration = Duration::from_secs(10 * 60);

/// One device's answer in [`Client::fetch_pre_keys`] (Go `preKeyResp`).
pub type PreKeyResponse = Result<PreKeyBundle, ClientError>;

/// Go `preKeyToNode` — `<key>` (one-time) or `<skey>` (signed) with `<id>`
/// (3-byte big-endian key ID), `<value>` and, when signed, `<signature>`.
pub fn pre_key_to_node(key: &PreKey) -> Node {
    let id = key.key_id.to_be_bytes();
    let mut children = vec![
        Node::new("id").with_content(id[1..].to_vec()),
        Node::new("value").with_content(key.key_pair.public.to_vec()),
    ];
    let tag = match key.signature.as_ref() {
        Some(signature) => {
            children.push(Node::new("signature").with_content(signature.to_vec()));
            "skey"
        }
        None => "key",
    };
    Node::new(tag).with_content(children)
}

/// Go `preKeysToNodes`.
pub fn pre_keys_to_nodes(keys: &[PreKey]) -> Vec<Node> {
    keys.iter().map(pre_key_to_node).collect()
}

/// The byte content of a required child, or a [`ClientError::PreKey`].
fn child_bytes<'a>(node: &'a Node, tag: &str, what: &str) -> Result<&'a [u8], ClientError> {
    let child = node
        .optional_child_by_tag(&[tag])
        .ok_or_else(|| ClientError::PreKey(format!("{what} node doesn't contain {tag} tag")))?;
    match &child.content {
        NodeContent::Bytes(bytes) => Ok(bytes),
        _ => Err(ClientError::PreKey(format!("{what} {tag} has unexpected content"))),
    }
}

/// A fixed-size byte child (`<value>` = 32, `<signature>` = 64, …).
fn child_array<const N: usize>(node: &Node, tag: &str, what: &str) -> Result<[u8; N], ClientError> {
    let bytes = child_bytes(node, tag, what)?;
    <[u8; N]>::try_from(bytes).map_err(|_| {
        ClientError::PreKey(format!("{what} {tag} has unexpected number of bytes ({}, expected {N})", bytes.len()))
    })
}

/// Go `nodeToPreKey` — a `<key>`/`<skey>` node. The private half is unknown,
/// so the returned key pair carries a zero private key.
pub fn node_to_pre_key(node: &Node) -> Result<PreKey, ClientError> {
    let id: [u8; 3] = child_array(node, "id", "prekey")?;
    let public: [u8; 32] = child_array(node, "value", "prekey")?;
    let signature = if node.tag == "skey" { Some(child_array::<64>(node, "signature", "prekey")?) } else { None };
    Ok(PreKey {
        key_pair: KeyPair { private: [0u8; 32], public },
        key_id: u32::from_be_bytes([0, id[0], id[1], id[2]]),
        signature,
    })
}

/// Go `nodeToPreKeyBundle` — one `<user>` child of the fetch response.
pub fn node_to_pre_key_bundle(device_id: u32, node: &Node) -> Result<PreKeyBundle, ClientError> {
    if let Some(error) = node.optional_child_by_tag(&["error"]) {
        return Err(ClientError::PreKey(format!("got error getting prekeys: {error:?}")));
    }
    let registration: [u8; 4] = child_array(node, "registration", "prekey response")
        .map_err(|_| ClientError::PreKey("invalid registration ID in prekey response".into()))?;
    let keys = node.optional_child_by_tag(&["keys"]).unwrap_or(node);
    let identity_key: [u8; 32] = child_array(keys, "identity", "prekey response")
        .map_err(|_| ClientError::PreKey("invalid identity key in prekey response".into()))?;
    let pre_key = keys
        .optional_child_by_tag(&["key"])
        .map(node_to_pre_key)
        .transpose()
        .map_err(|err| ClientError::PreKey(format!("invalid prekey in prekey response: {err}")))?;
    let signed = keys
        .optional_child_by_tag(&["skey"])
        .ok_or_else(|| ClientError::PreKey("missing signed prekey in prekey response".into()))
        .and_then(node_to_pre_key)
        .map_err(|err| ClientError::PreKey(format!("invalid signed prekey in prekey response: {err}")))?;
    let signed_pre_key_signature =
        signed.signature.ok_or_else(|| ClientError::PreKey("signed prekey has no signature".into()))?;
    Ok(PreKeyBundle {
        registration_id: u32::from_be_bytes(registration),
        device_id,
        pre_key_id: pre_key.as_ref().map(|k| k.key_id),
        pre_key_public: pre_key.as_ref().map(|k| k.key_pair.public),
        signed_pre_key_id: signed.key_id,
        signed_pre_key_public: signed.key_pair.public,
        signed_pre_key_signature,
        identity_key,
    })
}

/// The `<iq xmlns="encrypt">` query every pre-key request uses.
fn encrypt_query(kind: IqKind, content: Vec<Node>) -> InfoQuery {
    InfoQuery { namespace: "encrypt".into(), kind, to: Some(server_jid()), content, ..Default::default() }
}

impl Client {
    /// Go: `getServerPreKeyCount` — how many one-time pre-keys the server
    /// still holds for us.
    pub async fn get_server_pre_key_count(&self) -> Result<usize, ClientError> {
        let resp = self.send_iq(encrypt_query(IqKind::Get, vec![Node::new("count")])).await?;
        let count = resp
            .optional_child_by_tag(&["count"])
            .ok_or_else(|| ClientError::PreKey("missing <count> in prekey count response".into()))?;
        let value = attr_int(count, "value").ok_or_else(|| ClientError::PreKey("missing `value` in <count>".into()))?;
        usize::try_from(value).map_err(|_| ClientError::PreKey(format!("negative prekey count {value}")))
    }

    /// Go: `uploadPreKeys` — upload a batch of one-time pre-keys (812 on the
    /// very first upload, 50 otherwise) plus our signed pre-key, then mark
    /// them as uploaded.
    ///
    /// The `last_pre_key_upload` mutex doubles as Go's `uploadPreKeysLock`.
    pub async fn upload_pre_keys(&self, initial_upload: bool) -> Result<(), ClientError> {
        let mut last_upload = self.last_pre_key_upload.lock().await;
        if last_upload.is_some_and(|at| at.elapsed() < PRE_KEY_UPLOAD_RACE_WINDOW) {
            let server_count = self.get_server_pre_key_count().await.unwrap_or(0);
            if server_count >= WANTED_PRE_KEY_COUNT as usize {
                tracing::debug!("canceling prekey upload request due to likely race condition");
                return Ok(());
            }
        }
        let (registration_id, identity, signed_pre_key, pre_key_store) = {
            let device = self.device.lock().await;
            (device.registration_id, device.identity_key.public, device.signed_pre_key.clone(), device.pre_keys.clone())
        };
        let wanted = if initial_upload { INITIAL_PRE_KEY_COUNT } else { WANTED_PRE_KEY_COUNT };
        let pre_keys = pre_key_store.get_or_gen_pre_keys(wanted).await?;
        let Some(last_id) = pre_keys.last().map(|k| k.key_id) else {
            return Err(ClientError::PreKey("prekey store returned no keys to upload".into()));
        };
        tracing::info!("uploading {} new prekeys to server", pre_keys.len());
        self.send_iq(encrypt_query(
            IqKind::Set,
            vec![
                Node::new("registration").with_content(registration_id.to_be_bytes().to_vec()),
                Node::new("type").with_content(vec![CURVE25519_KEY_TYPE]),
                Node::new("identity").with_content(identity.to_vec()),
                Node::new("list").with_content(pre_keys_to_nodes(&pre_keys)),
                pre_key_to_node(&signed_pre_key),
            ],
        ))
        .await?;
        tracing::debug!("got response to uploading prekeys");
        pre_key_store.mark_pre_keys_as_uploaded(last_id).await?;
        *last_upload = Some(Instant::now());
        Ok(())
    }

    /// Go: `fetchPreKeys` — the pre-key bundles of `users` (one entry per
    /// `<user>` in the response; a per-device failure is kept in the map).
    pub async fn fetch_pre_keys(&self, users: &[Jid]) -> Result<HashMap<Jid, PreKeyResponse>, ClientError> {
        let requests: Vec<Node> =
            users.iter().map(|user| Node::new("user").attr("jid", user.clone()).attr("reason", "identity")).collect();
        let resp = self.send_iq(encrypt_query(IqKind::Get, vec![Node::new("key").with_content(requests)])).await?;
        if resp.children().is_empty() {
            return Err(ClientError::PreKey("got empty response to prekey request".into()));
        }
        let list = resp
            .optional_child_by_tag(&["list"])
            .ok_or_else(|| ClientError::PreKey("missing <list> in prekey response".into()))?;
        let mut out = HashMap::new();
        for child in list.children_by_tag("user") {
            let Some(jid) = attr_jid(child, "jid") else {
                tracing::warn!("prekey response <user> without a jid");
                continue;
            };
            let bundle = node_to_pre_key_bundle(u32::from(jid.device), child);
            out.insert(jid, bundle);
        }
        Ok(out)
    }

    /// Go: `fetchPreKeysNoError` — like [`Self::fetch_pre_keys`] but logs
    /// failures and only returns the bundles that parsed.
    pub async fn fetch_pre_keys_no_error(&self, devices: &[Jid]) -> HashMap<Jid, PreKeyBundle> {
        if devices.is_empty() {
            return HashMap::new();
        }
        let responses = match self.fetch_pre_keys(devices).await {
            Ok(responses) => responses,
            Err(err) => {
                tracing::warn!("failed to fetch prekeys for {devices:?} with no existing session: {err}");
                return HashMap::new();
            }
        };
        let mut bundles = HashMap::with_capacity(responses.len());
        for (jid, resp) in responses {
            match resp {
                Ok(bundle) => {
                    bundles.insert(jid, bundle);
                }
                Err(err) => tracing::warn!("failed to fetch prekey for {jid}: {err}"),
            }
        }
        bundles
    }

    /// The pre-key half of Go's `handleConnectSuccess` goroutine: top up the
    /// server when it (or the database) runs below [`MIN_PRE_KEY_COUNT`].
    pub async fn ensure_pre_keys_uploaded(&self) {
        let pre_key_store = self.device.lock().await.pre_keys.clone();
        let db_count = match pre_key_store.uploaded_pre_key_count().await {
            Ok(count) => count,
            Err(err) => {
                tracing::error!("failed to get number of prekeys in database: {err}");
                return;
            }
        };
        let server_count = match self.get_server_pre_key_count().await {
            Ok(count) => count,
            Err(err) => {
                tracing::warn!("failed to get number of prekeys on server: {err}");
                return;
            }
        };
        tracing::debug!("database has {db_count} prekeys, server says we have {server_count}");
        if server_count < MIN_PRE_KEY_COUNT || db_count < MIN_PRE_KEY_COUNT {
            if let Err(err) = self.upload_pre_keys(db_count == 0 && server_count == 0).await {
                tracing::error!("failed to upload prekeys: {err}");
            }
            match self.get_server_pre_key_count().await {
                Ok(count) => tracing::debug!("prekey count after upload: {count}"),
                Err(err) => tracing::debug!("failed to get prekey count after upload: {err}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signed_key() -> PreKey {
        KeyPair::generate().create_signed_pre_key(0x0001_0203)
    }

    #[test]
    fn pre_key_to_node_matches_go_layout() {
        let key = PreKey::generate(0x00AB_CDEF);
        let node = pre_key_to_node(&key);
        assert_eq!(node.tag, "key");
        let tags: Vec<&str> = node.children().iter().map(|c| c.tag.as_str()).collect();
        assert_eq!(tags, ["id", "value"]);
        assert_eq!(node.children()[0].as_bytes(), Some(&[0xAB, 0xCD, 0xEF][..]));
        assert_eq!(node.children()[1].as_bytes(), Some(key.key_pair.public.as_slice()));

        let signed = signed_key();
        let node = pre_key_to_node(&signed);
        assert_eq!(node.tag, "skey");
        let tags: Vec<&str> = node.children().iter().map(|c| c.tag.as_str()).collect();
        assert_eq!(tags, ["id", "value", "signature"]);
        assert_eq!(node.children()[2].as_bytes().map(<[u8]>::len), Some(64));
    }

    #[test]
    fn node_to_pre_key_round_trips() {
        let signed = signed_key();
        let parsed = node_to_pre_key(&pre_key_to_node(&signed)).expect("parse");
        assert_eq!(parsed.key_id, signed.key_id);
        assert_eq!(parsed.key_pair.public, signed.key_pair.public);
        assert_eq!(parsed.signature, signed.signature);

        let bad = Node::new("key").with_content(vec![Node::new("id").with_content(vec![1u8, 2])]);
        assert!(node_to_pre_key(&bad).is_err(), "2-byte id is rejected");
    }

    fn user_node(with_one_time: bool) -> (Node, PreKey, PreKey, [u8; 32]) {
        let identity = KeyPair::generate();
        let signed = identity.create_signed_pre_key(7);
        let one_time = PreKey::generate(42);
        let mut keys = vec![
            Node::new("registration").with_content(0x1234_5678u32.to_be_bytes().to_vec()),
            Node::new("type").with_content(vec![CURVE25519_KEY_TYPE]),
            Node::new("identity").with_content(identity.public.to_vec()),
        ];
        if with_one_time {
            keys.push(pre_key_to_node(&one_time));
        }
        keys.push(pre_key_to_node(&signed));
        (Node::new("user").attr("jid", "4911:3@s.whatsapp.net").with_content(keys), one_time, signed, identity.public)
    }

    #[test]
    fn node_to_pre_key_bundle_parses_both_shapes() {
        let (node, one_time, signed, identity) = user_node(true);
        let bundle = node_to_pre_key_bundle(3, &node).expect("bundle");
        assert_eq!(bundle.registration_id, 0x1234_5678);
        assert_eq!(bundle.device_id, 3);
        assert_eq!(bundle.pre_key_id, Some(42));
        assert_eq!(bundle.pre_key_public, Some(one_time.key_pair.public));
        assert_eq!(bundle.signed_pre_key_id, 7);
        assert_eq!(bundle.signed_pre_key_public, signed.key_pair.public);
        assert_eq!(Some(bundle.signed_pre_key_signature), signed.signature);
        assert_eq!(bundle.identity_key, identity);

        let (node, _, _, _) = user_node(false);
        let bundle = node_to_pre_key_bundle(0, &node).expect("bundle without one-time key");
        assert_eq!(bundle.pre_key_id, None);
        assert_eq!(bundle.pre_key_public, None);

        let error = Node::new("user").with_content(vec![Node::new("error").attr("code", "404")]);
        assert!(node_to_pre_key_bundle(0, &error).is_err());
    }

    #[tokio::test]
    async fn pre_key_requests_fail_cleanly_without_a_socket() {
        let container = crate::store::sqlite::Container::open_memory().await.expect("memory");
        let client = Client::new(
            container.new_device(),
            super::super::ClientConfig::default(),
            std::sync::Arc::new(|_: crate::WaEvent| {}),
        );
        assert!(client.get_server_pre_key_count().await.is_err());
        assert!(client.fetch_pre_keys(&[Jid::new("1", "s.whatsapp.net")]).await.is_err());
        assert!(client.fetch_pre_keys_no_error(&[]).await.is_empty());
        // Must neither panic nor upload when the count query fails.
        client.ensure_pre_keys_uploaded().await;
    }
}
