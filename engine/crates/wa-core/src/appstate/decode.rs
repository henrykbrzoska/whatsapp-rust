// Ported from the Go client (`appstate/decode.go`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Decoding of app state mutations and patch lists.
//!
//! Go counterparts: `decodeMutation`, `parseSnapshotInternal`,
//! `parsePatchListInternal` and `ParsePatchList` in
//! `Go appstate/decode.go`.
//!
//! `Processor.decodeMutations`/`DecodePatches` need an app state key store, so
//! they are Task B; they call [`decode_mutation`] per mutation after resolving
//! the key into [`ExpandedAppStateKeys`].

use prost::Message;
use wa_proto::wa_web_protobuf_sync_action::{SyncActionData, SyncActionValue};
use wa_proto::wa_web_protobufs_server_sync::{ExternalBlobReference, SyncdMutations, SyncdPatch, SyncdSnapshot};

use crate::binary::NodeContent;
use crate::binary::node::{AttrValue, Node};
use crate::crypto::cbc;

use super::hash::{VALUE_MAC_LEN, generate_content_mac, generate_index_mac};
use super::{AppStateError, ExpandedAppStateKeys, SyncdOperation, WAPatchName, mutation_operation};

/// Length of the IV prefixed to an encrypted `SyncActionData` (`aes.BlockSize`).
pub const IV_LEN: usize = 16;

/// `appstate.Mutation` — one decoded mutation of an app state patch.
#[derive(Debug, Clone, PartialEq)]
pub struct Mutation {
    /// The app state sync key ID used to encrypt the mutation.
    pub key_id: Vec<u8>,
    /// Whether the mutation sets or removes the entry.
    pub operation: SyncdOperation,
    /// The decrypted `SyncActionValue`.
    pub action: Option<SyncActionValue>,
    /// The `SyncActionData.version` (the action's own version, not the patch's).
    pub version: i32,
    /// The decoded JSON index, e.g. `["mute", "1234@s.whatsapp.net"]`.
    pub index: Vec<String>,
    /// HMAC of the JSON index.
    pub index_mac: Vec<u8>,
    /// The trailing 32 bytes of the encrypted value.
    pub value_mac: Vec<u8>,
    /// The app state version of the patch this mutation came from.
    pub patch_version: u64,
}

/// `appstate.PatchList` — a decoded `<collection>` response.
#[derive(Debug, Clone, PartialEq)]
pub struct PatchList {
    /// The collection these patches belong to.
    pub name: WAPatchName,
    /// `has_more_patches` — the server has more patches waiting.
    pub has_more_patches: bool,
    /// The decoded `SyncdPatch`es, in server order.
    pub patches: Vec<SyncdPatch>,
    /// The full snapshot, when the server sent one instead of/in addition to patches.
    pub snapshot: Option<SyncdSnapshot>,
}

impl PatchList {
    /// An empty patch list for a collection.
    pub fn new(name: impl Into<WAPatchName>) -> Self {
        Self { name: name.into(), has_more_patches: false, patches: Vec::new(), snapshot: None }
    }
}

/// `DownloadExternalFunc` — downloads the blob an `ExternalBlobReference` points at.
///
/// Go passes `func(context.Context, *waServerSync.ExternalBlobReference) ([]byte, error)`;
/// the context is dropped here (cancellation = dropping the future), so the
/// caller finishes the media download first and hands the bytes back.
pub type DownloadExternal<'a> = &'a dyn Fn(&ExternalBlobReference) -> Result<Vec<u8>, AppStateError>;

/// Unmarshals a `SyncActionData` protobuf (the plaintext of one mutation).
///
/// Go unmarshals it inline in `decodeMutation`; the function is public here
/// because key recovery (`recovery.go`) and history sync parse the same message
/// without the surrounding MAC/crypto steps.
pub fn decode_sync_action_data(plaintext: &[u8]) -> Result<SyncActionData, AppStateError> {
    Ok(SyncActionData::decode(plaintext)?)
}

