// Ported from the Go client `binary/node.go` and `binary/xml.go`, © Tulir Asokan,
// MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The `Node` type — an element of WhatsApp's binary XML document.

use std::collections::BTreeMap;
use std::fmt;

use crate::types::jid::Jid;

use super::decoder::Decoder;
use super::encoder::Encoder;
use super::errors::BinaryError;

/// `binary.Attrs` — the attributes of an element.
///
/// A `BTreeMap` keeps the encoded attribute order deterministic (Go's map
/// iteration order is random, which is why Go dumps aren't byte-stable).
pub type Attrs = BTreeMap<String, AttrValue>;

/// A single attribute value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttrValue {
    /// A plain string (`string` in Go).
    Text(String),
    /// A parsed JID, written with the JID tokens.
    Jid(Jid),
    /// A signed integer, written as its decimal string.
    Int(i64),
    /// An unsigned integer, written as its decimal string.
    Uint(u64),
    /// A boolean, written as `true`/`false`.
    Bool(bool),
}

impl AttrValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            AttrValue::Text(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_jid(&self) -> Option<&Jid> {
        match self {
            AttrValue::Jid(jid) => Some(jid),
            _ => None,
        }
    }
}

impl From<String> for AttrValue {
    fn from(value: String) -> Self {
        AttrValue::Text(value)
    }
}

impl From<&str> for AttrValue {
    fn from(value: &str) -> Self {
        AttrValue::Text(value.to_owned())
    }
}

impl From<Jid> for AttrValue {
    fn from(value: Jid) -> Self {
        AttrValue::Jid(value)
    }
}

impl From<i64> for AttrValue {
    fn from(value: i64) -> Self {
        AttrValue::Int(value)
    }
}

impl From<u64> for AttrValue {
    fn from(value: u64) -> Self {
        AttrValue::Uint(value)
    }
}

impl From<bool> for AttrValue {
    fn from(value: bool) -> Self {
        AttrValue::Bool(value)
    }
}

impl fmt::Display for AttrValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttrValue::Text(text) => f.write_str(text),
            AttrValue::Jid(jid) => write!(f, "{jid}"),
            AttrValue::Int(value) => write!(f, "{value}"),
            AttrValue::Uint(value) => write!(f, "{value}"),
            AttrValue::Bool(value) => write!(f, "{value}"),
        }
    }
}

/// `Node.Content` — what's inside an element.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum NodeContent {
    /// No content at all (Go's `nil`).
    #[default]
    Empty,
    /// A list of child elements.
    Nodes(Vec<Node>),
    /// Raw bytes (e.g. a protobuf payload).
    Bytes(Vec<u8>),
    /// A string (Go's `string` content).
    Text(String),
    /// A JID (written with the JID tokens, like Go's `types.JID` content).
    Jid(Jid),
}

impl NodeContent {
    pub fn is_empty(&self) -> bool {
        matches!(self, NodeContent::Empty)
    }
}

impl From<Vec<Node>> for NodeContent {
    fn from(value: Vec<Node>) -> Self {
        NodeContent::Nodes(value)
    }
}

impl From<Vec<u8>> for NodeContent {
    fn from(value: Vec<u8>) -> Self {
        NodeContent::Bytes(value)
    }
}

impl From<Jid> for NodeContent {
    fn from(value: Jid) -> Self {
        NodeContent::Jid(value)
    }
}

impl From<String> for NodeContent {
    fn from(value: String) -> Self {
        NodeContent::Text(value)
    }
}

impl From<&str> for NodeContent {
    fn from(value: &str) -> Self {
        NodeContent::Text(value.to_owned())
    }
}

/// How `Node::to_xml_string` prints things.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XmlOptions {
    /// Indent children with two spaces and newlines.
    pub indent: bool,
    /// Print byte content as hex only up to this many bytes, then a comment.
    pub max_bytes_as_hex: usize,
}

impl Default for XmlOptions {
    fn default() -> Self {
        Self { indent: false, max_bytes_as_hex: 128 }
    }
}

/// Attributes of these tags are credentials and never printed.
const REDACTED_ATTRS: &[(&str, &[&str])] = &[("media_conn", &["auth", "ip_token"])];

/// Content of these child tags is a credential and never printed.
const REDACTED_CHILDREN: &[(&str, &[&str])] = &[("relay", &["token", "auth_token", "key", "hbh_key"])];

const REDACTED: &str = "[redacted]";

/// `binary.Node` — an XML element of the WhatsApp protocol.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Node {
    /// The tag of the element.
    pub tag: String,
    /// The attributes of the element.
    pub attrs: Attrs,
    /// The content inside the element.
    pub content: NodeContent,
}

impl Node {
    pub fn new(tag: impl Into<String>) -> Self {
        Self { tag: tag.into(), ..Default::default() }
    }

    pub fn with_attrs(mut self, attrs: Attrs) -> Self {
        self.attrs = attrs;
        self
    }

