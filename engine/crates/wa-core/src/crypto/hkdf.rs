// Ported from the Go client (`util/hkdfutil/hkdf.go`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! HKDF-SHA256 (RFC 5869) — the key-derivation wrapper used all over the
//! WhatsApp protocol (media keys, `Signal` chains, app-state keys).
//!
//! Go counterpart: `hkdfutil.SHA256(key, salt, info, length)` in
//! `Go util/hkdfutil/hkdf.go`, itself a thin wrapper around
//! `golang.org/x/crypto/hkdf`.
//!
//! Go passes `nil` salts in several call sites; `x/crypto/hkdf` treats a `nil`
//! salt as `HashLen` zero bytes, which is exactly what [`Hkdf::new`] with
//! `None` does, so [`sha256`] maps an empty salt to `None` and stays
//! byte-compatible with Go.

use hkdf::{Hkdf, HkdfExtract};
use sha2::Sha256;

/// Length of the HKDF-SHA256 pseudorandom key (`HashLen` = 32 bytes).
pub const PRK_LEN: usize = 32;

/// Maximum HKDF-SHA256 output (`255 * HashLen`), the RFC 5869 limit.
pub const MAX_OUTPUT_LEN: usize = 255 * PRK_LEN;

/// `hkdf.Extract` — turns input keying material into a pseudorandom key.
///
/// Go counterpart: the extract half of `golang.org/x/crypto/hkdf.New`.
/// An empty `salt` is treated as `HashLen` zero bytes, like Go's `nil` salt.
pub fn extract(salt: &[u8], ikm: &[u8]) -> [u8; PRK_LEN] {
    let salt = if salt.is_empty() { None } else { Some(salt) };
    let mut hkdf = HkdfExtract::<Sha256>::new(salt);
    hkdf.input_ikm(ikm);
    let (prk, _) = hkdf.finalize();
    prk.into()
}

/// `hkdf.Expand` — expands a pseudorandom key into `len` output bytes.
///
/// Go counterpart: the expand half of `golang.org/x/crypto/hkdf.HKDF.Read`.
/// Output is clamped to [`MAX_OUTPUT_LEN`]; a `prk` shorter than [`PRK_LEN`]
/// is rejected by the underlying crate and yields an empty vector (it can only
/// happen on caller error — Go would panic here, we do not).
pub fn hkdf_expand(prk: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let len = len.min(MAX_OUTPUT_LEN);
    let Ok(hkdf) = Hkdf::<Sha256>::from_prk(prk) else {
        tracing::warn!(prk_len = prk.len(), "hkdf: refusing to expand an invalid pseudorandom key");
        return Vec::new();
    };
    let mut okm = vec![0u8; len];
    if let Err(e) = hkdf.expand(info, &mut okm) {
        tracing::warn!(error = %e, len, "hkdf: expand failed");
        return Vec::new();
    }
    okm
}

/// `hkdfutil.SHA256` — extract-and-expand in one step.
///
/// Go: `hkdfutil.SHA256(key, salt, info, length uint8) []byte`. The `u8`
/// length caps the output at 255 bytes, exactly like the Go signature.
pub fn sha256(key: &[u8], salt: &[u8], info: &[u8], length: u8) -> Vec<u8> {
    let hkdf = Hkdf::<Sha256>::new(if salt.is_empty() { None } else { Some(salt) }, key);
    let mut okm = vec![0u8; length as usize];
    if let Err(e) = hkdf.expand(info, &mut okm) {
        // Unreachable: `length` is a `u8`, so the output never exceeds 255 bytes.
        tracing::warn!(error = %e, length, "hkdf: expand failed");
        return Vec::new();
    }
    okm
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        hex::decode(s).expect("test fixture is valid hex")
    }

    #[test]
    fn rfc5869_test_case_1() {
        // RFC 5869 Appendix A.1.
        let ikm = hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let salt = hex("000102030405060708090a0b0c");
        let info = hex("f0f1f2f3f4f5f6f7f8f9");

        assert_eq!(
            hex::encode(extract(&salt, &ikm)),
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5"
        );
        assert_eq!(
            hex::encode(sha256(&ikm, &salt, &info, 42)),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
        assert_eq!(hkdf_expand(&extract(&salt, &ikm), &info, 42), sha256(&ikm, &salt, &info, 42));
    }

    #[test]
    fn rfc5869_test_case_3_uses_zero_salt() {
        // RFC 5869 Appendix A.3 has a zero-length/zero salt — the same as Go's `nil`.
        let ikm = hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let expected = "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8";
        assert_eq!(hex::encode(sha256(&ikm, &[], &[], 42)), expected);
        // A non-nil salt of `HashLen` zero bytes must give the same answer.
        assert_eq!(hex::encode(sha256(&ikm, &[0u8; PRK_LEN], &[], 42)), expected);
        assert_eq!(hex::encode(hkdf_expand(&extract(&[], &ikm), &[], 42)), expected);
    }

    #[test]
    fn go_vector_nil_salt() {
        // Go: hkdfutil.SHA256(0x0b*22, nil, 0xf0..0xf9, 42)
        let ikm = hex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let info = hex("f0f1f2f3f4f5f6f7f8f9");
        assert_eq!(
            hex::encode(sha256(&ikm, &[], &info, 42)),
            "abbafb13f5c1bc489d4203135817956dd521b39e3bd61d1cc85cef884d1f8e2e2ca9c19f23df620dd394"
        );
    }

    #[test]
    fn expand_is_clamped_and_short_prk_is_rejected() {
        assert_eq!(hkdf_expand(&[0u8; PRK_LEN], b"", 255 * 32 + 100).len(), MAX_OUTPUT_LEN);
        assert!(hkdf_expand(b"too short", b"", 32).is_empty());
        assert_eq!(sha256(b"key", b"salt", b"info", 0).len(), 0);
    }
}
