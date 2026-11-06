// Ported from the Go client (`appstate/lthash/lthash.go`), © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! LTHash — the summation hash WhatsApp uses to keep app state integrity.
//!
//! Every item is hashed with HKDF-SHA256 (`hkdf_info`, `hkdf_size` output
//! bytes) and added to a 128-byte buffer interpreted as 64 little-endian
//! `u16` lanes that wrap modulo 2^16. Removing an item subtracts its hash
//! again, so the digest only depends on the multiset of items, not the order
//! they arrived in — which is what lets a client verify a server-side patch
//! without replaying the whole history.
//!
//! Byte-for-byte port of the Go package; the golden vectors in the tests were
//! generated with `go run` against `Go appstate/lthash`.
//!
//! Deviations: Go slices the input and would panic on a length mismatch; here
//! a mismatch is [`AppStateError::InvalidHashLength`]. Nothing else differs.

use crate::crypto::hkdf;

use super::AppStateError;

/// `lthash.LTHash` — a configured LTHash instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LTHash {
    /// HKDF info string, e.g. `b"WhatsApp Patch Integrity"`.
    pub hkdf_info: &'static [u8],
    /// HKDF output length in bytes (`uint8` in Go).
    pub hkdf_size: u8,
}

/// `lthash.WAPatchIntegrity` — the instance used by app state sync patches.
pub const WAPATCH_INTEGRITY: LTHash = LTHash { hkdf_info: b"WhatsApp Patch Integrity", hkdf_size: 128 };

/// The two primitive operations of [`LTHash::perform_mutations`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LtHashOp {
    /// Add the item's HKDF digest lane-wise.
    Add,
    /// Subtract the item's HKDF digest lane-wise.
    Subtract,
}

impl LTHash {
    /// Builds an instance with an explicit HKDF info string and digest size.
    pub const fn new(hkdf_info: &'static [u8], hkdf_size: u8) -> Self {
        Self { hkdf_info, hkdf_size }
    }

    /// `SubtractThenAdd` — returns a new buffer (the input is left untouched).
    pub fn subtract_then_add(&self, base: &[u8], subtract: &[&[u8]], add: &[&[u8]]) -> Result<Vec<u8>, AppStateError> {
        let mut output = base.to_vec();
        self.subtract_then_add_in_place(&mut output, subtract, add)?;
        Ok(output)
    }

    /// `SubtractThenAddInPlace` — subtracts every item, then adds every item.
    pub fn subtract_then_add_in_place(
        &self,
        base: &mut [u8],
        subtract: &[&[u8]],
        add: &[&[u8]],
    ) -> Result<(), AppStateError> {
        self.check_base(base.len())?;
        self.multiple_op(base, subtract, LtHashOp::Subtract)?;
        self.multiple_op(base, add, LtHashOp::Add)
    }

    /// `PerformMutations` — applies an ordered list of add/subtract operations.
    ///
    /// Equivalent to [`Self::subtract_then_add_in_place`] for the same inputs:
    /// lane-wise addition wraps modulo 2^16, so the order of the operations
    /// does not change the digest.
    pub fn perform_mutations(&self, base: &mut [u8], mutations: &[(LtHashOp, &[u8])]) -> Result<(), AppStateError> {
        self.check_base(base.len())?;
        for (op, item) in mutations {
            let digest = self.hash_item(item);
            check_len(base.len(), digest.len())?;
            perform_pointwise_with_overflow(base, &digest, *op);
        }
        Ok(())
    }

    /// The HKDF digest of one item (`hkdfutil.SHA256(item, nil, info, size)`).
    pub fn hash_item(&self, item: &[u8]) -> Vec<u8> {
        hkdf::sha256(item, &[], self.hkdf_info, self.hkdf_size)
    }

    /// Rejects a base buffer that is not exactly one `hkdf_size`-long, even-lengthed digest.
    fn check_base(&self, base_len: usize) -> Result<(), AppStateError> {
        let expected = self.hkdf_size as usize;
        if base_len != expected || !base_len.is_multiple_of(2) {
            return Err(AppStateError::InvalidHashLength { expected, actual: base_len });
        }
        Ok(())
    }

    fn multiple_op(&self, base: &mut [u8], items: &[&[u8]], op: LtHashOp) -> Result<(), AppStateError> {
        for item in items {
            let digest = self.hash_item(item);
            if digest.len() != self.hkdf_size as usize {
                return Err(AppStateError::InvalidKeyData("LTHash item digest was not produced"));
            }
            check_len(base.len(), digest.len())?;
            perform_pointwise_with_overflow(base, &digest, op);
        }
        Ok(())
    }
}

