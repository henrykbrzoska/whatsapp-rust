// Ported from the Go client `pair.go` + `pair-code.go` (constants) +
// `qrchan.go` (rotation policy), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0.

//! Pairing: the `pair-device` / `pair-success` handshake, ADV secret rotation
//! and the QR channel (ports of `Go pair.go`, the `PairClientType`
//! constants of `pair-code.go` and the rotation policy of `qrchan.go`).
//!
//! `POST /pair/code` (code pairing, passkey linking) is out of scope for
//! Phase 3: `TODO(Phase 7)` (`generateCompanionEphemeralKey`,
//! `handleCodePairNotification`, `PairPhone`).

use std::sync::Arc;
use std::time::Duration;

use hmac::{Hmac, Mac};
use prost::Message;
use rand::RngCore;
use sha2::Sha256;
use tokio::sync::mpsc;
use wa_proto::wa_adv::{AdvDeviceIdentity, AdvEncryptionType, AdvSignedDeviceIdentity, AdvSignedDeviceIdentityHmac};

use super::{Client, ClientError};
use crate::binary::node::{AttrValue, Node, NodeContent};
use crate::crypto::keys::{KeyPair, calculate_signature, verify_signature};
use crate::types::jid::Jid;

/// Go `pair.go` — `ADVAccountSignaturePrefix = {6, 0}`.
pub const ADV_ACCOUNT_SIGNATURE_PREFIX: [u8; 2] = [6, 0];
/// Go `pair.go` — `ADVDeviceSignaturePrefix = {6, 1}`.
pub const ADV_DEVICE_SIGNATURE_PREFIX: [u8; 2] = [6, 1];
/// Go `pair.go` — `ADVHostedAccountSignaturePrefix = {6, 5}`.
pub const ADV_HOSTED_ACCOUNT_SIGNATURE_PREFIX: [u8; 2] = [6, 5];

/// Go `pair.go` — `ADVSecretLength = 32`.
pub const ADV_SECRET_LENGTH: usize = 32;
/// Go `pair.go` — `ADVKeyIndexLength = 4`.
pub const ADV_KEY_INDEX_LENGTH: usize = 4;
/// Go `pair.go` — `ADVSecretJID = "adv"`.
pub const ADV_SECRET_JID: &str = "adv";

/// Go `qrchan.go` — a QR code stays valid for 20 s (`QRChannelTimeout`).
pub const QR_CHANNEL_TIMEOUT: Duration = Duration::from_secs(20);
/// Go `qrchan.go` — the server rotates the `ref` roughly every 8 s
/// (`QRChannelUnknownEvent`); the channel closes after 3 rotations.
pub const QR_ROTATION_LIMIT: u32 = 3;

/// Go `pair-code.go` — `PairClientType` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairClientType {
    Unknown,
    Chrome,
    Edge,
    Firefox,
    IE,
    Opera,
    Safari,
    Electron,
    UWP,
    OtherWebClient,
    MacOS,
    Android,
}

impl PairClientType {
    /// Go `PairClientType.String()` — the wire value (`"0"`–`"9"`, `"c"`, `"e"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "0",
            Self::Chrome => "1",
            Self::Edge => "2",
            Self::Firefox => "3",
            Self::IE => "4",
            Self::Opera => "5",
            Self::Safari => "6",
            Self::Electron => "7",
            Self::UWP => "8",
            Self::OtherWebClient => "9",
            Self::MacOS => "c",
            Self::Android => "e",
        }
    }

    /// Go `getPairClientType` — from the `DeviceProps` platform type
    /// (`wa_companion_reg::device_props::PlatformType`).
    pub fn from_user_agent_platform(platform: i32) -> Self {
        use wa_proto::wa_companion_reg::device_props::PlatformType;
        match platform {
            x if x == PlatformType::Chrome as i32 => Self::Chrome,
            x if x == PlatformType::Edge as i32 => Self::Edge,
            x if x == PlatformType::Firefox as i32 => Self::Firefox,
            x if x == PlatformType::Ie as i32 => Self::IE,
            x if x == PlatformType::Opera as i32 => Self::Opera,
            x if x == PlatformType::Safari as i32 => Self::Safari,
            x if x == PlatformType::Desktop as i32 => Self::Electron,
            x if x == PlatformType::Uwp as i32 => Self::UWP,
            x if x == PlatformType::Catalina as i32 => Self::MacOS,
            x if x == PlatformType::AndroidPhone as i32 => Self::Android,
            _ => Self::Unknown,
        }
    }
}