/// `decodeMutation` — verifies the MACs, decrypts and decodes one mutation.
///
/// `keys` must be the [`ExpandedAppStateKeys`] of `mutation.record.key_id`.
/// With `validate_macs == false` the content/index MACs are not checked (Go
/// does that when it cannot fetch the sync key); everything else is identical.
pub fn decode_mutation(
    mutation: &wa_proto::wa_web_protobufs_server_sync::SyncdMutation,
    keys: &ExpandedAppStateKeys,
    validate_macs: bool,
) -> Result<Mutation, AppStateError> {
    let operation = mutation_operation(mutation);
    let record = mutation.record.as_ref().ok_or(AppStateError::InvalidMutation("mutation has no record"))?;
    let key_id = record
        .key_id
        .as_ref()
        .and_then(|key_id| key_id.id.as_deref())
        .ok_or(AppStateError::InvalidMutation("mutation has no key ID"))?;
    let blob = record
        .value
        .as_ref()
        .and_then(|value| value.blob.as_deref())
        .ok_or(AppStateError::InvalidMutation("mutation has no value blob"))?;
    if blob.len() < VALUE_MAC_LEN {
        return Err(AppStateError::InvalidMutation("value blob is shorter than its value MAC"));
    }

    let (content, value_mac) = blob.split_at(blob.len() - VALUE_MAC_LEN);
    if validate_macs {
        let expected_value_mac = generate_content_mac(operation, content, key_id, &keys.value_mac);
        if !constant_time_eq(&expected_value_mac, value_mac) {
            return Err(AppStateError::MismatchingContentMac);
        }
    }

    if content.len() < IV_LEN {
        return Err(AppStateError::InvalidMutation("value blob is shorter than its IV"));
    }
    let (iv, ciphertext) = content.split_at(IV_LEN);
    let plaintext = cbc::decrypt(&keys.value_encryption, iv, ciphertext)?;
    let sync_action = decode_sync_action_data(&plaintext)?;

    let index_mac = record.index.as_ref().and_then(|index| index.blob.as_deref()).unwrap_or_default();
    let index_json = sync_action.index.as_deref().unwrap_or_default();
    if validate_macs {
        let expected_index_mac = generate_index_mac(&keys.index, index_json);
        if !constant_time_eq(&expected_index_mac, index_mac) {
            return Err(AppStateError::MismatchingIndexMac);
        }
    }

    Ok(Mutation {
        key_id: key_id.to_vec(),
        operation,
        action: sync_action.value,
        version: sync_action.version.unwrap_or_default(),
        index: decode_index_json(index_json)?,
        index_mac: index_mac.to_vec(),
        value_mac: value_mac.to_vec(),
        patch_version: 0,
    })
}

/// `ParsePatchList` — decodes a `<collection>` node into a [`PatchList`].
///
/// Any external mutation/snapshot blob is fetched through `download_external`;
/// when the node references one and no downloader is given, an error is
/// returned instead of Go's nil-pointer panic.
pub fn decode_patch_list(
    collection: &Node,
    download_external: Option<DownloadExternal<'_>>,
) -> Result<PatchList, AppStateError> {
    let name = collection
        .attrs
        .get("name")
        .and_then(AttrValue::as_str)
        .ok_or(AppStateError::InvalidPatchList("collection has no name attribute"))?;

    let snapshot = parse_snapshot(collection, download_external)?;
    let patches = parse_patches(collection, download_external)?;

    Ok(PatchList {
        name: WAPatchName::new(name),
        has_more_patches: attr_bool(collection, "has_more_patches"),
        patches,
        snapshot,
    })
}