fn check_len(base_len: usize, item_len: usize) -> Result<(), AppStateError> {
    if !base_len.is_multiple_of(2) {
        return Err(AppStateError::InvalidHashLength { expected: base_len + 1, actual: base_len });
    }
    if base_len != item_len {
        return Err(AppStateError::InvalidHashLength { expected: base_len, actual: item_len });
    }
    Ok(())
}

/// `performPointwiseWithOverflow` — lane-wise `u16` add/subtract on the
/// little-endian buffer, wrapping modulo 2^16 like Go's unsigned arithmetic.
fn perform_pointwise_with_overflow(base: &mut [u8], input: &[u8], op: LtHashOp) {
    for (base_lane, input_lane) in base.chunks_mut(2).zip(input.chunks(2)) {
        let x = u16::from_le_bytes([base_lane[0], base_lane[1]]);
        let y = u16::from_le_bytes([input_lane[0], input_lane[1]]);
        let result = match op {
            LtHashOp::Subtract => x.wrapping_sub(y),
            LtHashOp::Add => x.wrapping_add(y),
        };
        base_lane.copy_from_slice(&result.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lth() -> LTHash {
        WAPATCH_INTEGRITY
    }

    fn zero_base() -> Vec<u8> {
        vec![0u8; 128]
    }

    const ITEM1: &[u8] = b"item1";
    const ITEM2: &[u8] = b"item2";
    const ITEM3: &[u8] = b"item3";
    const ITEM4: &[u8] = b"item4";
    const X: &[u8] = b"x";

    #[test]
    fn go_vector_add_one_item() {
        // Go: WAPatchIntegrity.SubtractThenAdd(zeroBase, nil, [][]byte{[]byte("item1")})
        let out = lth().subtract_then_add(&zero_base(), &[], &[ITEM1]).expect("subtract_then_add");
        assert_eq!(
            hex::encode(&out),
            "a7757178af7929be52a927e0a4ff91e7a0a758a8f4838a4159a2a35b9ebc04588fed8709108e9424ffc763d351ad50c810ba2a602fe5ba1eeaa8a1734e320bb363c83e618dd98a9469ed078c763b6b4b6aedbca6fe5aa313eab3d3a255885f4831cfe94d8fe377ef47422ee6d18feab73dfe0eb9d8a668405a81486428132e58"
        );
    }

    #[test]
    fn go_vector_add_two_items() {
        // Go: WAPatchIntegrity.SubtractThenAdd(zeroBase, nil, [][]byte{"item1", "item2"})
        let out = lth().subtract_then_add(&zero_base(), &[], &[ITEM1, ITEM2]).expect("subtract_then_add");
        assert_eq!(
            hex::encode(&out),
            "eaaf4d3f0428ac63c0af9300dab5015dd93679431be1700ddf8772fe740695a88f6adcc1ead1a632b8fbab347d4e0ce0648a721f7b11defd681cf53e6505d0ae272ecb4a58e820ce325193f3588f75c11af797707a61256f245c8bdc66807d2624b5c14c370e3e387df606a6bbf36d5f98ac23ee503abffe7d2b406a6a712c5b"
        );
    }

    #[test]
    fn go_vector_subtract_one_add_two() {
        // Go: WAPatchIntegrity.SubtractThenAdd(zeroBase, [][]byte{"item1"}, [][]byte{"item3", "item4"})
        let out = lth().subtract_then_add(&zero_base(), &[ITEM1], &[ITEM3, ITEM4]).expect("subtract_then_add");
        assert_eq!(
            hex::encode(&out),
            "9e58844cff6a8243655890730422b11f3797dd2ffef7ce9e265a2350f3dedd70fc692a1c9be49af3bc03ada4e10ff92aa90a583cc118b1518a4e3aecbaea0daa620253d62631b614cf18499607845e72ec9da6a43ee5b0c210a59466b3d08af1c6c2980d103455f24a55caaecfbd8907e60d82926093401d49bb4e8cd027d418"
        );
    }

    #[test]
    fn go_vector_chained_mutations() {
        // Go: add {item1,item2}, then subtract {item1} + add {item3}
        let chained = lth().subtract_then_add(&zero_base(), &[], &[ITEM1, ITEM2]).expect("subtract_then_add");
        let chained = lth().subtract_then_add(&chained, &[ITEM1], &[ITEM3]).expect("subtract_then_add");
        assert_eq!(
            hex::encode(&chained),
            "415218674b09e3c1f0ca65c31e5adb539340da1c3683d3b631d688d837b9f75c24c2ac0e1d51f071281ca6b2a91d68c1f59c29f3ee810e5752cc054eddb91adf1a8a12d67dee888b248b7989579c9d6602b07e90856430c87ac0be66a575aff55e5a0106cee7d7ca017e8df051646ed98ed99136fdff81cf717667de15ef3516"
        );
    }

    #[test]
    fn go_vector_two_binary_value_macs() {
        // The app state path hashes 32-byte value MACs, not text.
        let mut v1 = vec![0u8; 32];
        let mut v2 = vec![0u8; 32];
        for i in 0..32 {
            v1[i] = (i + 1) as u8;
            v2[i] = 0xA0u8.wrapping_add(i as u8);
        }
        let out = lth().subtract_then_add(&zero_base(), &[], &[&v1, &v2]).expect("subtract_then_add");
        assert_eq!(
            hex::encode(&out),
            "55231159be432eed3e1bd73b941a31d601822093f5a28101e9539db75c95ad2a3b30bb5a98a56c1bf0b95f0b142e5c24a55590dc064bed03b84cf3efa31dd967ac287ce0f76998fe0505b6064329d51f9892b42d8587ec064da24eb67f5cb2d67a3b1a5848b21f5d3efc0445c7a7c8557ca3199ea4039da877dda3792b07b204"
        );
    }

    #[test]
    fn add_then_subtract_returns_to_the_original_digest() {
        let base = zero_base();
        let added = lth().subtract_then_add(&base, &[], &[ITEM1, ITEM2]).expect("add");
        let removed = lth().subtract_then_add(&added, &[ITEM1, ITEM2], &[]).expect("subtract");
        assert_eq!(removed, base);
    }

    #[test]
    fn perform_mutations_matches_subtract_then_add() {
        let base = zero_base();
        let expected = lth().subtract_then_add(&base, &[ITEM1], &[ITEM3, ITEM4]).expect("subtract_then_add");
        // Same multiset of operations, but interleaved: lane-wise addition
        // wraps modulo 2^16, so the order must not matter.
        let mut mutated = base.clone();
        lth()
            .perform_mutations(
                &mut mutated,
                &[(LtHashOp::Add, ITEM3), (LtHashOp::Subtract, ITEM1), (LtHashOp::Add, ITEM4)],
            )
            .expect("perform_mutations");
        assert_eq!(mutated, expected);
    }

    #[test]
    fn lane_arithmetic_wraps_modulo_2_to_the_16() {
        // A single 0xFFFF lane added twice wraps around to 0xFFFE.
        let mut base = vec![0u8; 2];
        let digest = lth().hash_item(ITEM1);
        perform_pointwise_with_overflow(&mut base, &digest, LtHashOp::Add);
        perform_pointwise_with_overflow(&mut base, &digest, LtHashOp::Add);
        let lane = u16::from_le_bytes([digest[0], digest[1]]);
        assert_eq!(u16::from_le_bytes([base[0], base[1]]), lane.wrapping_mul(2));
        perform_pointwise_with_overflow(&mut base, &digest, LtHashOp::Subtract);
        assert_eq!(u16::from_le_bytes([base[0], base[1]]), lane);
    }

    #[test]
    fn length_mismatches_are_errors_not_panics() {
        let mut odd = vec![0u8; 127];
        assert!(matches!(
            lth().subtract_then_add_in_place(&mut odd, &[], &[]),
            Err(AppStateError::InvalidHashLength { expected: 128, actual: 127 })
        ));
        let mut short = vec![0u8; 32];
        assert!(matches!(
            lth().subtract_then_add_in_place(&mut short, &[], &[ITEM1]),
            Err(AppStateError::InvalidHashLength { expected: 128, actual: 32 })
        ));
        let mut base = zero_base();
        assert!(lth().perform_mutations(&mut base, &[(LtHashOp::Add, ITEM1)]).is_ok());
    }

    #[test]
    fn custom_instance_matches_hkdf_parameters() {
        let small = LTHash::new(b"custom info", 16);
        assert_eq!(small.hash_item(X).len(), 16);
        let out = small.subtract_then_add(&[0u8; 16], &[], &[X]).expect("subtract_then_add");
        assert_eq!(out, small.hash_item(X));
        // The 128-byte instance must not use the custom info string.
        assert_ne!(lth().hash_item(X), small.hash_item(X));
    }
}
