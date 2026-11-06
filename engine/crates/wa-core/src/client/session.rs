// Ported from the Go client `request.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Request plumbing: `generateRequestID`, the `responseWaiters` map and the
//! `sendIQ*` family (port of `Go request.go`).
//!
//! The pieces here are deliberately free of the socket: the owning [`Client`]
//! (see `super`) supplies the waiters map, the send path and the reconnect
//! wait. Every function documents the Go function it ports.

use std::time::Duration;

use tokio::sync::oneshot;

use crate::binary::node::{AttrValue, Node};
use crate::types::jid::Jid;

use super::{Client, ClientError};

/// Go's `xmlStreamEndNode` — the sentinel waiter payload on disconnect.
pub fn xml_stream_end_node() -> Node {
    Node::new("xmlstreamend")
}

/// Go `isDisconnectNode` — a waiter payload that means "the socket went away".
pub fn is_disconnect_node(node: &Node) -> bool {
    node.tag == "xmlstreamend" || node.tag == "stream:error"
}

/// Go `isAuthErrorDisconnect` — a disconnect node that must never be retried
/// (`401`, `conflict type=replaced`, `conflict type=device_removed`).
pub fn is_auth_error_disconnect(node: &Node) -> bool {
    if node.tag != "stream:error" {
        return false;
    }
    if node.attrs.get("code").and_then(AttrValue::as_str) == Some("401") {
        return true;
    }
    matches!(
        node.optional_child_by_tag(&["conflict"])
            .and_then(|conflict| conflict.attrs.get("type"))
            .and_then(AttrValue::as_str),
        Some("replaced" | "device_removed")
    )
}

/// Go `infoQueryType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IqKind {
    #[default]
    Get,
    Set,
}

impl IqKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "get",
            Self::Set => "set",
        }
    }
}

/// Go `infoQuery` — one `iq` round-trip.
#[derive(Debug, Clone, Default)]
pub struct InfoQuery {
    pub namespace: String,
    pub kind: IqKind,
    pub to: Option<Jid>,
    pub target: Option<Jid>,
    pub id: Option<String>,
    pub smax_id: Option<String>,
    pub content: Vec<Node>,
    /// Per-query timeout; [`ClientConfig::request_timeout`] when `None`.
    pub timeout: Option<Duration>,
    /// Go `NoRetry` — return [`ClientError::Disconnected`] instead of
    /// re-sending after a reconnect.
    pub no_retry: bool,
}

#[allow(dead_code)]
impl InfoQuery {
    /// The `iq` node this query sends (Go `sendIQAsyncAndGetData`).
    pub fn to_node(&self, id: &str) -> Node {
        let mut node = Node::new("iq")
            .attr("id", id.to_owned())
            .attr("xmlns", self.namespace.clone())
            .attr("type", self.kind.as_str().to_owned());
        if let Some(smax_id) = &self.smax_id {
            node = node.attr("smax_id", smax_id.clone());
        }
        if let Some(to) = &self.to {
            node = node.attr("to", to.clone());
        }
        if let Some(target) = &self.target {
            node = node.attr("target", target.clone());
        }
        node.with_content(self.content.clone())
    }
}

impl Client {
    /// Go `generateRequestID` — `uniqueID + idCounter.Add(1)`.
    pub fn generate_request_id(&self) -> String {
        format!("{}{}", self.unique_id, self.id_counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1)
    }

    /// Go `waitResponse` — register a waiter for `id`.
    pub async fn register_waiter(&self, id: &str) -> oneshot::Receiver<Node> {
        let (tx, rx) = oneshot::channel();
        self.response_waiters.lock().await.insert(id.to_owned(), tx);
        rx
    }

    /// Go `cancelResponse` — drop the waiter for `id` without resolving it.
    pub async fn cancel_waiter(&self, id: &str) {
        self.response_waiters.lock().await.remove(id);
    }

    /// Go `receiveResponse` — resolve the waiter matching an `iq`/`ack` `id`.
    /// Returns `true` when the node was consumed by a waiter.
    pub async fn receive_response(&self, node: Node) -> bool {
        let Node { tag, attrs, .. } = &node;
        if tag != "iq" && tag != "ack" {
            return false;
        }
        let Some(id) = attrs.get("id").and_then(AttrValue::as_str) else {
            return false;
        };
        let sender = self.response_waiters.lock().await.remove(id);
        match sender {
            Some(sender) => {
                // The receiver may have gone away (timeout); that is fine.
                let _ = sender.send(node);
                true
            }
            None => false,
        }
    }

    /// Go `clearResponseWaiters` — resolve every waiter with the disconnect
    /// node (or drop them when `node` is `None`), then empty the map.
    pub async fn clear_response_waiters(&self, node: Option<Node>) {
        let waiters = std::mem::take(&mut *self.response_waiters.lock().await);
        for (_, sender) in waiters {
            match &node {
                Some(node) => {
                    let _ = sender.send(node.clone());
                }
                None => {
                    // Dropping the sender resolves the receiver with `Err`.
                }
            }
        }
    }