    pub fn with_content(mut self, content: impl Into<NodeContent>) -> Self {
        self.content = content.into();
        self
    }

    /// Adds a single attribute (builder style).
    pub fn attr(mut self, key: impl Into<String>, value: impl Into<AttrValue>) -> Self {
        self.attrs.insert(key.into(), value.into());
        self
    }

    /// `GetChildren` — the content as children, or an empty slice.
    pub fn children(&self) -> &[Node] {
        match &self.content {
            NodeContent::Nodes(nodes) => nodes,
            _ => &[],
        }
    }

    /// `GetChildrenByTag` — the children with a matching tag.
    pub fn children_by_tag(&self, tag: &str) -> impl Iterator<Item = &Node> {
        self.children().iter().filter(move |node| node.tag == tag)
    }

    /// `GetOptionalChildByTag` — walks the given tags down the tree.
    pub fn optional_child_by_tag(&self, tags: &[&str]) -> Option<&Node> {
        let mut current = self;
        for tag in tags {
            current = current.children().iter().find(|child| child.tag == *tag)?;
        }
        Some(current)
    }

    /// `GetChildByTag` — like [`Self::optional_child_by_tag`], but returns this
    /// node itself when the path doesn't exist (like Go).
    pub fn child_by_tag(&self, tags: &[&str]) -> Node {
        self.optional_child_by_tag(tags).cloned().unwrap_or_else(|| self.clone())
    }

    /// The content as text/bytes if it is one of those.
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match &self.content {
            NodeContent::Bytes(bytes) => Some(bytes),
            NodeContent::Text(text) => Some(text.as_bytes()),
            _ => None,
        }
    }

    /// The content as a string if it's textual.
    pub fn as_str(&self) -> Option<&str> {
        match &self.content {
            NodeContent::Text(text) => Some(text),
            _ => None,
        }
    }

    /// `Node.String` — the XML representation, with credentials redacted.
    pub fn to_xml_string(&self) -> String {
        self.xml_with(XmlOptions::default())
    }

    /// `Node.String` with explicit options.
    pub fn xml_with(&self, opts: XmlOptions) -> String {
        let content = self.content_string(opts);
        let attrs = self.attribute_string();
        if content.is_empty() {
            return format!("<{}{}/>", self.tag, attrs);
        }
        let newline = if content.len() == 1 || !opts.indent { "" } else { "\n" };
        format!("<{}{}>{newline}{}{newline}</{}>", self.tag, attrs, content.join(newline), self.tag)
    }

    fn attribute_string(&self) -> String {
        if self.attrs.is_empty() {
            return String::new();
        }
        let redacted = REDACTED_ATTRS.iter().find(|(tag, _)| *tag == self.tag).map(|(_, keys)| *keys);
        let mut parts: Vec<String> = self
            .attrs
            .iter()
            .map(|(key, value)| {
                if redacted.is_some_and(|keys| keys.contains(&key.as_str())) {
                    format!(" {key}=\"{REDACTED}\"")
                } else {
                    format!(" {key}=\"{value}\"")
                }
            })
            .collect();
        parts.sort();
        parts.concat()
    }

    fn content_string(&self, opts: XmlOptions) -> Vec<String> {
        let mut split: Vec<String> = Vec::new();
        match &self.content {
            NodeContent::Nodes(nodes) => {
                let redacted = REDACTED_CHILDREN.iter().find(|(tag, _)| *tag == self.tag).map(|(_, tags)| *tags);
                for node in nodes {
                    let mut node = node.clone();
                    if redacted.is_some_and(|tags| tags.contains(&node.tag.as_str())) {
                        node.content = NodeContent::Text(REDACTED.to_owned());
                    }
                    split.extend(node.xml_with(opts).split('\n').map(str::to_owned));
                }
            }
            NodeContent::Bytes(bytes) => bytes_content(bytes, opts, &mut split),
            NodeContent::Text(text) => push_text(text, opts, &mut split),
            NodeContent::Jid(jid) => push_text(&jid.to_string(), opts, &mut split),
            NodeContent::Empty => {}
        }
        if split.len() > 1 && opts.indent {
            for line in &mut split {
                line.insert_str(0, "  ");
            }
        }
        split
    }
}

/// Port of `printable` + the byte branch of `contentString`.
fn bytes_content(bytes: &[u8], opts: XmlOptions, split: &mut Vec<String>) {
    match std::str::from_utf8(bytes) {
        // `printable` — valid UTF-8 without control characters.
        Ok(text) if !text.is_empty() && text.chars().all(|c| !c.is_control()) => push_text(text, opts, split),
        _ if bytes.len() > opts.max_bytes_as_hex => {
            split.push(format!("<!-- {} bytes -->", bytes.len()));
        }
        _ => {
            let hex = hex(bytes);
            if opts.indent {
                for chunk in hex.as_bytes().chunks(80) {
                    split.push(String::from_utf8_lossy(chunk).into_owned());
                }
            } else {
                split.push(hex);
            }
        }
    }
}

