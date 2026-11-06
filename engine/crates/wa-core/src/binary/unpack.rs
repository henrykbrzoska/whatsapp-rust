// Ported from the Go client `binary/unpack.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Unpacking of the data type byte in front of every WhatsApp WebSocket frame.

use std::io::Read;

use super::errors::BinaryError;

/// `binary.Unpack` — strips the data type byte and decompresses zlib payloads.
///
/// The first byte tells whether the rest is zlib-compressed (`2 & dataType`).
/// There is no `pack` counterpart: [`super::marshal`] already returns data with
/// a leading zero (i.e. uncompressed), exactly like in Go.
pub fn unpack(data: &[u8]) -> Result<Vec<u8>, BinaryError> {
    let (data_type, data) = data.split_first().ok_or(BinaryError::UnexpectedEof)?;
    if 2 & data_type > 0 {
        let mut decompressor = flate2::read::ZlibDecoder::new(data);
        let mut out = Vec::new();
        decompressor.read_to_end(&mut out).map_err(|err| BinaryError::Zlib(err.to_string()))?;
        Ok(out)
    } else {
        Ok(data.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binary::{marshal, node::Node, unmarshal};

    #[test]
    fn passes_through_uncompressed_data() {
        let packed = marshal(&Node::new("ping"));
        assert_eq!(packed[0], 0, "marshal already writes the data type byte");
        assert_eq!(unpack(&packed).unwrap(), packed[1..]);
    }

    #[test]
    fn decompresses_zlib_data() {
        // Data type byte 2 = zlib compressed payload (no leading zero inside).
        let frame = marshal(&Node::new("success").attr("t", "1234"));
        let compressed = crate::binary::testutil::zlib(&frame[1..]);
        let mut packed = vec![2];
        packed.extend_from_slice(&compressed);

        let unpacked = unpack(&packed).unwrap();
        assert_eq!(unpacked, frame[1..], "unpack drops the data type byte");
        assert_eq!(unmarshal(&unpacked).unwrap(), Node::new("success").attr("t", "1234"));
    }

    #[test]
    fn rejects_empty_and_corrupt_input() {
        assert_eq!(unpack(&[]), Err(BinaryError::UnexpectedEof));
        assert!(matches!(unpack(&[2, 1, 2, 3]), Err(BinaryError::Zlib(_))));
    }
}