impl std::fmt::Display for PairClientType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Go `concatBytes`.
pub fn concat_bytes(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(parts.iter().map(|part| part.len()).sum());
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

/// Go `verifyAccountSignature` — `account_signature_key` must be 32 bytes,
/// `account_signature` 64; the message is `prefix ‖ details ‖ ikp.pub`.
pub fn verify_account_signature(identity: &AdvSignedDeviceIdentity, identity_key: &KeyPair, hosted: bool) -> bool {
    let Some(key) = identity.account_signature_key.as_deref().and_then(|key| <&[u8; 32]>::try_from(key).ok()) else {
        return false;
    };
    let Some(signature) =
        identity.account_signature.as_deref().and_then(|signature| <&[u8; 64]>::try_from(signature).ok())
    else {
        return false;
    };
    let prefix: &[u8] = if hosted { &ADV_HOSTED_ACCOUNT_SIGNATURE_PREFIX } else { &ADV_ACCOUNT_SIGNATURE_PREFIX };
    let message =
        concat_bytes(&[prefix, identity.details.as_deref().unwrap_or_default(), identity_key.public.as_slice()]);
    verify_signature(key, &message, signature)
}

/// Go `generateDeviceSignature` — `details ‖ ikp.pub ‖ account_signature_key`
/// under the `[6, 1]` prefix, signed by the identity key.
pub fn generate_device_signature(identity: &AdvSignedDeviceIdentity, identity_key: &KeyPair) -> [u8; 64] {
    let message = concat_bytes(&[
        ADV_DEVICE_SIGNATURE_PREFIX.as_slice(),
        identity.details.as_deref().unwrap_or_default(),
        identity_key.public.as_slice(),
        identity.account_signature_key.as_deref().unwrap_or_default(),
    ]);
    let mut random = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut random);
    calculate_signature(&identity_key.private, &message, &random)
}

/// Go `pair.go` HMAC check — `HMAC-SHA256(adv_secret, [prefix] ‖ details)`
/// (the `[6, 5]` prefix only for hosted accounts), compared in constant time.
pub fn verify_device_identity_hmac(container: &AdvSignedDeviceIdentityHmac, adv_secret_key: &[u8]) -> bool {
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(adv_secret_key) else {
        return false;
    };
    if container.account_type == Some(AdvEncryptionType::Hosted as i32) {
        mac.update(&ADV_HOSTED_ACCOUNT_SIGNATURE_PREFIX);
    }
    mac.update(container.details.as_deref().unwrap_or_default());
    mac.verify_slice(container.hmac.as_deref().unwrap_or_default()).is_ok()
}

/// Go `base64.StdEncoding.EncodeToString` (padded standard alphabet).
pub(crate) fn base64_std(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Child by tag (`pair-success` → `device-identity`, …).
fn child<'a>(node: &'a Node, tag: &str) -> Option<&'a Node> {
    node.optional_child_by_tag(&[tag])
}

/// Raw-bytes child (`device-identity`, `client-props`) or `None`.
fn child_bytes(node: &Node, tag: &str) -> Option<Vec<u8>> {
    child(node, tag).and_then(Node::as_bytes).map(<[u8]>::to_vec)
}

