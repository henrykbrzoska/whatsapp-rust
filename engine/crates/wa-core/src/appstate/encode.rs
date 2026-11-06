// Ported from the Go client (`appstate/encode.go`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Encoding of app state mutations and patches.
//!
//! Go counterparts: `MutationInfo`, `PatchInfo`, the mutation loop of
//! `Processor.EncodePatch` and `json.Marshal(index)` in
//! `Go appstate/encode.go`.
//!
//! Key lookup (`Processor.EncodePatch` → `getAppStateKey`) is Task B: the
//! caller resolves the key, expands it with [`ExpandedAppStateKeys::expand`]
//! and passes it in, which keeps this module pure and testable offline.
//!
//! `serde_json` is only a dev-dependency of this crate, so the JSON index is
//! serialized by [`encode_index_json`], which reproduces Go's
//! `encoding/json` escaping byte for byte — the index MAC depends on it.

use prost::Message;
use wa_proto::wa_web_protobuf_sync_action::{SyncActionData, SyncActionValue};
use wa_proto::wa_web_protobufs_server_sync::{
    KeyId, SyncdIndex, SyncdMutation, SyncdPatch, SyncdRecord, SyncdValue, syncd_mutation::SyncdOperation,
};

use crate::crypto::cbc;

use super::hash::{AppStateWarning, HashState, generate_content_mac, generate_index_mac, generate_patch_mac};
use super::{AppStateError, ExpandedAppStateKeys, WAPatchName};

/// What a single mutation changes.
///
/// Go counterpart: `appstate.MutationInfo`.
#[derive(Debug, Clone, PartialEq)]
pub struct MutationInfo {
    /// The thing being mutated, e.g. `["mute", "1234@s.whatsapp.net"]`.
    pub index: Vec<String>,
    /// A static per-action version (`2` for mute, `5` for pin, …).
    pub version: i32,
    /// The action payload.
    pub value: SyncActionValue,
}

/// A whole patch — several mutations of the same collection, sent together.
///
/// Go counterpart: `appstate.PatchInfo`; `time.Time` became `i64` Unix
/// milliseconds, with `0` meaning "now" exactly like Go's zero timestamp.
#[derive(Debug, Clone, PartialEq)]
pub struct PatchInfo {
    /// When the patch was created; `0` = now.
    pub timestamp_millis: i64,
    /// The collection being mutated.
    pub name: WAPatchName,
    /// The individual mutations of this patch.
    pub mutations: Vec<MutationInfo>,
}

impl PatchInfo {
    /// An empty patch for a collection.
    pub fn new(name: impl Into<WAPatchName>) -> Self {
        Self { timestamp_millis: 0, name: name.into(), mutations: Vec::new() }
    }
}

/// One encoded [`SyncdMutation`] plus the MACs that were calculated for it.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodedMutation {
    /// The wire mutation.
    pub mutation: SyncdMutation,
    /// The JSON index that was encrypted (`+` in `index_mac`).
    pub index_json: Vec<u8>,
    /// `HMAC-SHA256(keys.index, index_json)`.
    pub index_mac: Vec<u8>,
    /// The trailing 32 bytes of the value blob.
    pub value_mac: Vec<u8>,
}

/// A fully encoded patch and the state it produced.
#[derive(Debug, Clone, PartialEq)]
pub struct EncodedPatch {
    /// The marshalled `SyncdPatch`, ready to be put into a `<patch>` node.
    pub bytes: Vec<u8>,
    /// The same patch, un-marshalled (useful to double-check the MACs).
    pub patch: SyncdPatch,
    /// The collection state after this patch (version incremented, hash updated).
    pub state: HashState,
    /// Non-fatal problems found while updating the hash.
    pub warnings: Vec<AppStateWarning>,
}

