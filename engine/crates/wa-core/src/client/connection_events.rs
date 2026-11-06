// Ported from the Go client `connectionevents.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Connection-event handlers: `handleConnectSuccess`, `handleConnectFailure`,
//! `handleStreamError`, `handleIB` and `SetPassive`
//! (port of `Go connectionevents.go`).

use std::time::Duration;

use crate::binary::node::{AttrValue, Node};
use crate::types::jid::Jid;

use super::session::{InfoQuery, IqKind};
use super::{Client, ClientError, server_jid};

/// Go `events.ConnectFailureReason` — one constant per reason code.
pub mod reason {
    pub const GENERIC: i32 = 400;
    pub const LOGGED_OUT: i32 = 401;
    pub const TEMP_BANNED: i32 = 402;
    pub const MAIN_DEVICE_GONE: i32 = 403;
    pub const CLIENT_OUTDATED: i32 = 405;
    pub const UNKNOWN_LOGOUT: i32 = 406;
    pub const BAD_USER_AGENT: i32 = 409;
    pub const CAT_EXPIRED: i32 = 413;
    pub const CAT_INVALID: i32 = 414;
    pub const NOT_FOUND: i32 = 415;
    pub const CLIENT_UNKNOWN: i32 = 418;
    pub const INTERNAL_SERVER_ERROR: i32 = 500;
    pub const EXPERIMENTAL: i32 = 501;
    pub const SERVICE_UNAVAILABLE: i32 = 503;

    /// Go `ConnectFailureReason.IsLoggedOut` — `401 | 403 | 406`.
    pub fn is_logged_out(reason: i32) -> bool {
        matches!(reason, LOGGED_OUT | MAIN_DEVICE_GONE | UNKNOWN_LOGOUT)
    }

    /// Reasons the auto-reconnect loop keeps retrying
    /// (`500 | 501 | 503` in Go's `autoReconnect`).
    pub fn is_retryable(reason: i32) -> bool {
        matches!(reason, INTERNAL_SERVER_ERROR | EXPERIMENTAL | SERVICE_UNAVAILABLE)
    }
}

impl Client {
    /// Go `handleConnectSuccess` — mark the session logged in, adopt the LID
    /// when the store has none, then `set_passive(false)` + `Connected`.
    ///
    /// The pre-key top-up ([`Client::ensure_pre_keys_uploaded`]), the
    /// passive IQ and `Connected` run in a spawned task like Go's goroutine.
    /// `StoreLIDPNMapping` needs the LID map + contact stores and is
    /// `TODO(Phase 4)`.
    pub async fn handle_connect_success(self: &std::sync::Arc<Self>, node: &Node) {
        if !self.paired.load(std::sync::atomic::Ordering::SeqCst) {
            tracing::warn!("received connect success without pairing, ignoring");
            return;
        }
        tracing::info!("successfully authenticated");
        self.reconnect_errors.store(0, std::sync::atomic::Ordering::SeqCst);
        self.is_logged_in.store(true, std::sync::atomic::Ordering::SeqCst);

        let node_lid = node
            .attrs
            .get("lid")
            .and_then(AttrValue::as_str)
            .and_then(|lid| lid.parse::<Jid>().ok())
            .unwrap_or_default();
        if let Ok(timestamp) = node.attrs.get("t").and_then(AttrValue::as_str).unwrap_or_default().parse::<i64>() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or_default();
            self.server_time_offset.store(timestamp - now, std::sync::atomic::Ordering::SeqCst);
        }

        {
            let mut device = self.device.lock().await;
            if !device.lid.is_empty() && !node_lid.is_empty() && device.lid != node_lid {
                tracing::warn!("stored LID doesn't match one in connect success, clearing");
                device.lid = Jid::default();
            }
            if device.lid.is_empty() && !node_lid.is_empty() {
                device.lid = node_lid.clone();
                match device.save().await {
                    Ok(()) => tracing::info!("updated LID"),
                    Err(err) => tracing::warn!("failed to save device after updating LID: {err}"),
                }
            }
        }
        // Go `connectionevents.go:184` — our own two identities are a mapping
        // like any other, and the one both the peer-message path and our own
        // device fan-out need. Read from the device, not from the `<success>`
        // node: a reconnect does not always carry the LID, and a paired device
        // already knows both halves.
        let (own_id, own_lid) = {
            let device = self.device.lock().await;
            (device.id.clone().unwrap_or_default(), device.lid.clone())
        };
        self.store_lid_pn_mapping(&own_lid, &own_id).await;