/// `parseSnapshotInternal` — the `<snapshot>` child of a `<collection>` node.
fn parse_snapshot(
    collection: &Node,
    download_external: Option<DownloadExternal<'_>>,
) -> Result<Option<SyncdSnapshot>, AppStateError> {
    let Some(snapshot_node) = collection.optional_child_by_tag(&["snapshot"]) else {
        return Ok(None);
    };
    let NodeContent::Bytes(raw_snapshot) = &snapshot_node.content else {
        return Ok(None);
    };
    let reference = ExternalBlobReference::decode(raw_snapshot.as_slice())
        .map_err(|_| AppStateError::InvalidPatchList("failed to unmarshal the snapshot blob reference"))?;
    let download = download_external
        .ok_or(AppStateError::InvalidPatchList("the snapshot is external but cannot be downloaded"))?;
    let raw_data = download(&reference)?;
    let snapshot = SyncdSnapshot::decode(raw_data.as_slice())
        .map_err(|_| AppStateError::InvalidPatchList("failed to unmarshal the downloaded snapshot"))?;
    Ok(Some(snapshot))
}

/// `parsePatchListInternal` — the `<patch>` children of a `<patches>` node.
fn parse_patches(
    collection: &Node,
    download_external: Option<DownloadExternal<'_>>,
) -> Result<Vec<SyncdPatch>, AppStateError> {
    let Some(patches_node) = collection.optional_child_by_tag(&["patches"]) else {
        return Ok(Vec::new());
    };
    let mut patches = Vec::with_capacity(patches_node.children().len());
    for patch_node in patches_node.children() {
        if patch_node.tag != "patch" {
            continue;
        }
        let NodeContent::Bytes(raw_patch) = &patch_node.content else {
            continue;
        };
        let mut patch = SyncdPatch::decode(raw_patch.as_slice())
            .map_err(|_| AppStateError::InvalidPatchList("failed to unmarshal a patch"))?;
        if let (Some(reference), Some(download)) = (patch.external_mutations.clone(), download_external) {
            let raw_data = download(&reference)?;
            let downloaded = SyncdMutations::decode(raw_data.as_slice())
                .map_err(|_| AppStateError::InvalidPatchList("failed to unmarshal the mutation list"))?;
            if downloaded.mutations.is_empty() {
                return Err(AppStateError::InvalidPatchList("didn't get any mutations from the external download"));
            }
            patch.mutations = downloaded.mutations;
        }
        patches.push(patch);
    }
    Ok(patches)
}

/// Reads an attribute as a boolean (`has_more_patches`).
///
/// The server writes booleans as the strings `true`/`false`; a parsed
/// [`AttrValue::Bool`] (or `1`/`0`) is accepted too.
fn attr_bool(node: &Node, key: &str) -> bool {
    match node.attrs.get(key) {
        Some(AttrValue::Bool(value)) => *value,
        Some(AttrValue::Int(value)) => *value != 0,
        Some(AttrValue::Uint(value)) => *value != 0,
        Some(AttrValue::Text(text)) => text == "true" || text == "1",
        Some(AttrValue::Jid(_)) | None => false,
    }
}

/// Decodes the JSON index of a mutation into a list of strings.
///
/// Go uses `json.Unmarshal(syncAction.Index, &index)`; `serde_json` is only a
/// dev-dependency of this crate, so the array-of-strings subset is parsed here —
/// exactly the shape WhatsApp uses (`["mute", "1234@s.whatsapp.net"]`).
pub fn decode_index_json(input: &[u8]) -> Result<Vec<String>, AppStateError> {
    let text = std::str::from_utf8(input).map_err(|_| AppStateError::InvalidIndexJson("not valid UTF-8"))?;
    let mut cursor = JsonCursor { bytes: text.as_bytes(), pos: 0 };
    cursor.skip_whitespace();
    if cursor.peek() == Some(b'n') {
        // Go unmarshals `null` into a nil []string without an error.
        cursor.expect_literal("null")?;
        cursor.skip_whitespace();
        cursor.expect_end()?;
        return Ok(Vec::new());
    }
    cursor.expect(b'[')?;
    let mut out = Vec::new();
    cursor.skip_whitespace();
    if cursor.peek() == Some(b']') {
        cursor.pos += 1;
        cursor.skip_whitespace();
        cursor.expect_end()?;
        return Ok(out);
    }
    loop {
        cursor.skip_whitespace();
        out.push(cursor.parse_string()?);
        cursor.skip_whitespace();
        match cursor.next() {
            Some(b',') => continue,
            Some(b']') => break,
            _ => return Err(AppStateError::InvalidIndexJson("expected ',' or ']'")),
        }
    }
    cursor.skip_whitespace();
    cursor.expect_end()?;
    Ok(out)
}