/// `json.Marshal([]string)` — the exact bytes Go puts in `SyncActionData.index`.
///
/// Go's `encoding/json` escapes `"`, `\`, control characters, `<`, `>`, `&` and
/// U+2028/U+2029 by default (`SetEscapeHTML` is never turned off in the Go client).
pub fn encode_index_json(index: &[String]) -> Vec<u8> {
    let mut out = String::new();
    out.push('[');
    for (position, part) in index.iter().enumerate() {
        if position > 0 {
            out.push(',');
        }
        escape_json_string(&mut out, part);
    }
    out.push(']');
    out.into_bytes()
}

fn escape_json_string(out: &mut String, value: &str) {
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            control if (control as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", control as u32)),
            printable => out.push(printable),
        }
    }
    out.push('"');
}

/// `EncodeSyncActionData` — marshals the plaintext of one mutation.
///
/// Sets `value.timestamp` (Go sets it from `PatchInfo.Timestamp`) and always
/// writes `padding` as an empty `bytes` field, exactly like Go.
pub fn encode_sync_action_data(
    index: &[String],
    version: i32,
    value: &SyncActionValue,
    timestamp_millis: i64,
) -> Result<Vec<u8>, AppStateError> {
    let mut value = value.clone();
    value.timestamp = Some(timestamp_millis_or_now(timestamp_millis));
    let data = SyncActionData {
        index: Some(encode_index_json(index)),
        value: Some(value),
        padding: Some(Vec::new()),
        version: Some(version),
    };
    Ok(data.encode_to_vec())
}

/// Encodes one mutation: index JSON, CBC encryption, value MAC and index MAC.
///
/// Go does this inline in `Processor.EncodePatch`; splitting it out makes the
/// crypto/MAC step testable without a store.
pub fn encode_mutation(
    info: &MutationInfo,
    key_id: &[u8],
    keys: &ExpandedAppStateKeys,
    timestamp_millis: i64,
) -> Result<EncodedMutation, AppStateError> {
    let index_json = encode_index_json(&info.index);
    let plaintext = encode_sync_action_data(&info.index, info.version, &info.value, timestamp_millis)?;

    // `cbcutil.Encrypt(key, nil, content)` prepends a fresh random IV to the ciphertext.
    let encrypted_content = cbc::encrypt(&keys.value_encryption, None, &plaintext)?;
    let value_mac = generate_content_mac(SyncdOperation::Set, &encrypted_content, key_id, &keys.value_mac);
    let index_mac = generate_index_mac(&keys.index, &index_json);

    let mut value_blob = encrypted_content;
    value_blob.extend_from_slice(&value_mac);

    Ok(EncodedMutation {
        mutation: SyncdMutation {
            operation: Some(SyncdOperation::Set as i32),
            record: Some(SyncdRecord {
                index: Some(SyncdIndex { blob: Some(index_mac.clone()) }),
                value: Some(SyncdValue { blob: Some(value_blob) }),
                key_id: Some(KeyId { id: Some(key_id.to_vec()) }),
            }),
        },
        index_json,
        index_mac,
        value_mac,
    })
}

/// `Processor.EncodePatch` without the store — builds a signed `SyncdPatch`.
///
/// `get_prev_value_mac(index_mac, max_index)` looks up the value MAC of the
/// previous `SET` for the same index (the store's
/// `GetAppStateMutationMAC`); `max_index` is the position of the mutation in
/// this patch so the caller can resolve collisions inside the patch itself.
///
/// The returned [`EncodedPatch::state`] is the state to persist after the patch
/// is sent: its LTHash includes this patch's mutations and its version is
/// `state.version + 1`, exactly like Go.
pub fn encode_patch<F>(
    key_id: &[u8],
    keys: &ExpandedAppStateKeys,
    state: &HashState,
    patch: &PatchInfo,
    mut get_prev_value_mac: F,
) -> Result<EncodedPatch, AppStateError>
where
    F: FnMut(&[u8], usize) -> Result<Option<Vec<u8>>, AppStateError>,
{
    let timestamp_millis = timestamp_millis_or_now(patch.timestamp_millis);
    let mut mutations = Vec::with_capacity(patch.mutations.len());
    for mutation_info in &patch.mutations {
        mutations.push(encode_mutation(mutation_info, key_id, keys, timestamp_millis)?.mutation);
    }

    let mut new_state = state.clone();
    let warnings = new_state.update_hash(&mutations, &mut get_prev_value_mac)?;
    new_state.version = new_state.version.saturating_add(1);

    let mut syncd_patch = SyncdPatch {
        version: None,
        mutations,
        external_mutations: None,
        snapshot_mac: Some(new_state.generate_snapshot_mac(&patch.name, &keys.snapshot_mac)),
        patch_mac: None,
        key_id: Some(KeyId { id: Some(key_id.to_vec()) }),
        exit_code: None,
        device_index: None,
        client_debug_data: None,
    };
    syncd_patch.patch_mac = Some(generate_patch_mac(&syncd_patch, &patch.name, &keys.patch_mac, new_state.version)?);

    let bytes = syncd_patch.encode_to_vec();
    Ok(EncodedPatch { bytes, patch: syncd_patch, state: new_state, warnings })
}