        // Go runs the pre-key top-up, `SetPassive(false)` and `Connected` in a
        // goroutine: the IQ round-trips must not block the node handler queue.
        let client = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            client.ensure_pre_keys_uploaded().await;
            if let Err(err) = client.set_passive(false).await {
                tracing::warn!("failed to send post-connect passive iq: {err}");
            }
            client.dispatch_event(crate::WaEvent::Connected);
        });
    }

    /// Go `handleConnectFailure` — map the `reason` to `ConnectFailure`,
    /// `LoggedOut`, `TemporaryBan` or `ClientOutdated`.
    pub async fn handle_connect_failure(&self, node: &Node) {
        let reason: i32 =
            node.attrs.get("reason").and_then(AttrValue::as_str).and_then(|r| r.parse().ok()).unwrap_or_default();
        let message = node.attrs.get("message").and_then(AttrValue::as_str).unwrap_or_default().to_owned();
        // CAT (`413`/`414`) has no Rust equivalent yet → TODO(Phase 6), treat as non-retryable.
        let will_auto_reconnect = reason::is_retryable(reason);
        if !will_auto_reconnect {
            self.expect_disconnect().await;
        }
        if reason == reason::MAIN_DEVICE_GONE {
            tracing::debug!("message for 403 connect failure");
        }
        if reason::is_logged_out(reason) {
            tracing::info!("got {reason} connect failure, sending LoggedOut and deleting the session");
            self.dispatch_event(crate::WaEvent::LoggedOut { on_connect: true, reason });
            if let Err(err) = self.device.lock().await.delete().await {
                tracing::warn!("failed to delete store after {reason} failure: {err}");
            }
        } else if reason == reason::TEMP_BANNED {
            tracing::warn!("temporary ban connect failure: {}", node.to_xml_string());
            let code: i32 =
                node.attrs.get("code").and_then(AttrValue::as_str).and_then(|c| c.parse().ok()).unwrap_or_default();
            let expire: i64 =
                node.attrs.get("expire").and_then(AttrValue::as_str).and_then(|e| e.parse().ok()).unwrap_or_default();
            self.dispatch_event(crate::WaEvent::TemporaryBan { code, expire_ms: expire * 1000 });
        } else if reason == reason::CLIENT_OUTDATED {
            tracing::error!("client outdated (405) connect failure");
            self.dispatch_event(crate::WaEvent::ClientOutdated);
        } else if will_auto_reconnect {
            tracing::warn!("got {reason}/{message} connect failure, assuming automatic reconnect will handle it");
        } else {
            tracing::warn!("unknown connect failure: {}", node.to_xml_string());
            self.dispatch_event(crate::WaEvent::ConnectFailure { reason, message });
        }
    }

    /// Go `handleStreamError` — `515` reconnects silently, `401/device_removed`
    /// logs out and deletes the device, `replaced` emits `StreamReplaced`,
    /// `503` is left to auto-reconnect, everything else is `StreamError`.
    ///
    /// Boxed like [`Client::auto_reconnect`]: this is reachable from
    /// `connect` (via the handler queue) and spawns `connect`, so an
    /// `async fn` would form an opaque-future type cycle.
    pub fn handle_stream_error(
        self: &std::sync::Arc<Self>,
        node: &Node,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        let this = std::sync::Arc::clone(self);
        let node = node.clone();
        Box::pin(async move { this.handle_stream_error_inner(&node).await })
    }

    async fn handle_stream_error_inner(self: &std::sync::Arc<Self>, node: &Node) {
        self.is_logged_in.store(false, std::sync::atomic::Ordering::SeqCst);
        self.clear_response_waiters(Some(node.clone())).await;
        let code = node.attrs.get("code").and_then(AttrValue::as_str).unwrap_or_default().to_owned();
        let conflict_type = node
            .optional_child_by_tag(&["conflict"])
            .and_then(|conflict| conflict.attrs.get("type"))
            .and_then(AttrValue::as_str)
            .unwrap_or_default()
            .to_owned();
        if code == "515" {
            tracing::info!("got 515 code, reconnecting");
            let this = std::sync::Arc::clone(self);
            tokio::spawn(async move {
                this.disconnect().await;
                if let Err(err) = this.connect().await {
                    tracing::error!("failed to reconnect after 515 code: {err}");
                }
            });
        } else if code == "401" && conflict_type == "device_removed" {
            self.expect_disconnect().await;
            tracing::info!("got device removed stream error, sending LoggedOut and deleting the session");
            self.dispatch_event(crate::WaEvent::LoggedOut { on_connect: false, reason: reason::LOGGED_OUT });
            if let Err(err) = self.device.lock().await.delete().await {
                tracing::warn!("failed to delete store after device_removed error: {err}");
            }
        } else if conflict_type == "replaced" {
            self.expect_disconnect().await;
            tracing::info!("got replaced stream error, sending StreamReplaced");
            self.dispatch_event(crate::WaEvent::StreamReplaced);
        } else if code == "503" {
            tracing::warn!("got 503 stream error, assuming automatic reconnect will handle it");
        } else {
            tracing::error!("unknown stream error: {}", node.to_xml_string());
            self.dispatch_event(crate::WaEvent::StreamError { code });
        }
    }

    /// Go `handleIB` — `downgrade_webclient` emits
    /// `QRScannedWithoutMultidevice`; `offline_preview`/`offline` are
    /// `TODO(Phase 4)` (history sync).
    pub async fn handle_ib(&self, node: &Node) {
        for child in node.children() {
            match child.tag.as_str() {
                "downgrade_webclient" => self.dispatch_event(crate::WaEvent::QrScannedWithoutMultidevice),
                // TODO(Phase 4): OfflineSyncPreview / OfflineSyncCompleted (history sync).
                "offline_preview" | "offline" | "dirty" => {
                    tracing::debug!("ignoring ib child {} until history sync lands", child.tag);
                }
                _ => tracing::debug!("ignoring unknown ib child {}", child.tag),
            }
        }
    }

    /// Go `SetPassive` — `iq xmlns=passive` with an `active`/`passive` child.
    pub async fn set_passive(&self, passive: bool) -> Result<(), ClientError> {
        let tag = if passive { "passive" } else { "active" };
        self.send_iq(InfoQuery {
            namespace: "passive".into(),
            kind: IqKind::Set,
            to: Some(server_jid()),
            content: vec![Node::new(tag)],
            timeout: Some(Duration::from_secs(10)),
            ..Default::default()
        })
        .await
        .map(|_| ())
    }
}
