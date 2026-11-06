// Ported from the Go client (`mediaconn.go`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The media connection — which hosts attachments are fetched from.
//!
//! Media never travels over the WebSocket: WhatsApp answers an
//! `<iq xmlns="w:m"><media_conn/></iq>` with a list of CDN hostnames plus an
//! auth token and a TTL, and the actual bytes are an ordinary HTTPS GET
//! against one of those hosts (see [`super::download`]).
//!
//! The answer is cached until its TTL expires, because every download would
//! otherwise cost an extra round trip.

use std::time::{Duration, Instant};

use crate::binary::node::Node;
use crate::client::ack::attr_text;
use crate::client::message::attr_int;
use crate::client::session::{InfoQuery, IqKind};
use crate::client::{Client, ClientError, server_jid};

/// One CDN host attachments can be downloaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaConnHost {
    pub hostname: String,
}

/// The cached answer of `<iq xmlns="w:m"><media_conn/></iq>`.
#[derive(Debug, Clone)]
pub struct MediaConn {
    /// Auth token for the media hosts (not part of the URL for direct paths,
    /// but the server still sends it).
    pub auth: String,
    pub auth_ttl: i64,
    /// How long this answer stays valid, in seconds.
    pub ttl: i64,
    pub max_buckets: i64,
    /// When this engine received the answer.
    pub fetched_at: Instant,
    /// The hosts to try, in server order.
    pub hosts: Vec<MediaConnHost>,
}

impl MediaConn {
    /// `true` once the TTL has passed and the answer must be re-queried.
    pub fn expired(&self) -> bool {
        let ttl = self.ttl.max(0) as u64;
        self.fetched_at.elapsed() >= Duration::from_secs(ttl)
    }

    /// The hostnames, in the order they should be tried.
    pub fn hostnames(&self) -> impl Iterator<Item = &str> {
        self.hosts.iter().map(|h| h.hostname.as_str())
    }
}

impl Client {
    /// The media connection, from cache when it is still valid.
    ///
    /// `force` re-queries even a fresh answer (used after a `403`, which means
    /// the auth token went stale early).
    pub async fn refresh_media_conn(&self, force: bool) -> Result<MediaConn, ClientError> {
        {
            let cached = self.media_conn_cache.lock().await;
            if let Some(conn) = cached.as_ref()
                && !force
                && !conn.expired()
            {
                return Ok(conn.clone());
            }
        }
        let conn = self.query_media_conn().await?;
        let mut cached = self.media_conn_cache.lock().await;
        *cached = Some(conn.clone());
        Ok(conn)
    }

    /// `<iq to="s.whatsapp.net" type="set" xmlns="w:m"><media_conn/></iq>`.
    async fn query_media_conn(&self) -> Result<MediaConn, ClientError> {
        let response = self
            .send_iq(InfoQuery {
                namespace: "w:m".to_owned(),
                kind: IqKind::Set,
                to: Some(server_jid()),
                content: vec![Node::new("media_conn")],
                ..Default::default()
            })
            .await?;
        parse_media_conn(&response)
    }
}

/// Read a `<media_conn>` answer. The attributes are required; unknown children
/// are ignored, so a new element in the answer cannot break a download.
pub fn parse_media_conn(response: &Node) -> Result<MediaConn, ClientError> {
    let node = response
        .optional_child_by_tag(&["media_conn"])
        .ok_or(ClientError::MissingElement { tag: "media_conn", parent: "iq" })?;
    let auth = attr_text(node, "auth").ok_or(ClientError::MissingElement { tag: "auth", parent: "media_conn" })?;
    let hosts: Vec<MediaConnHost> = node
        .children()
        .iter()
        .filter(|child| child.tag == "host")
        .filter_map(|child| attr_text(child, "hostname").map(|hostname| MediaConnHost { hostname }))
        .collect();
    if hosts.is_empty() {
        return Err(ClientError::MissingElement { tag: "host", parent: "media_conn" });
    }
    Ok(MediaConn {
        auth,
        auth_ttl: attr_int(node, "auth_ttl").unwrap_or_default(),
        ttl: attr_int(node, "ttl").unwrap_or(0),
        max_buckets: attr_int(node, "max_buckets").unwrap_or_default(),
        fetched_at: Instant::now(),
        hosts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer() -> Node {
        Node::new("iq").with_content(vec![
            Node::new("media_conn")
                .attr("auth", "AUTHTOKEN")
                .attr("ttl", "3600")
                .attr("auth_ttl", "21600")
                .attr("max_buckets", "12")
                .with_content(vec![
                    Node::new("host").attr("hostname", "mmg.whatsapp.net"),
                    Node::new("host").attr("hostname", "media-xyz.cdn.whatsapp.net"),
                    // Anything that is not a `<host>` must be ignored.
                    Node::new("something-new").attr("hostname", "nope.example"),
                ]),
        ])
    }

    #[test]
    fn parses_hosts_and_ttl() {
        let conn = parse_media_conn(&answer()).expect("a media conn");
        assert_eq!(conn.auth, "AUTHTOKEN");
        assert_eq!((conn.ttl, conn.auth_ttl, conn.max_buckets), (3600, 21600, 12));
        assert_eq!(conn.hostnames().collect::<Vec<_>>(), ["mmg.whatsapp.net", "media-xyz.cdn.whatsapp.net"]);
        assert!(!conn.expired(), "a fresh answer with a one-hour ttl is valid");
    }

    #[test]
    fn a_zero_ttl_answer_is_immediately_stale() {
        let node = Node::new("iq").with_content(vec![
            Node::new("media_conn")
                .attr("auth", "A")
                .attr("ttl", "0")
                .with_content(vec![Node::new("host").attr("hostname", "h")]),
        ]);
        assert!(parse_media_conn(&node).unwrap().expired());
    }

    #[test]
    fn an_answer_without_auth_or_hosts_is_rejected() {
        let no_auth = Node::new("iq").with_content(vec![
            Node::new("media_conn").attr("ttl", "60").with_content(vec![Node::new("host").attr("hostname", "h")]),
        ]);
        assert!(matches!(parse_media_conn(&no_auth), Err(ClientError::MissingElement { tag: "auth", .. })));

        let no_hosts = Node::new("iq").with_content(vec![Node::new("media_conn").attr("auth", "A").attr("ttl", "60")]);
        assert!(matches!(parse_media_conn(&no_hosts), Err(ClientError::MissingElement { tag: "host", .. })));

        let empty = Node::new("iq");
        assert!(matches!(parse_media_conn(&empty), Err(ClientError::MissingElement { tag: "media_conn", .. })));
    }
}