/// A minimal cursor over a JSON document holding an array of strings.
struct JsonCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl JsonCursor<'_> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek();
        if byte.is_some() {
            self.pos += 1;
        }
        byte
    }

    fn skip_whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.pos += 1;
        }
    }

    fn expect(&mut self, expected: u8) -> Result<(), AppStateError> {
        if self.next() == Some(expected) {
            Ok(())
        } else {
            Err(AppStateError::InvalidIndexJson("unexpected character"))
        }
    }

    fn expect_literal(&mut self, literal: &str) -> Result<(), AppStateError> {
        if self.bytes[self.pos..].starts_with(literal.as_bytes()) {
            self.pos += literal.len();
            Ok(())
        } else {
            Err(AppStateError::InvalidIndexJson("unexpected literal"))
        }
    }

    fn expect_end(&mut self) -> Result<(), AppStateError> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err(AppStateError::InvalidIndexJson("trailing data after the index array"))
        }
    }

    fn parse_string(&mut self) -> Result<String, AppStateError> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            match self.next() {
                Some(b'"') => return Ok(out),
                Some(b'\\') => self.parse_escape(&mut out)?,
                Some(byte) if byte < 0x20 => {
                    return Err(AppStateError::InvalidIndexJson("unescaped control character in a string"));
                }
                Some(byte) => {
                    // Re-assemble the UTF-8 sequence that starts with this byte.
                    let start = self.pos - 1;
                    let continuation = match byte {
                        0xC0..=0xDF => 1,
                        0xE0..=0xEF => 2,
                        0xF0..=0xF7 => 3,
                        _ => 0,
                    };
                    let slice = self
                        .bytes
                        .get(start..self.pos + continuation)
                        .ok_or(AppStateError::InvalidIndexJson("truncated UTF-8 sequence"))?;
                    let decoded = std::str::from_utf8(slice)
                        .map_err(|_| AppStateError::InvalidIndexJson("invalid UTF-8 in a string"))?;
                    out.push_str(decoded);
                    self.pos += continuation;
                }
                None => return Err(AppStateError::InvalidIndexJson("unterminated string")),
            }
        }
    }

    fn parse_escape(&mut self, out: &mut String) -> Result<(), AppStateError> {
        match self.next() {
            Some(b'"') => out.push('"'),
            Some(b'\\') => out.push('\\'),
            Some(b'/') => out.push('/'),
            Some(b'b') => out.push('\u{08}'),
            Some(b'f') => out.push('\u{0c}'),
            Some(b'n') => out.push('\n'),
            Some(b'r') => out.push('\r'),
            Some(b't') => out.push('\t'),
            Some(b'u') => {
                let first = self.parse_hex4()?;
                if (0xD800..0xDC00).contains(&first) {
                    // High surrogate: a low surrogate escape must follow.
                    self.expect(b'\\')?;
                    self.expect(b'u')?;
                    let second = self.parse_hex4()?;
                    if !(0xDC00..0xE000).contains(&second) {
                        return Err(AppStateError::InvalidIndexJson("invalid low surrogate"));
                    }
                    let scalar = 0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                    let ch = char::from_u32(scalar).ok_or(AppStateError::InvalidIndexJson("invalid surrogate pair"))?;
                    out.push(ch);
                } else {
                    let ch = char::from_u32(first).ok_or(AppStateError::InvalidIndexJson("invalid unicode escape"))?;
                    out.push(ch);
                }
            }
            _ => return Err(AppStateError::InvalidIndexJson("invalid escape sequence")),
        }
        Ok(())
    }

    fn parse_hex4(&mut self) -> Result<u32, AppStateError> {
        let slice = self
            .bytes
            .get(self.pos..self.pos + 4)
            .ok_or(AppStateError::InvalidIndexJson("truncated unicode escape"))?;
        let text = std::str::from_utf8(slice).map_err(|_| AppStateError::InvalidIndexJson("invalid unicode escape"))?;
        let value =
            u32::from_str_radix(text, 16).map_err(|_| AppStateError::InvalidIndexJson("invalid unicode escape"))?;
        self.pos += 4;
        Ok(value)
    }
}