/// `PatchInfo.Timestamp.IsZero()` handling — `0` means "now", like Go.
fn timestamp_millis_or_now(timestamp_millis: i64) -> i64 {
    if timestamp_millis == 0 {
        match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(0),
            Err(_) => 0,
        }
    } else {
        timestamp_millis
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appstate::decode::{decode_mutation, decode_sync_action_data};
    use wa_proto::wa_web_protobuf_sync_action::MuteAction;

    fn hex(s: &str) -> Vec<u8> {
        hex::decode(s).expect("test fixture is valid hex")
    }

    fn go_key_data() -> Vec<u8> {
        hex("b0b1b2b3b4b5b6b7b8b9babbbcbdbebfc0c1c2c3c4c5c6c7c8c9cacbcccdcecf")
    }

    fn go_keys() -> ExpandedAppStateKeys {
        ExpandedAppStateKeys::expand(&go_key_data()).expect("expand keys")
    }

    fn mute_info() -> MutationInfo {
        MutationInfo {
            index: vec!["mute".to_owned(), "1234@s.whatsapp.net".to_owned()],
            version: 2,
            value: SyncActionValue {
                mute_action: Some(MuteAction { muted: Some(true), mute_end_timestamp: Some(-1), ..Default::default() }),
                ..Default::default()
            },
        }
    }

    #[test]
    fn index_json_matches_go() {
        // Go: json.Marshal([]string{"mute", "1234@s.whatsapp.net"})
        let json = encode_index_json(&["mute".to_owned(), "1234@s.whatsapp.net".to_owned()]);
        assert_eq!(json, hex("5b226d757465222c223132333440732e77686174736170702e6e6574225d"));
        assert_eq!(encode_index_json(&[]), b"[]".to_vec());
        assert_eq!(encode_index_json(&[String::new()]), b"[\"\"]".to_vec());
    }

    #[test]
    fn index_json_escapes_like_go() {
        // Go: json.Marshal([]string{"a\"b", "c\\d", "e\nf", "<g>", "h&i", "\u2028"})
        let json = encode_index_json(&[
            "a\"b".to_owned(),
            "c\\d".to_owned(),
            "e\nf".to_owned(),
            "<g>".to_owned(),
            "h&i".to_owned(),
            "\u{2028}".to_owned(),
            "\u{1}".to_owned(),
        ]);
        assert_eq!(
            String::from_utf8(json).expect("utf-8"),
            r#"["a\"b","c\\d","e\nf","\u003cg\u003e","h\u0026i","\u2028","\u0001"]"#
        );
    }

    #[test]
    fn sync_action_data_roundtrips() {
        let blob = encode_sync_action_data(
            &["mute".to_owned(), "1234@s.whatsapp.net".to_owned()],
            2,
            &mute_info().value,
            1_700_000_000_000,
        )
        .expect("encode");
        let decoded = decode_sync_action_data(&blob).expect("decode");
        assert_eq!(decoded.version, Some(2));
        assert_eq!(decoded.index, Some(encode_index_json(&mute_info().index)));
        assert_eq!(decoded.padding.as_deref(), Some(&b""[..]));
        let value = decoded.value.expect("value");
        assert_eq!(value.timestamp, Some(1_700_000_000_000));
        assert_eq!(value.mute_action.expect("mute").muted, Some(true));
    }

    #[test]
    fn a_zero_timestamp_means_now() {
        let before = timestamp_millis_or_now(0);
        let blob = encode_sync_action_data(&["mute".to_owned()], 2, &mute_info().value, 0).expect("encode");
        let decoded = decode_sync_action_data(&blob).expect("decode");
        let stamp = decoded.value.and_then(|value| value.timestamp).expect("timestamp");
        assert!(stamp >= before, "expected a fresh timestamp, got {stamp} (before {before})");
    }

    #[test]
    fn encode_mutation_matches_the_go_vector_when_the_iv_is_fixed() {
        // Go (fixed IV 0x00..0x0F): value blob and index MAC for the mute mutation.
        let keys = go_keys();
        let key_id = hex("deadbeef");
        let info = mute_info();
        let index_json = encode_index_json(&info.index);
        assert_eq!(
            hex::encode(generate_index_mac(&keys.index, &index_json)),
            "298c3924effcf768b7e3f01794bef4773cec83c3fabd95a527ff0136cd4b447d"
        );

        // Same plaintext and keys, but with the IV Go used.
        let plaintext =
            encode_sync_action_data(&info.index, info.version, &info.value, 1_700_000_000_000).expect("encode");
        let ciphertext =
            cbc::encrypt_with_iv(&keys.value_encryption, &hex("000102030405060708090a0b0c0d0e0f"), &plaintext)
                .expect("encrypt");
        assert_eq!(
            hex::encode(&ciphertext),
            "46663c0ad3fbce1f8cf529c0d418a0f35bbd6c247c7745b8074be6bb9e4a3315e3740992fff48fec7340f7af0ca5060ff5aa862dcdbae65d5be65b3b2699d3cb"
        );

        let encoded = encode_mutation(&info, &key_id, &keys, 1_700_000_000_000).expect("encode mutation");
        assert_eq!(encoded.index_mac, hex("298c3924effcf768b7e3f01794bef4773cec83c3fabd95a527ff0136cd4b447d"));
        assert_eq!(encoded.value_mac.len(), 32);
        assert_eq!(encoded.mutation.operation, Some(SyncdOperation::Set as i32));
    }

    #[test]
    fn encode_then_decode_roundtrip() {
        let keys = go_keys();
        let key_id = hex("deadbeef");
        let info = MutationInfo {
            index: vec!["pin_v1".to_owned(), "5511999@s.whatsapp.net".to_owned()],
            version: 5,
            value: SyncActionValue {
                pin_action: Some(wa_proto::wa_web_protobuf_sync_action::PinAction { pinned: Some(true) }),
                ..Default::default()
            },
        };
        let encoded = encode_mutation(&info, &key_id, &keys, 1_700_000_000_000).expect("encode");
        let decoded = decode_mutation(&encoded.mutation, &keys, true).expect("decode");
        assert_eq!(decoded.index, info.index);
        assert_eq!(decoded.version, 5);
        assert_eq!(decoded.key_id, key_id);
        assert_eq!(decoded.value_mac, encoded.value_mac);
        assert_eq!(decoded.index_mac, encoded.index_mac);
        let action = decoded.action.expect("action");
        assert_eq!(action.timestamp, Some(1_700_000_000_000));
        assert_eq!(action.pin_action.expect("pin").pinned, Some(true));
    }

    #[test]
    fn encode_patch_signs_the_result() {
        let keys = go_keys();
        let key_id = hex("deadbeef");
        let state = HashState::new();
        let mut patch = PatchInfo::new(WAPatchName::REGULAR_HIGH);
        patch.timestamp_millis = 1_700_000_000_000;
        patch.mutations.push(mute_info());

        let encoded = encode_patch(&key_id, &keys, &state, &patch, |_, _| Ok(None)).expect("encode patch");
        assert_eq!(encoded.state.version, 1);
        assert_ne!(encoded.state.hash, [0u8; 128]);
        assert!(encoded.warnings.is_empty());
        assert_eq!(encoded.patch.mutations.len(), 1);
        assert_eq!(encoded.patch.key_id.as_ref().and_then(|k| k.id.as_deref()), Some(&key_id[..]));

        // The patch MAC verifies against the new state version.
        assert!(
            super::super::verify_patch_mac(&encoded.patch, &patch.name, &keys.patch_mac, encoded.state.version).is_ok()
        );
        // The snapshot MAC binds the new LTHash.
        assert!(
            encoded
                .state
                .verify_snapshot_mac(
                    &patch.name,
                    &keys.snapshot_mac,
                    encoded.patch.snapshot_mac.as_deref().expect("snapshot mac")
                )
                .is_ok()
        );

        // And the encoded bytes decode back into the same patch.
        let decoded = SyncdPatch::decode(encoded.bytes.as_slice()).expect("decode patch");
        assert_eq!(decoded.mutations.len(), 1);
        assert_eq!(decoded.snapshot_mac, encoded.patch.snapshot_mac);
        assert_eq!(decoded.patch_mac, encoded.patch.patch_mac);
    }

    #[test]
    fn encode_patch_uses_the_previous_value_mac_for_removals() {
        let keys = go_keys();
        let key_id = hex("deadbeef");
        // First patch: set the mute entry.
        let mut first = PatchInfo::new(WAPatchName::REGULAR_HIGH);
        first.timestamp_millis = 1_700_000_000_000;
        first.mutations.push(mute_info());
        let first_encoded = encode_patch(&key_id, &keys, &HashState::new(), &first, |_, _| Ok(None)).expect("patch");

        // Second patch: a REMOVE would be a SET in the Go client's builders, so drive
        // `update_hash` through a hand-built patch to exercise the lookup.
        let mut state = first_encoded.state.clone();
        let value_blob = first_encoded.patch.mutations[0]
            .record
            .as_ref()
            .and_then(|record| record.value.as_ref())
            .and_then(|value| value.blob.clone())
            .expect("value blob");
        let previous_value_mac = value_blob[value_blob.len() - 32..].to_vec();
        let remove = SyncdMutation {
            operation: Some(SyncdOperation::Remove as i32),
            record: Some(SyncdRecord {
                index: first_encoded.patch.mutations[0].record.as_ref().and_then(|record| record.index.clone()),
                value: Some(SyncdValue { blob: Some(Vec::new()) }),
                key_id: Some(KeyId { id: Some(key_id.clone()) }),
            }),
        };
        let warnings = state.update_hash(&[remove], |_, _| Ok(Some(previous_value_mac.clone()))).expect("update hash");
        assert!(warnings.is_empty());
        assert_eq!(state.hash, [0u8; 128]);
    }

    #[test]
    fn encode_patch_reports_warnings_and_errors() {
        let keys = go_keys();
        let key_id = hex("deadbeef");
        let mut patch = PatchInfo::new(WAPatchName::REGULAR_HIGH);
        patch.timestamp_millis = 1_700_000_000_000;
        patch.mutations.push(mute_info());

        let encoded = encode_patch(&key_id, &keys, &HashState::new(), &patch, |_, _| Err(AppStateError::KeyNotFound));
        assert!(matches!(encoded, Err(AppStateError::KeyNotFound)));

        // An empty patch still bumps the version and produces a signed patch.
        let empty = PatchInfo::new(WAPatchName::REGULAR);
        let encoded = encode_patch(&key_id, &keys, &HashState::new(), &empty, |_, _| Ok(None)).expect("encode");
        assert_eq!(encoded.state.version, 1);
        assert!(encoded.patch.mutations.is_empty());
        assert!(encoded.patch.patch_mac.is_some());
    }
}