/// Port of the `default`/string branch of `contentString`.
fn push_text(text: &str, opts: XmlOptions, split: &mut Vec<String>) {
    if opts.indent {
        split.extend(text.split('\n').map(str::to_owned));
    } else {
        split.push(text.replace('\n', "\\n"));
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `binary.Marshal` — encodes a node into a complete, uncompressed frame.
///
/// Like Go, the result starts with the frame data-type byte (`0`, i.e. "not
/// zlib-compressed") and is ready to be sent as-is. To read a node back, pass
/// the frame through [`super::unpack`] first — `unmarshal` expects the payload
/// without that byte, exactly like Go's `Unpack` + `Unmarshal` pair.
pub fn marshal(node: &Node) -> Vec<u8> {
    let mut encoder = Encoder::new();
    encoder.write_node(node);
    encoder.into_data()
}

/// `binary.Unmarshal` — decodes a binary XML payload into a node.
///
/// `data` is the payload *after* [`super::unpack`] (i.e. without the frame data
/// type byte) — the same contract as Go's `binary.Unmarshal`.
pub fn unmarshal(data: &[u8]) -> Result<Node, BinaryError> {
    let mut decoder = Decoder::new(data);
    let node = decoder.read_node()?;
    if decoder.position() != data.len() {
        return Err(BinaryError::LeftoverBytes(data.len() - decoder.position()));
    }
    Ok(node)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Node {
        Node::new("message")
            .attr("to", "48123456789@s.whatsapp.net")
            .attr("id", "3EB0C767D097")
            .with_content(vec![Node::new("media").attr("type", "image"), Node::new("empty")])
    }

    #[test]
    fn children_helpers() {
        let node = sample();
        assert_eq!(node.children().len(), 2);
        assert_eq!(node.children_by_tag("media").count(), 1);
        assert_eq!(node.children_by_tag("nope").count(), 0);
        assert_eq!(node.child_by_tag(&["media"]).attrs["type"].as_str(), Some("image"));
        // Like Go, a missing path returns the node itself.
        assert_eq!(node.child_by_tag(&["nope"]), node);
        assert_eq!(node.optional_child_by_tag(&["nope"]), None);
    }

    #[test]
    fn nested_lookup_walks_every_tag() {
        let inner = Node::new("tctoken").with_content("abc");
        let node = Node::new("iq").with_content(vec![Node::new("tokens").with_content(vec![inner])]);
        assert_eq!(node.child_by_tag(&["tokens", "tctoken"]).as_str(), Some("abc"));
        assert_eq!(node.optional_child_by_tag(&["tokens", "missing"]), None);
    }

    #[test]
    fn content_accessors() {
        assert_eq!(Node::new("x").with_content("hi").as_str(), Some("hi"));
        assert_eq!(Node::new("x").with_content(vec![1u8, 2]).as_bytes(), Some(&[1, 2][..]));
        assert_eq!(Node::new("x").with_content("hi").as_bytes(), Some(&b"hi"[..]));
        assert_eq!(Node::new("x").as_bytes(), None);
        assert!(Node::new("x").content.is_empty());
    }

    #[test]
    fn xml_renders_and_redacts() {
        assert_eq!(Node::new("a").to_xml_string(), "<a/>");
        let xml = sample().to_xml_string();
        assert!(xml.contains(r#"<message id="3EB0C767D097" to="48123456789@s.whatsapp.net">"#), "{xml}");

        let media_conn =
            Node::new("media_conn").attr("auth", "secret").attr("ttl", "60").attr("ip_token", "also-secret");
        let xml = media_conn.to_xml_string();
        assert!(xml.contains(r#"auth="[redacted]""#), "{xml}");
        assert!(xml.contains(r#"ip_token="[redacted]""#), "{xml}");
        assert!(xml.contains(r#"ttl="60""#), "{xml}");
        assert!(!xml.contains("secret"), "{xml}");

        let relay = Node::new("relay").with_content(vec![
            Node::new("token").with_content("very-secret"),
            Node::new("hostname").with_content("mmg.whatsapp.net"),
        ]);
        let xml = relay.to_xml_string();
        assert!(!xml.contains("very-secret"), "{xml}");
        assert!(xml.contains("mmg.whatsapp.net"), "{xml}");
    }

    #[test]
    fn xml_prints_bytes_and_truncates() {
        let small = Node::new("x").with_content(vec![0xffu8, 0x00]);
        assert_eq!(small.to_xml_string(), "<x>ff00</x>");

        let large = Node::new("x").with_content(vec![0u8; 200]);
        assert_eq!(large.to_xml_string(), "<x><!-- 200 bytes --></x>");

        let text = Node::new("x").with_content("a\nb");
        assert_eq!(text.to_xml_string(), r"<x>a\nb</x>");
    }
}