    /// Go `sendNodeAndGetData` — marshal the node and push it onto the socket,
    /// returning the marshalled bytes (needed by `retry_frame`).
    pub async fn send_node_and_get_data(&self, node: &Node) -> Result<Vec<u8>, ClientError> {
        let socket = self.socket.lock().await.clone().ok_or(ClientError::NotConnected)?;
        let payload = crate::binary::marshal(node);
        let xml = node.to_xml_string();
        tracing::debug!("send {}", xml);
        crate::request_log::log_outgoing(&xml);
        socket.send_frame(&payload).await.map_err(ClientError::Noise)?;
        Ok(payload)
    }

    /// Go `sendNode`.
    pub async fn send_node(&self, node: &Node) -> Result<(), ClientError> {
        self.send_node_and_get_data(node).await.map(|_| ())
    }

    /// Go `sendIQAsyncAndGetData` — register the waiter, send the node, return
    /// the receiver plus the marshalled bytes.
    pub async fn send_iq_async_and_get_data(
        &self,
        query: &InfoQuery,
    ) -> Result<(String, oneshot::Receiver<Node>, Vec<u8>), ClientError> {
        let id = query.id.clone().unwrap_or_else(|| self.generate_request_id());
        let waiter = self.register_waiter(&id).await;
        let node = query.to_node(&id);
        match self.send_node_and_get_data(&node).await {
            Ok(data) => Ok((id, waiter, data)),
            Err(err) => {
                self.cancel_waiter(&id).await;
                Err(err)
            }
        }
    }

    /// Go `sendIQAsync` — the receiver half of an `iq` round-trip.
    pub async fn send_iq_async(&self, query: InfoQuery) -> Result<oneshot::Receiver<Node>, ClientError> {
        self.send_iq_async_and_get_data(&query).await.map(|(_, rx, _)| rx)
    }

    /// Go `sendIQ` — send an `iq` and wait for the `result`/`error` response.
    pub async fn send_iq(&self, query: InfoQuery) -> Result<Node, ClientError> {
        let timeout = query.timeout.unwrap_or(self.config.request_timeout);
        let no_retry = query.no_retry;
        let (id, waiter, data) = self.send_iq_async_and_get_data(&query).await?;
        let response = tokio::time::timeout(timeout, waiter)
            .await
            .map_err(|_| ClientError::RequestTimeout { what: "info query", id: id.clone() })?;
        let response = response.map_err(|_| ClientError::Disconnected { what: "info query", id: id.clone() })?;
        if is_disconnect_node(&response) {
            if no_retry {
                return Err(ClientError::Disconnected { what: "info query", id });
            }
            // `retry_frame` returns the raw node; the `iq` shape check belongs
            // to the caller (Go: `retryFrame` + the `res.Tag != "iq"` check in
            // `sendIQ`).
            let response = self.retry_frame("info query", &id, &data, &response, timeout).await?;
            return Self::check_iq_response(&id, response);
        }
        Self::check_iq_response(&id, response)
    }

    /// Validate an `iq` response: `type=result` passes through, `type=error`
    /// becomes [`ClientError::Iq`], anything else too.
    fn check_iq_response(id: &str, response: Node) -> Result<Node, ClientError> {
        let response_type = response.attrs.get("type").and_then(AttrValue::as_str).unwrap_or_default();
        if response.tag != "iq" || (response_type != "result" && response_type != "error") {
            return Err(ClientError::Iq { id: id.to_owned(), node: response.to_xml_string() });
        }
        if response_type == "error" {
            return Err(ClientError::Iq { id: id.to_owned(), node: response.to_xml_string() });
        }
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_end_and_disconnect_nodes() {
        assert_eq!(xml_stream_end_node().tag, "xmlstreamend");
        assert!(is_disconnect_node(&xml_stream_end_node()));
        assert!(is_disconnect_node(&Node::new("stream:error")));
        assert!(!is_disconnect_node(&Node::new("iq")));
        assert!(!is_disconnect_node(&Node::new("ack")));
    }

    #[test]
    fn auth_errors_are_not_retried() {
        let err401 = Node::new("stream:error").attr("code", "401");
        assert!(is_auth_error_disconnect(&err401));
        let replaced = Node::new("stream:error").with_content(vec![Node::new("conflict").attr("type", "replaced")]);
        assert!(is_auth_error_disconnect(&replaced));
        let removed =
            Node::new("stream:error").with_content(vec![Node::new("conflict").attr("type", "device_removed")]);
        assert!(is_auth_error_disconnect(&removed));
        let other = Node::new("stream:error").attr("code", "515");
        assert!(!is_auth_error_disconnect(&other));
        assert!(!is_auth_error_disconnect(&Node::new("iq")));
    }

    #[test]
    fn iq_node_carries_the_query_fields() {
        let query = InfoQuery {
            namespace: "w:p".into(),
            kind: IqKind::Get,
            to: Some(Jid::new("", crate::types::jid::DEFAULT_USER_SERVER)),
            id: None,
            content: vec![Node::new("ping")],
            ..Default::default()
        };
        let node = query.to_node("abc.1-5");
        assert_eq!(node.tag, "iq");
        assert_eq!(node.attrs.get("id").and_then(AttrValue::as_str), Some("abc.1-5"));
        assert_eq!(node.attrs.get("xmlns").and_then(AttrValue::as_str), Some("w:p"));
        assert_eq!(node.attrs.get("type").and_then(AttrValue::as_str), Some("get"));
        assert_eq!(node.children().len(), 1);
    }
}