/// Compares two byte strings without an early exit on the first differing byte.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::NodeContent;
    use crate::binary::node::Node;
    use prost::Message;
    use wa_proto::wa_web_protobufs_server_sync::{
        KeyId, SyncdIndex, SyncdMutation, SyncdRecord, SyncdValue, SyncdVersion, syncd_mutation::SyncdOperation as Op,
    };

    fn hex(s: &str) -> Vec<u8> {
        hex::decode(s).expect("test fixture is valid hex")
    }

    fn go_key_data() -> Vec<u8> {
        hex("b0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecf")
    }

    /// The mutation Go produced with `MUTE`, `1234@s.whatsapp.net`, key ID `deadbeef`.
    fn go_mutation() -> SyncdMutation {
        SyncdMutation {
            operation: Some(Op::Set as i32),
            record: Some(SyncdRecord {
                index: Some(SyncdIndex {
                    blob: Some(hex("298c3924effcf768b7e3f01794bef4773cec83c3fabd95a527ff0136cd4b447d")),
                }),
                value: Some(SyncdValue {
                    blob: Some(hex(
                        "000102030405060708090a0b0c0d0e0f46663c0ad3fbce1f8cf529c0d418a0f35bbd6c247c7745b8074be6bb9e4a3315e3740992fff48fec7340f7af0ca5060ff5aa862dcdbae65d5be65b3b2699d3cbf65304fcaf0e02d37ef64ba2dac345989503c62574ac4caef61ec54bca0fa5fd",
                    )),
                }),
                key_id: Some(KeyId { id: Some(hex("deadbeef")) }),
            }),
        }
    }

    fn collection(children: Vec<Node>, has_more: Option<bool>) -> Node {
        let mut node = Node::new("collection").attr("name", "regular_low");
        if let Some(has_more) = has_more {
            node = node.attr("has_more_patches", has_more);
        }
        node.content = NodeContent::Nodes(children);
        node
    }

    #[test]
    fn decodes_a_go_produced_sync_action_data() {
        // Go: proto.Marshal(&waSyncAction.SyncActionData{Index: ["mute","1234@s.whatsapp.net"], ...})
        let blob = hex(
            "0a1e5b226d757465222c223132333440732e77686174736170702e6e6574225d12160880d095ffbc31220d080110ffffffffffffffffff011a002002",
        );
        let data = decode_sync_action_data(&blob).expect("decode");
        assert_eq!(data.index, Some(hex("5b226d757465222c223132333440732e77686174736170702e6e6574225d")));
        assert_eq!(data.version, Some(2));
        assert_eq!(data.padding.as_deref(), Some(&b""[..]));
        let value = data.value.expect("action value");
        assert_eq!(value.timestamp, Some(1_700_000_000_000));
        let mute = value.mute_action.expect("mute action");
        assert_eq!(mute.muted, Some(true));
        assert_eq!(mute.mute_end_timestamp, Some(-1));
    }

    #[test]
    fn rejects_a_corrupt_sync_action_data() {
        assert!(matches!(decode_sync_action_data(&[0xFF, 0xFF, 0xFF]), Err(AppStateError::ProtobufDecode(_))));
    }

    #[test]
    fn decodes_a_go_produced_mutation() {
        let keys = ExpandedAppStateKeys::expand(&go_key_data()).expect("expand keys");
        let mutation = decode_mutation(&go_mutation(), &keys, true).expect("decode mutation");
        assert_eq!(mutation.key_id, hex("deadbeef"));
        assert_eq!(mutation.operation, SyncdOperation::Set);
        assert_eq!(mutation.version, 2);
        assert_eq!(mutation.index, vec!["mute".to_owned(), "1234@s.whatsapp.net".to_owned()]);
        assert_eq!(mutation.index_mac, hex("298c3924effcf768b7e3f01794bef4773cec83c3fabd95a527ff0136cd4b447d"));
        assert_eq!(mutation.value_mac, hex("f65304fcaf0e02d37ef64ba2dac345989503c62574ac4caef61ec54bca0fa5fd"));
        assert_eq!(mutation.value_mac.len(), VALUE_MAC_LEN);
        assert_eq!(mutation.patch_version, 0);
        let action = mutation.action.expect("action");
        assert_eq!(action.timestamp, Some(1_700_000_000_000));
        let mute = action.mute_action.expect("mute action");
        assert_eq!(mute.muted, Some(true));
        assert_eq!(mute.mute_end_timestamp, Some(-1));
    }

    #[test]
    fn rejects_a_tampered_mutation_content_mac() {
        let keys = ExpandedAppStateKeys::expand(&go_key_data()).expect("expand keys");
        let mut mutation = go_mutation();
        if let Some(blob) =
            mutation.record.as_mut().and_then(|record| record.value.as_mut()).and_then(|value| value.blob.as_mut())
        {
            blob[IV_LEN] ^= 0xFF; // flip a ciphertext byte
        }
        assert!(matches!(decode_mutation(&mutation, &keys, true), Err(AppStateError::MismatchingContentMac)));
        // Without MAC validation the tampered ciphertext fails to unpad instead.
        assert!(decode_mutation(&mutation, &keys, false).is_err());
    }

    #[test]
    fn rejects_a_tampered_index_mac() {
        let keys = ExpandedAppStateKeys::expand(&go_key_data()).expect("expand keys");
        let mut mutation = go_mutation();
        if let Some(blob) =
            mutation.record.as_mut().and_then(|record| record.index.as_mut()).and_then(|index| index.blob.as_mut())
        {
            blob[0] ^= 0xFF;
        }
        assert!(matches!(decode_mutation(&mutation, &keys, true), Err(AppStateError::MismatchingIndexMac)));
    }

    #[test]
    fn rejects_malformed_mutations_by_value_not_panic() {
        let keys = ExpandedAppStateKeys::expand(&go_key_data()).expect("expand keys");
        let no_record = SyncdMutation { operation: Some(Op::Set as i32), record: None };
        assert!(matches!(decode_mutation(&no_record, &keys, true), Err(AppStateError::InvalidMutation(_))));

        let mut no_key_id = go_mutation();
        if let Some(record) = no_key_id.record.as_mut() {
            record.key_id = None;
        }
        assert!(matches!(decode_mutation(&no_key_id, &keys, true), Err(AppStateError::InvalidMutation(_))));

        let mut short_blob = go_mutation();
        if let Some(record) = short_blob.record.as_mut() {
            record.value = Some(SyncdValue { blob: Some(vec![0u8; 8]) });
        }
        assert!(matches!(decode_mutation(&short_blob, &keys, true), Err(AppStateError::InvalidMutation(_))));

        let mut no_value = go_mutation();
        if let Some(record) = no_value.record.as_mut() {
            record.value = None;
        }
        assert!(matches!(decode_mutation(&no_value, &keys, true), Err(AppStateError::InvalidMutation(_))));

        // 32 bytes of MAC with no IV in front of it.
        let mut no_iv = go_mutation();
        if let Some(record) = no_iv.record.as_mut() {
            record.value = Some(SyncdValue { blob: Some(vec![0u8; VALUE_MAC_LEN]) });
        }
        assert!(matches!(decode_mutation(&no_iv, &keys, false), Err(AppStateError::InvalidMutation(_))));
    }

    #[test]
    fn index_json_parses_the_go_output() {
        let json = hex("5b226d757465222c223132333440732e77686174736170702e6e6574225d");
        assert_eq!(decode_index_json(&json).expect("parse"), vec!["mute".to_owned(), "1234@s.whatsapp.net".to_owned()]);
    }

    #[test]
    fn index_json_handles_whitespace_escapes_and_edge_cases() {
        assert_eq!(decode_index_json(b"[]").expect("parse"), Vec::<String>::new());
        assert_eq!(decode_index_json(b"null").expect("parse"), Vec::<String>::new());
        assert_eq!(
            decode_index_json(br#" [ "a" , "b\"c" , "\\" , "line\nbreak" , "snowman \u2603" ] "#).expect("parse"),
            vec!["a", "b\"c", "\\", "line\nbreak", "snowman \u{2603}"]
        );
        assert_eq!(decode_index_json(br#"["\u003c"]"#).expect("parse"), vec!["<"]);
        assert_eq!(decode_index_json(br#"["\ud83d\ude00"]"#).expect("parse"), vec!["\u{1F600}"]);
        assert_eq!(decode_index_json(br#"["a\/b"]"#).expect("parse"), vec!["a/b"]);
        assert_eq!(decode_index_json(br#"["\b\f\r\t"]"#).expect("parse"), vec!["\u{8}\u{c}\r\t"]);
        assert_eq!(decode_index_json("[\"héllo\"]".as_bytes()).expect("parse"), vec!["héllo"]);
    }

    #[test]
    fn index_json_rejects_invalid_documents() {
        for bad in [
            &b""[..],
            b"[",
            b"[1]",
            b"[\"a\"] extra",
            b"[\"a\"]",
            b"\"a\"",
            b"[\"a]",
            b"[\"\\q\"]",
            b"[\"\\ud83d\"]",
            b"[\"\\u12\"]",
            b"[\"a\"] [\"b\"]",
        ] {
            // `["a"]` is valid; only the malformed ones must fail.
            let result = decode_index_json(bad);
            if bad == b"[\"a\"]" {
                assert!(result.is_ok(), "expected {:?} to parse", String::from_utf8_lossy(bad));
            } else {
                assert!(result.is_err(), "expected {:?} to fail", String::from_utf8_lossy(bad));
            }
        }
        // An unescaped control character is invalid JSON too.
        assert!(decode_index_json(b"[\"a\x01b\"]").is_err());
    }

    fn patch_bytes() -> Vec<u8> {
        SyncdPatch {
            version: Some(SyncdVersion { version: Some(5) }),
            mutations: vec![go_mutation()],
            snapshot_mac: Some(vec![1u8; 32]),
            patch_mac: Some(vec![2u8; 32]),
            key_id: Some(KeyId { id: Some(hex("deadbeef")) }),
            ..Default::default()
        }
        .encode_to_vec()
    }

    #[test]
    fn decodes_a_patch_list_node() {
        let patches = Node::new("patches").with_content(vec![
            Node::new("patch").with_content(patch_bytes()),
            // Non-patch children and non-byte content are skipped, like Go.
            Node::new("not-a-patch").with_content(patch_bytes()),
            Node::new("patch").with_content("text"),
        ]);
        let node = collection(vec![patches], Some(true));
        let list = decode_patch_list(&node, None).expect("decode patch list");
        assert_eq!(list.name.as_str(), "regular_low");
        assert!(list.has_more_patches);
        assert!(list.snapshot.is_none());
        assert_eq!(list.patches.len(), 1);
        assert_eq!(list.patches[0].version.as_ref().and_then(|v| v.version), Some(5));
        assert_eq!(list.patches[0].mutations.len(), 1);
    }

    #[test]
    fn decodes_a_patch_list_without_patches_or_flag() {
        let list = decode_patch_list(&collection(Vec::new(), None), None).expect("decode patch list");
        assert!(list.patches.is_empty());
        assert!(!list.has_more_patches);
        assert!(list.snapshot.is_none());

        let only_flag = collection(Vec::new(), Some(false));
        assert!(!decode_patch_list(&only_flag, None).expect("decode").has_more_patches);
    }

    #[test]
    fn a_collection_without_a_name_is_rejected() {
        let node = Node::new("collection");
        assert!(matches!(
            decode_patch_list(&node, None),
            Err(AppStateError::InvalidPatchList("collection has no name attribute"))
        ));
    }

    #[test]
    fn a_broken_patch_payload_is_rejected() {
        let patches = Node::new("patches").with_content(vec![Node::new("patch").with_content(vec![0xFF, 0xFF, 0xFF])]);
        assert!(matches!(
            decode_patch_list(&collection(vec![patches], None), None),
            Err(AppStateError::InvalidPatchList(_))
        ));
    }

    #[test]
    fn external_mutations_are_downloaded_through_the_callback() {
        let mut patch = SyncdPatch {
            version: Some(SyncdVersion { version: Some(9) }),
            external_mutations: Some(ExternalBlobReference {
                direct_path: Some("/mms/x".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let blob = SyncdMutations { mutations: vec![go_mutation()] }.encode_to_vec();
        let patches = Node::new("patches").with_content(vec![Node::new("patch").with_content(patch.encode_to_vec())]);

        let download = |reference: &ExternalBlobReference| {
            assert_eq!(reference.direct_path.as_deref(), Some("/mms/x"));
            Ok(blob.clone())
        };
        let list = decode_patch_list(&collection(vec![patches], None), Some(&download)).expect("decode");
        assert_eq!(list.patches.len(), 1);
        assert_eq!(list.patches[0].mutations.len(), 1);

        // Without a downloader the reference is left alone (Go's behaviour).
        let patches = Node::new("patches").with_content(vec![Node::new("patch").with_content(patch.encode_to_vec())]);
        let list = decode_patch_list(&collection(vec![patches], None), None).expect("decode");
        assert!(list.patches[0].mutations.is_empty());
        assert!(list.patches[0].external_mutations.is_some());

        // An empty external mutation list is an error.
        patch.external_mutations = None;
        let empty = SyncdMutations { mutations: Vec::new() }.encode_to_vec();
        let empty_download = |_: &ExternalBlobReference| Ok(empty.clone());
        let with_external =
            SyncdPatch { external_mutations: Some(ExternalBlobReference::default()), ..Default::default() };
        let patches =
            Node::new("patches").with_content(vec![Node::new("patch").with_content(with_external.encode_to_vec())]);
        assert!(matches!(
            decode_patch_list(&collection(vec![patches], None), Some(&empty_download)),
            Err(AppStateError::InvalidPatchList(_))
        ));
    }

    #[test]
    fn snapshots_are_decoded_through_the_download_callback() {
        let snapshot = SyncdSnapshot {
            version: Some(SyncdVersion { version: Some(42) }),
            records: vec![go_mutation().record.expect("record")],
            mac: Some(vec![3u8; 32]),
            key_id: Some(KeyId { id: Some(hex("deadbeef")) }),
        };
        let reference = ExternalBlobReference { direct_path: Some("/mms/snapshot".into()), ..Default::default() };
        let snapshot_node = Node::new("snapshot").with_content(reference.encode_to_vec());
        let node = collection(vec![snapshot_node], None);

        let download = |_: &ExternalBlobReference| Ok(snapshot.encode_to_vec());
        let list = decode_patch_list(&node, Some(&download)).expect("decode");
        let decoded = list.snapshot.expect("snapshot");
        assert_eq!(decoded.version.as_ref().and_then(|v| v.version), Some(42));
        assert_eq!(decoded.records.len(), 1);
        assert_eq!(decoded.mac, Some(vec![3u8; 32]));

        // Without a downloader this is an error, not a panic.
        assert!(matches!(
            decode_patch_list(&node, None),
            Err(AppStateError::InvalidPatchList("the snapshot is external but cannot be downloaded"))
        ));

        // A broken blob reference is an error too.
        let broken = collection(vec![Node::new("snapshot").with_content(vec![0xFF, 0xFF, 0xFF])], None);
        assert!(matches!(decode_patch_list(&broken, Some(&download)), Err(AppStateError::InvalidPatchList(_))));

        // A snapshot node without byte content is ignored, like Go.
        let text_snapshot = collection(vec![Node::new("snapshot").with_content("text")], None);
        assert!(decode_patch_list(&text_snapshot, None).expect("decode").snapshot.is_none());
    }
}