/// A string attr (`biz@name`, `platform@name`) or `""`.
fn attr_string(node: &Node, attr: &str) -> String {
    match node.attrs.get(attr) {
        Some(AttrValue::Text(text)) => text.clone(),
        Some(AttrValue::Jid(jid)) => jid.to_string(),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

/// A JID attr (`device@jid`, `device@lid`, `iq@from`) — wire nodes carry
/// `AttrValue::Jid`, hand-built nodes may carry text.
fn attr_jid(node: &Node, attr: &str) -> Jid {
    match node.attrs.get(attr) {
        Some(AttrValue::Jid(jid)) => jid.clone(),
        Some(AttrValue::Text(text)) => text.parse().unwrap_or_default(),
        _ => Jid::default(),
    }
}

/// A unix-seconds attr (`t`) or `None`.
fn attr_unix(node: &Node, attr: &str) -> Option<i64> {
    match node.attrs.get(attr)? {
        AttrValue::Text(text) => text.parse().ok(),
        AttrValue::Int(value) => Some(*value),
        AttrValue::Uint(value) => i64::try_from(*value).ok(),
        _ => None,
    }
}

impl Client {
    /// Go `handleIQ` — only `from == s.whatsapp.net` with exactly one child;
    /// dispatches `pair-device` / `pair-success`, ignores everything else.
    pub async fn handle_iq(self: &Arc<Self>, node: &Node) {
        let children = node.children();
        if node.tag != "iq" || children.len() != 1 || attr_jid(node, "from") != super::server_jid() {
            tracing::debug!("ignoring iq node {}", node.to_xml_string());
            return;
        }
        match children[0].tag.as_str() {
            "pair-device" => self.handle_pair_device(node).await,
            "pair-success" => self.handle_pair_success(node).await,
            other => tracing::debug!("unhandled iq child {other}"),
        }
    }

    /// Go `getQRClientType` — `DeviceProps.platform_type` first, then the
    /// `ClientPayload` user-agent platform. Our props report `Unknown` on a
    /// web platform, so this yields [`PairClientType::OtherWebClient`].
    pub fn qr_client_type(&self) -> PairClientType {
        use wa_proto::wa_companion_reg::device_props::PlatformType;
        use wa_proto::wa_web_protobufs_wa6::client_payload::user_agent::Platform;
        let platform_type = crate::types::version::device_props().platform_type.unwrap_or_default();
        let by_props = [
            (PlatformType::Chrome, PairClientType::Chrome),
            (PlatformType::Firefox, PairClientType::Firefox),
            (PlatformType::Edge, PairClientType::Edge),
            (PlatformType::Ie, PairClientType::IE),
            (PlatformType::Opera, PairClientType::Opera),
            (PlatformType::Safari, PairClientType::Safari),
            (PlatformType::Uwp, PairClientType::UWP),
            (PlatformType::AndroidPhone, PairClientType::Android),
        ];
        if let Some((_, client_type)) = by_props.iter().find(|(props, _)| *props as i32 == platform_type) {
            return *client_type;
        }
        let payload = crate::types::version::build_client_payload(crate::types::version::WA_VERSION);
        match payload.user_agent.and_then(|agent| agent.platform) {
            Some(p) if p == Platform::Web as i32 => PairClientType::OtherWebClient,
            Some(p) if p == Platform::Macos as i32 => PairClientType::MacOS,
            _ => PairClientType::Unknown,
        }
    }

    /// Go `makeQRData` —
    /// `https://wa.me/settings/linked_devices#{ref},{noise},{identity},{adv},{type}`
    /// with standard base64 of the noise/identity public keys and the ADV secret.
    pub async fn make_qr_data(&self, reference: &[u8], client_type: PairClientType) -> String {
        let device = self.device.lock().await;
        format!(
            "https://wa.me/settings/linked_devices#{},{},{},{},{}",
            String::from_utf8_lossy(reference),
            base64_std(&device.noise_key.public),
            base64_std(&device.identity_key.public),
            base64_std(&device.adv_secret_key),
            client_type,
        )
    }

    /// Build the codes for `refs` and emit them as one `Qr` event, and as
    /// `QRChannelItem::Code`s on a live `qr_channel()`.
    async fn emit_qr_codes(&self, refs: &[Vec<u8>]) {
        let client_type = self.qr_client_type();
        let mut codes = Vec::with_capacity(refs.len());
        for reference in refs {
            codes.push(self.make_qr_data(reference, client_type).await);
        }
        if let Some(tx) = self.qr_tx.lock().await.as_ref() {
            for code in &codes {
                if tx.try_send(QRChannelItem::Code(code.clone())).is_err() {
                    tracing::debug!("qr channel is full or closed, dropping a code");
                }
            }
        }
        self.dispatch_event(crate::WaEvent::Qr { codes });
    }

    /// Go `handlePairDevice` — `paired = false`, ack the iq (`type=result`,
    /// same `to`/`id`), then emit **every** `ref` as a QR code in one event.
    pub async fn handle_pair_device(self: &Arc<Self>, node: &Node) {
        self.paired.store(false, std::sync::atomic::Ordering::SeqCst);
        let mut ack = Node::new("iq").attr("type", "result");
        if let Some(from) = node.attrs.get("from") {
            ack = ack.attr("to", from.clone());
        }
        if let Some(id) = node.attrs.get("id") {
            ack = ack.attr("id", id.clone());
        }
        if let Err(err) = self.send_node(&ack).await {
            tracing::warn!("failed to send acknowledgement for pair-device request: {err}");
        }

        let Some(pair_device) = child(node, "pair-device") else {
            tracing::warn!("iq node is missing pair-device");
            return;
        };
        let mut refs = Vec::new();
        for (index, item) in pair_device.children().iter().enumerate() {
            if item.tag != "ref" {
                tracing::warn!("pair-device node contains unexpected child tag {} at index {index}", item.tag);
                continue;
            }
            match &item.content {
                NodeContent::Bytes(bytes) => refs.push(bytes.clone()),
                NodeContent::Text(text) => refs.push(text.as_bytes().to_vec()),
                _ => tracing::warn!("pair-device node contains unexpected child content at index {index}"),
            }
        }
        *self.qr_refs.lock().await = refs.clone();
        self.emit_qr_codes(&refs).await;
    }

    /// Go `handlePairSuccess` — parse the `pair-success` child of `node`
    /// (the iq), set the server time offset, then `handle_pair`; on error
    /// `disconnect()` + `PairError`, on success `PairSuccess`.
    ///
    /// Go runs `handlePair` in a goroutine; here it runs inline — it never
    /// waits for a response, so it cannot stall the handler queue.
    pub async fn handle_pair_success(self: &Arc<Self>, node: &Node) {
        if let Some(t) = attr_unix(node, "t") {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or_default();
            self.server_time_offset.store(t - now, std::sync::atomic::Ordering::SeqCst);
        }
        let req_id = attr_string(node, "id");
        let Some(pair_success) = child(node, "pair-success") else {
            tracing::warn!("iq node is missing pair-success");
            return;
        };
        let identity_bytes = child_bytes(pair_success, "device-identity").unwrap_or_default();
        let business_name = child(pair_success, "biz").map(|biz| attr_string(biz, "name")).unwrap_or_default();
        let jid = child(pair_success, "device").map(|device| attr_jid(device, "jid")).unwrap_or_default();
        let lid = child(pair_success, "device").map(|device| attr_jid(device, "lid")).unwrap_or_default();
        let platform = child(pair_success, "platform").map(|p| attr_string(p, "name")).unwrap_or_default();
        if let Some(props) = child_bytes(pair_success, "client-props") {
            match wa_proto::wa_companion_reg::ClientPairingProps::decode(props.as_slice()) {
                Ok(props) => tracing::debug!("client pairing props: {props:?}"),
                Err(err) => tracing::warn!("failed to parse client pairing props: {err}"),
            }
        }

        match self.handle_pair(&identity_bytes, &req_id, &business_name, &platform, jid.clone(), lid.clone()).await {
            Err(err) => {
                tracing::error!("failed to pair device: {err}");
                self.disconnect().await;
                self.dispatch_event(crate::WaEvent::PairError {
                    id: jid.to_string(),
                    lid: lid.to_string(),
                    business_name,
                    platform,
                    error: err.to_string(),
                });
            }
            Ok(()) => {
                tracing::info!("successfully paired {jid}");
                // TODO(Phase 4): sendUnifiedSession + pre-key upload (Go prekeys.go).
                if let Some(tx) = self.qr_tx.lock().await.take() {
                    let _ = tx.try_send(QRChannelItem::Success);
                }
                self.dispatch_event(crate::WaEvent::PairSuccess {
                    id: jid.to_string(),
                    lid: lid.to_string(),
                    business_name,
                    platform,
                });
            }
        }
    }

    /// Go `handlePair` — HMAC check, signature checks, sign the device
    /// identity, persist the device, `expect_disconnect()` and send the
    /// `pair-device-sign` confirmation (`iq type=result id=req_id`).
    pub async fn handle_pair(
        &self,
        identity_bytes: &[u8],
        req_id: &str,
        business_name: &str,
        platform: &str,
        jid: Jid,
        lid: Jid,
    ) -> Result<(), ClientError> {
        // 1. The HMAC container.
        let Ok(container) = AdvSignedDeviceIdentityHmac::decode(identity_bytes) else {
            self.send_pair_error(req_id, 500, "internal-error").await;
            return Err(ClientError::Pair("failed to parse device identity container in pair success message".into()));
        };
        // 2. HMAC over the details with the ADV secret.
        let (adv_secret_key, identity_key) = {
            let device = self.device.lock().await;
            (device.adv_secret_key.clone(), device.identity_key.clone())
        };
        if !verify_device_identity_hmac(&container, &adv_secret_key) {
            tracing::warn!("invalid HMAC from pair success message");
            self.send_pair_error(req_id, 401, "hmac-mismatch").await;
            return Err(ClientError::Pair("invalid device identity HMAC in pair success message".into()));
        }
        // 3. The signed identity and its details.
        let Ok(mut device_identity) = AdvSignedDeviceIdentity::decode(container.details.as_deref().unwrap_or_default())
        else {
            self.send_pair_error(req_id, 500, "internal-error").await;
            return Err(ClientError::Pair("failed to parse signed device identity in pair success message".into()));
        };
        let Ok(details) = AdvDeviceIdentity::decode(device_identity.details.as_deref().unwrap_or_default()) else {
            self.send_pair_error(req_id, 500, "internal-error").await;
            return Err(ClientError::Pair("failed to parse device identity details in pair success message".into()));
        };
        // 4. The account signature.
        let hosted = details.device_type == Some(AdvEncryptionType::Hosted as i32);
        if !verify_account_signature(&device_identity, &identity_key, hosted) {
            self.send_pair_error(req_id, 401, "signature-mismatch").await;
            return Err(ClientError::Pair("invalid device signature in pair success message".into()));
        }
        // 5. Our device signature.
        device_identity.device_signature = Some(generate_device_signature(&device_identity, &identity_key).to_vec());
        // 6. Keep the account (with the account key), strip it for the reply.
        let account = device_identity.clone();
        let mut main_device_lid = lid.clone();
        main_device_lid.device = 0;
        let _main_device_identity = device_identity.account_signature_key.take();
        let self_signed = device_identity.encode_to_vec();

        // 7. Persist — the last step that can fail before the confirmation.
        {
            let mut device = self.device.lock().await;
            device.account = Some(account);
            device.id = Some(jid);
            device.lid = lid;
            device.business_name = business_name.to_owned();
            device.platform = platform.to_owned();
            if let Err(err) = device.save().await {
                drop(device);
                self.send_pair_error(req_id, 500, "internal-error").await;
                return Err(ClientError::Store(err.context("failed to save device store")));
            }
        }
        // 8. TODO(Phase 4): StoreLIDPNMapping(lid, jid) and
        // Identities::put_identity(main_device_lid.signal_address_user(), main_device_identity).
        // Go deletes the device and sends a 500 pair error when the identity put
        // fails — mirror that ordering here once the stores land.

        // 9. The server drops us right after pairing; that close is expected.
        self.expect_disconnect().await;
        self.paired.store(true, std::sync::atomic::Ordering::SeqCst);

        // 10. The confirmation.
        let confirmation = Node::new("iq")
            .attr("to", super::server_jid())
            .attr("type", "result")
            .attr("id", req_id.to_owned())
            .with_content(vec![Node::new("pair-device-sign").with_content(vec![
                Node::new("device-identity")
                    .attr("key-index", u64::from(details.key_index.unwrap_or_default()))
                    .with_content(self_signed),
            ])]);
        if let Err(err) = self.send_node(&confirmation).await {
            if let Err(delete_err) = self.device.lock().await.delete().await {
                tracing::warn!("failed to delete device after a failed pairing confirmation: {delete_err}");
            }
            return Err(ClientError::Pair(format!("failed to send pairing confirmation: {err}")));
        }
        Ok(())
    }

    /// Go `rotateADVSecret` — 32 fresh bytes into `device.adv_secret_key`,
    /// then re-emit the last QR codes with the new secret.
    ///
    /// Unlike Go, a paired device **is** saved here: our store is the only
    /// copy of the secret. An unpaired device has no JID yet (the store
    /// rejects the save); its secret is persisted by the `pair-success` save.
    pub async fn rotate_adv_secret(&self) -> Result<(), ClientError> {
        let mut secret = vec![0u8; ADV_SECRET_LENGTH];
        rand::thread_rng().fill_bytes(&mut secret);
        {
            let mut device = self.device.lock().await;
            device.adv_secret_key = secret;
            if device.id.is_some() {
                device.save().await?;
            }
        }
        let refs = self.qr_refs.lock().await.clone();
        if !refs.is_empty() {
            self.emit_qr_codes(&refs).await;
        }
        Ok(())
    }

    /// Go `sendPairError` — best-effort `iq type=error`.
    pub async fn send_pair_error(&self, id: &str, code: u16, text: &str) {
        let node = Node::new("iq")
            .attr("to", super::server_jid())
            .attr("type", "error")
            .attr("id", id.to_owned())
            .with_content(vec![Node::new("error").attr("code", u64::from(code)).attr("text", text.to_owned())]);
        if let Err(err) = self.send_node(&node).await {
            tracing::error!("failed to send pair error node: {err}");
        }
    }

    /// Go `GetQRChannel` — the QR code stream. Must be called *before*
    /// `connect()` on an unpaired device. Every `pair-device` node pushes all
    /// its codes as [`QRChannelItem::Code`]; `pair-success` sends
    /// [`QRChannelItem::Success`]. The channel closes after
    /// [`QR_ROTATION_LIMIT`] rotation windows (Go `QRChannelTimeout`).
    pub async fn qr_channel(self: &Arc<Self>) -> Result<mpsc::Receiver<QRChannelItem>, ClientError> {
        if self.is_connected().await {
            return Err(ClientError::Pair("GetQRChannel must be called before connecting".into()));
        }
        if self.device.lock().await.id.is_some() {
            return Err(ClientError::Pair(
                "GetQRChannel can only be called when there's no user ID in the client's store".into(),
            ));
        }
        let (tx, rx) = mpsc::channel::<QRChannelItem>(32);
        *self.qr_tx.lock().await = Some(tx);
        let this = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(QR_CHANNEL_TIMEOUT * (QR_ROTATION_LIMIT + 1)).await;
            if let Some(this) = this.upgrade()
                && let Some(tx) = this.qr_tx.lock().await.take()
                && !this.paired.load(std::sync::atomic::Ordering::SeqCst)
            {
                let _ = tx.try_send(QRChannelItem::Timeout);
            }
        });
        Ok(rx)
    }
}

/// Go `qrchan.go` — `QRChannelItem`.
#[derive(Debug, Clone)]
pub enum QRChannelItem {
    /// A QR code (`makeQRData` of one `pair-device` `ref`).
    Code(String),
    /// The server rotated the code (`QRChannelUnknownEvent`).
    Unknown,
    /// Emitted when the phone scanned with multidevice disabled.
    ScannedWithoutMultidevice,
    /// Emitted after `pair-success`.
    Success,
    /// No pairing happened before the channel's lifetime ran out
    /// (Go `QRChannelTimeout`).
    Timeout,
}
