// Ported from the Go client `binary/`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! WhatsApp's binary XML format (`Go binary`).
//!
//! [`Node`] is an element, [`marshal`] encodes it and [`unmarshal`] decodes it.
//! [`unpack`] strips the data type byte of a WebSocket frame and decompresses it
//! when needed.

pub mod attrs;
pub mod decoder;
pub mod encoder;
pub mod errors;
pub mod node;
pub mod token;
pub mod unpack;

#[cfg(test)]
pub mod testutil;

pub use attrs::{AttrError, AttrGetter};
pub use decoder::{Decoder, Value};
pub use encoder::Encoder;
pub use errors::BinaryError;
pub use node::{AttrValue, Attrs, Node, NodeContent, XmlOptions, marshal, unmarshal};
pub use unpack::unpack;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::jid::{DEFAULT_USER_SERVER, GROUP_SERVER, HIDDEN_USER_SERVER, Jid};

    /// Golden vectors produced by the Go implementation (`go run` against
    /// `Go binary`), so encoder and decoder can't drift apart silently.
    ///
    /// Only nodes with at most one attribute are listed: Go iterates attribute
    /// maps in random order, so its output is only byte-stable without them.
    const GOLDEN: &[(&str, &str, &str)] = &[
        // (name, hex, expected XML) — the XML keeps the table readable.
        ("empty_node", "00f801fc0161", "<a/>"),
        ("single_attribute", "00f8031908ff021234", r#"<iq id="1234"/>"#),
        ("digits_attribute", "00f8031908ff8648123456789f", r#"<iq id="48123456789"/>"#),
        ("hex_attribute", "00f8031908fb063eb0c767d097", r#"<iq id="3EB0C767D097"/>"#),
        ("double_byte_token_tag", "00f801ec00", "<read-self/>"),
        ("nibble_text_content", "00f802fc0178ff8648123456789f", "<x>48123456789</x>"),
        ("hex_text_content", "00f802fc0178fb063eb0c767d097", "<x>3EB0C767D097</x>"),
        ("empty_string_content", "00f802fc0178ff00", "<x></x>"),
        ("bytes_content", "00f802fc0178fc03010203", "<x>010203</x>"),
        ("jid_pair_content", "00f80211faff845511999f03", "<to>5511999@s.whatsapp.net</to>"),
        ("ad_jid_content", "00f80211f70003ff845511999f", "<to>5511999:3@s.whatsapp.net</to>"),
        ("lid_jid_content", "00f80211faff049988776676", "<to>99887766@lid</to>"),
        ("server_only_jid_content", "00f80211fa001c", "<to>g.us</to>"),
        ("children", "00f80213f802f80137f803fc01780443", r#"<message><media/><x type="image"/></message>"#),
    ];

    fn hex_of(data: &[u8]) -> String {
        hex::encode(data)
    }

    fn decode_hex(text: &str) -> Vec<u8> {
        hex::decode(text).expect("valid hex")
    }

    /// Decodes a complete frame (data type byte + payload) into a node.
    fn node_from_frame(frame: &[u8]) -> Node {
        unmarshal(&unpack(frame).expect("valid frame")).expect("valid payload")
    }

    #[test]
    fn golden_vectors_build_the_expected_nodes() {
        // The expected node for each vector, written out explicitly — this is
        // the part that would silently drift if only the hex were compared.
        let expected = [
            Node::new("a"),
            Node::new("iq").attr("id", "1234"),
            Node::new("iq").attr("id", "48123456789"),
            Node::new("iq").attr("id", "3EB0C767D097"),
            Node::new("read-self"),
            Node::new("x").with_content("48123456789"),
            Node::new("x").with_content("3EB0C767D097"),
            Node::new("x").with_content(""),
            Node::new("x").with_content(vec![1u8, 2, 3]),
            Node::new("to").with_content(Jid::new("5511999", DEFAULT_USER_SERVER)),
            Node::new("to").with_content(Jid::parse("5511999:3@s.whatsapp.net").unwrap()),
            Node::new("to").with_content(Jid::new("99887766", HIDDEN_USER_SERVER)),
            Node::new("to").with_content(Jid::new("", GROUP_SERVER)),
            Node::new("message").with_content(vec![Node::new("media"), Node::new("x").attr("type", "image")]),
        ];
        assert_eq!(GOLDEN.len(), expected.len());

        for ((name, hexed, xml), want) in GOLDEN.iter().zip(expected) {
            let frame = decode_hex(hexed);
            let node = node_from_frame(&frame);
            assert_eq!(node, want, "{name}: decoded node");
            assert_eq!(node.to_xml_string(), *xml, "{name}: xml");
            assert_eq!(hex_of(&marshal(&node)), *hexed, "{name}: re-encoded frame");
        }
    }

    #[test]
    fn long_byte_content_uses_binary_20() {
        let node = Node::new("x").with_content(vec![7u8; 300]);
        let expected = format!("00f802fc0178fd00012c{}", "07".repeat(300));
        let frame = decode_hex(&expected);
        assert_eq!(hex_of(&marshal(&node)), expected);
        assert_eq!(node_from_frame(&frame), node);
    }

    #[test]
    fn many_children_use_list_16() {
        let node = Node::new("list").with_content(vec![Node::new("a"); 300]);
        let expected = format!("00f80271f9012c{}", "f801fc0161".repeat(300));
        assert_eq!(hex_of(&marshal(&node)), expected);

        let decoded = node_from_frame(&decode_hex(&expected));
        assert_eq!(decoded.children().len(), 300);
        assert!(decoded.children().iter().all(|child| child.tag == "a"));
        assert_eq!(decoded, node);
    }

    #[test]
    fn zlib_frame_roundtrip() {
        // Go: zlib(<success t="1234"/>) behind data type byte 2.
        let frame = decode_hex("02789c000800f7fff8034c1aff02123403000e8102a9");
        let payload_hex = "f8034c1aff021234";
        let unpacked = unpack(&frame).unwrap();
        assert_eq!(hex_of(&unpacked), payload_hex, "payload after decompression");
        let node = node_from_frame(&frame);
        assert_eq!(node, Node::new("success").attr("t", "1234"));
        // Re-encoding yields the uncompressed frame (Go doesn't pack): 0 + payload.
        assert_eq!(hex_of(&marshal(&node)), format!("00{payload_hex}"));
    }

    #[test]
    fn nested_documents_roundtrip() {
        let node = Node::new("iq")
            .attr("id", "1234")
            .attr("type", "get")
            .attr("to", Jid::new("48123456789", DEFAULT_USER_SERVER))
            .with_content(vec![
                Node::new("ping").attr("xmlns", "urn:xmpp:ping"),
                Node::new("tokens")
                    .attr("count", "2")
                    .with_content(vec![Node::new("tctoken").with_content(vec![9u8, 8, 7])]),
            ]);
        assert_eq!(node_from_frame(&marshal(&node)), node);
    }

    #[test]
    fn text_that_looks_like_hex_stays_text() {
        // "010203" is valid hex packing, so make sure the decoder still returns text.
        let node = Node::new("x").with_content("010203");
        let data = marshal(&node);
        assert_eq!(hex_of(&data), "00f802fc0178ff03010203");
        assert_eq!(node_from_frame(&data).as_str(), Some("010203"));
    }
}
