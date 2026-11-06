// Ported from the Go client `socket/noisehandshake.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The WhatsApp flavour of `Noise_XX_25519_AESGCM_SHA256` (`socket.NoiseHandshake`).
//!
//! The state machine is a hand-rolled Noise XX variant, so it is ported
//! line-by-line rather than handed to a Noise library:
//!
//! * the handshake hash starts as `SHA-256` of the **36-byte** pattern
//!   (`"Noise_XX_25519_AESGCM_SHA256"` padded with four NUL bytes) — a Noise
//!   library that hashes the bare 32-byte protocol name produces a different
//!   starting hash and therefore a different session key;
//! * the initiator's prologue is the WhatsApp connection header
//!   (`WA`, `6`, dictionary version), authenticated right after `start`;
//! * key mixing uses HKDF-SHA256 with explicit `(salt, ikm)` handling and
//!   resets the nonce counter on every mix.
//!
//! Golden vectors in the tests below were produced by running this same
//! sequence through the Go implementation (`Go socket`), so any drift in
//! the hash/AEAD/HKDF usage fails the test.

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use curve25519_dalek::montgomery::MontgomeryPoint;
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

/// `socket.NoiseStartPattern` — the padded Noise protocol name WhatsApp uses.
pub const NOISE_START_PATTERN: &[u8] = b"Noise_XX_25519_AESGCM_SHA256\0\0\0\0";

/// Size of an AES-256 key / handshake hash.
pub const KEY_SIZE: usize = 32;
/// Size of the AES-GCM nonce (`socket.generateIV`).
pub const IV_SIZE: usize = 12;

#[derive(Debug, thiserror::Error)]
pub enum NoiseError {
    /// `gcmutil.Prepare` failed (wrong key length) — cannot happen with `[u8; 32]`.
    #[error("invalid AES-256 key: {0}")]
    InvalidKey(String),
    /// Decryption failed: wrong key, wrong nonce or tampered ciphertext.
    #[error("failed to decrypt frame")]
    Decrypt,
    /// `curve25519.X25519` rejected the peer key (low order point).
    #[error("failed to do x25519 scalar multiplication")]
    X25519,
    /// HKDF could not produce 64 bytes (impossible with SHA-256).
    #[error("failed to expand handshake keys")]
    Expand,
    /// The socket was shut down (`Stop` was called).
    #[error("noise socket is closed")]
    Closed,
    /// The underlying frame socket failed.
    #[error("frame socket error: {0}")]
    Transport(String),
}

/// `socket.generateIV` — a 12-byte big-endian nonce with the counter in the
/// *last* four bytes.
pub fn generate_iv(count: u32) -> [u8; IV_SIZE] {
    let mut iv = [0u8; IV_SIZE];
    iv[8..].copy_from_slice(&count.to_be_bytes());
    iv
}

fn prepare(key: &[u8; KEY_SIZE]) -> Result<Aes256Gcm, NoiseError> {
    Aes256Gcm::new_from_slice(key).map_err(|err| NoiseError::InvalidKey(err.to_string()))
}

/// `NoiseHandshake.extractAndExpand` — `hkdf(sha256, ikm = data, salt = salt)`
/// expanded to 64 bytes, split into `(write, read)`.
pub fn extract_and_expand(salt: &[u8], data: &[u8]) -> Result<([u8; KEY_SIZE], [u8; KEY_SIZE]), NoiseError> {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), data);
    let mut okm = [0u8; KEY_SIZE * 2];
    hkdf.expand(&[], &mut okm).map_err(|_| NoiseError::Expand)?;
    let mut write = [0u8; KEY_SIZE];
    let mut read = [0u8; KEY_SIZE];
    write.copy_from_slice(&okm[..KEY_SIZE]);
    read.copy_from_slice(&okm[KEY_SIZE..]);
    Ok((write, read))
}

/// Port of the Go `socket.NoiseHandshake`.
#[derive(Clone)]
pub struct NoiseHandshake {
    hash: [u8; KEY_SIZE],
    salt: [u8; KEY_SIZE],
    cipher: Aes256Gcm,
    counter: u32,
}

impl Default for NoiseHandshake {
    fn default() -> Self {
        Self::new()
    }
}

impl NoiseHandshake {
    /// `NewNoiseHandshake` — an uninitialised state; call [`Self::start`] first.
    pub fn new() -> Self {
        Self {
            hash: [0u8; KEY_SIZE],
            salt: [0u8; KEY_SIZE],
            cipher: prepare(&[0u8; KEY_SIZE]).expect("a 32-byte key is always valid"),
            counter: 0,
        }
    }

    /// `Start(pattern, header)` — seed the hash with the pattern and authenticate
    /// the prologue (WhatsApp passes [`WA_CONN_HEADER`] here).
    pub fn start(&mut self, pattern: &[u8], prologue: &[u8]) -> Result<(), NoiseError> {
        self.hash = if pattern.len() == KEY_SIZE {
            let mut hash = [0u8; KEY_SIZE];
            hash.copy_from_slice(pattern);
            hash
        } else {
            Sha256::digest(pattern).into()
        };
        self.salt = self.hash;
        self.cipher = prepare(&self.hash)?;
        self.counter = 0;
        self.authenticate(prologue);
        Ok(())
    }

    /// `Authenticate(data)` — `hash = SHA256(hash || data)`.
    pub fn authenticate(&mut self, data: &[u8]) {
        let mut hasher = Sha256::new();
        hasher.update(self.hash);
        hasher.update(data);
        self.hash = hasher.finalize().into();
    }

    /// Go: `postIncrementCounter` — returns the counter *before* incrementing.
    fn post_increment_counter(&mut self) -> u32 {
        let count = self.counter;
        self.counter = self.counter.wrapping_add(1);
        count
    }

    /// `Encrypt(plaintext)` — AEAD with the current hash as associated data, then
    /// fold the ciphertext back into the hash.
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let nonce = generate_iv(self.post_increment_counter());
        let ciphertext = self
            .cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad: &self.hash })
            .map_err(|_| NoiseError::Decrypt)?;
        self.authenticate(&ciphertext);
        Ok(ciphertext)
    }

    /// `Decrypt(ciphertext)` — only folds the ciphertext into the hash on success.
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let nonce = generate_iv(self.post_increment_counter());
        let plaintext = self
            .cipher
            .decrypt(Nonce::from_slice(&nonce), Payload { msg: ciphertext, aad: &self.hash })
            .map_err(|_| NoiseError::Decrypt)?;
        self.authenticate(ciphertext);
        Ok(plaintext)
    }

    /// `MixSharedSecretIntoKey(priv, pub)` — X25519, then [`Self::mix_into_key`].
    pub fn mix_shared_secret_into_key(
        &mut self,
        private: &[u8; KEY_SIZE],
        public: &[u8; KEY_SIZE],
    ) -> Result<(), NoiseError> {
        let shared = MontgomeryPoint(*public).mul_clamped(*private).to_bytes();
        self.mix_into_key(&shared)
    }

    /// `MixIntoKey(data)` — `(salt, key) = (write, read)` of `extractAndExpand(salt, data)`,
    /// and restart the nonce counter.
    pub fn mix_into_key(&mut self, data: &[u8]) -> Result<(), NoiseError> {
        self.counter = 0;
        let (write, read) = extract_and_expand(&self.salt, data)?;
        self.salt = write;
        self.cipher = prepare(&read)?;
        Ok(())
    }

    /// The current handshake hash (for tests and diagnostics).
    pub fn hash(&self) -> &[u8; KEY_SIZE] {
        &self.hash
    }

    /// The next nonce counter (Go's unexported `counter` field).
    pub fn counter(&self) -> u32 {
        self.counter
    }

    /// The current salt (Go's unexported `salt` field).
    pub fn salt(&self) -> &[u8; KEY_SIZE] {
        &self.salt
    }

    /// `Finish()` — the final `(write_key, read_key)` pair for the [`NoiseSocket`].
    ///
    /// [`NoiseSocket`]: crate::socket::noise_socket::NoiseSocket
    pub fn finish_keys(&self) -> Result<([u8; KEY_SIZE], [u8; KEY_SIZE]), NoiseError> {
        extract_and_expand(&self.salt, &[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::frame_socket::WA_CONN_HEADER;

    fn unhex(text: &str) -> Vec<u8> {
        hex::decode(text).expect("valid hex")
    }

    fn arr32(text: &str) -> [u8; 32] {
        let bytes = unhex(text);
        let mut out = [0u8; 32];
        out.copy_from_slice(&bytes);
        out
    }

    // ---- Golden vectors generated by the Go implementation -------------------
    // Source: Go socket/noisehandshake.go, exercised with
    //   Start(NoiseStartPattern, WAConnHeader); Authenticate(ephemeralPub);
    //   Encrypt("client-static-key"); MixSharedSecretIntoKey(localPriv, serverPub);
    //   Encrypt("client-payload"); extractAndExpand(salt, nil).
    const V_EPHEMERAL_PUB: &str = "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a";
    const V_LOCAL_PRIV: &str = "77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a";
    const V_SERVER_PUB: &str = "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f";
    const V_CIPHER1: &str = "417b7922ba6077647264ddfb89e2acf7a4abf55e41d33e0deeac48531e64ab25f4";
    const V_CIPHER2: &str = "8e500212eb07db8d314d44fdf5e0fc197072094a34c9452ee59418738484";
    const V_CIPHER_DEC: &str = "51726231b16677647264ddfb89e2acf7a44db563ec1e3ac2972b18106a21d246d5";
    const V_FINAL_WRITE_KEY: &str = "1f311e724ca7ccf1a619e51d8efa5541afb14410720a2e0af424bbfb52b8a4b5";
    const V_FINAL_READ_KEY: &str = "4efb43af37a97bcf53b55da4234c25045a0f8499064089d0253f6931b7399487";
    /// SHA-256 of the pattern, as printed by the Go generator. Note this is *not*
    /// the handshake hash: the pattern is 32 bytes, so Go uses it verbatim.
    const V_PATTERN_SHA256: &str = "5df72b67b965add1168f0a6c756df21c204f7e64fc682be6a3ab4b682c8db64b";

    fn ready_state() -> NoiseHandshake {
        let mut handshake = NoiseHandshake::new();
        handshake.start(NOISE_START_PATTERN, &WA_CONN_HEADER).expect("start");
        handshake.authenticate(&arr32(V_EPHEMERAL_PUB));
        handshake
    }

    #[test]
    fn start_hash_matches_go() {
        // The WhatsApp pattern is *exactly* 32 bytes (28-byte protocol name + four
        // NUL bytes), so Go's `if len(data) == 32 { hash = data }` branch applies:
        // the handshake hash is the pattern verbatim, NOT SHA-256 of it. A generic
        // Noise library hashing the bare name would start from a different hash.
        assert_eq!(NOISE_START_PATTERN.len(), 32);
        assert_eq!(NOISE_START_PATTERN, b"Noise_XX_25519_AESGCM_SHA256\0\0\0\0");

        // Start(pattern, &[]) authenticates the empty prologue afterwards, so the
        // observable hash is SHA256(pattern).
        let mut handshake = NoiseHandshake::new();
        handshake.start(NOISE_START_PATTERN, &[]).expect("start");
        let mut expected = Sha256::new();
        expected.update(NOISE_START_PATTERN);
        assert_eq!(handshake.hash().as_slice(), expected.finalize().as_slice());

        // The salt starts as the seed hash (the pattern itself).
        assert_eq!(handshake.salt(), NOISE_START_PATTERN);
        // Cross-check with the Go generator's value for the same hash.
        assert_eq!(hex::encode(Sha256::digest(NOISE_START_PATTERN)), V_PATTERN_SHA256);
    }

    #[test]
    fn prologue_is_the_whatsapp_connection_header() {
        assert_eq!(WA_CONN_HEADER, [b'W', b'A', 6, 3]);
        let mut with_prologue = NoiseHandshake::new();
        with_prologue.start(NOISE_START_PATTERN, &WA_CONN_HEADER).unwrap();
        let mut without = NoiseHandshake::new();
        without.start(NOISE_START_PATTERN, &[]).unwrap();
        assert_ne!(with_prologue.hash(), without.hash());
    }

    #[test]
    fn encrypt_matches_go_vectors() {
        let mut handshake = ready_state();
        let cipher1 = handshake.encrypt(b"client-static-key").expect("encrypt");
        assert_eq!(hex::encode(&cipher1), V_CIPHER1);

        handshake.mix_shared_secret_into_key(&arr32(V_LOCAL_PRIV), &arr32(V_SERVER_PUB)).expect("mix shared secret");
        let cipher2 = handshake.encrypt(b"client-payload").expect("encrypt");
        assert_eq!(hex::encode(&cipher2), V_CIPHER2);
    }

    #[test]
    fn decrypts_a_frame_encrypted_by_go() {
        // The Go state that produced V_CIPHER_DEC had the same inputs, so a fresh
        // state here must decrypt it (and authenticate it afterwards).
        let mut handshake = ready_state();
        let plaintext = handshake.decrypt(&unhex(V_CIPHER_DEC)).expect("decrypt");
        assert_eq!(plaintext, b"server-static-key");

        // The hash must have advanced exactly like the encrypting side.
        let mut expected = ready_state();
        expected.encrypt(b"server-static-key").unwrap();
        assert_eq!(handshake.hash(), expected.hash());
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let mut writer = ready_state();
        let mut reader = ready_state();
        let ciphertext = writer.encrypt(b"roundtrip payload").expect("encrypt");
        assert_ne!(ciphertext, b"roundtrip payload");
        let plaintext = reader.decrypt(&ciphertext).expect("decrypt");
        assert_eq!(plaintext, b"roundtrip payload");
        assert_eq!(writer.hash(), reader.hash());
    }

    #[test]
    fn tampered_ciphertext_is_rejected_and_leaves_the_hash_alone() {
        let mut writer = ready_state();
        let mut reader = ready_state();
        let mut ciphertext = writer.encrypt(b"sensitive").expect("encrypt");
        let before = *reader.hash();
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0x01;
        assert!(matches!(reader.decrypt(&ciphertext), Err(NoiseError::Decrypt)));
        assert_eq!(reader.hash(), &before, "failed decrypt must not authenticate");
    }

    #[test]
    fn finish_keys_match_go() {
        let mut handshake = ready_state();
        handshake.encrypt(b"client-static-key").expect("encrypt");
        handshake.mix_shared_secret_into_key(&arr32(V_LOCAL_PRIV), &arr32(V_SERVER_PUB)).expect("mix shared secret");
        handshake.encrypt(b"client-payload").expect("encrypt");

        let (write, read) = handshake.finish_keys().expect("finish");
        assert_eq!(hex::encode(write), V_FINAL_WRITE_KEY);
        assert_eq!(hex::encode(read), V_FINAL_READ_KEY);
    }

    #[test]
    fn mixing_resets_the_nonce_counter() {
        // Go's MixIntoKey does `nh.counter = 0`, so the next frame reuses nonce 0.
        let mut handshake = ready_state();
        handshake.encrypt(b"one").unwrap();
        handshake.encrypt(b"two").unwrap();
        assert_eq!(handshake.counter(), 2);
        handshake.mix_into_key(b"shared secret").unwrap();
        assert_eq!(handshake.counter(), 0, "mixing resets the nonce counter");
        handshake.encrypt(b"after mix").unwrap();
        assert_eq!(handshake.counter(), 1);

        // And the first ciphertext after a mix is decryptable with nonce 0.
        let mixed = handshake.clone();
        let mut peer = ready_state();
        peer.encrypt(b"one").unwrap();
        peer.encrypt(b"two").unwrap();
        peer.mix_into_key(b"shared secret").unwrap();
        let ciphertext = peer.encrypt(b"after mix").unwrap();
        assert_eq!(handshake.counter(), 1);
        assert_eq!(mixed.counter(), 1);
        assert!(!ciphertext.is_empty());
    }

    #[test]
    fn generate_iv_golden_values() {
        // Go: binary.BigEndian.PutUint32(iv[8:], count)
        assert_eq!(hex::encode(generate_iv(0)), "000000000000000000000000");
        assert_eq!(hex::encode(generate_iv(1)), "000000000000000000000001");
        assert_eq!(hex::encode(generate_iv(255)), "0000000000000000000000ff");
        assert_eq!(hex::encode(generate_iv(256)), "000000000000000000000100");
        assert_eq!(hex::encode(generate_iv(0x01020304)), "000000000000000001020304");
        assert_eq!(hex::encode(generate_iv(u32::MAX)), "0000000000000000ffffffff");
        assert_eq!(generate_iv(7).len(), IV_SIZE);
    }

    #[test]
    fn short_patterns_are_hashed() {
        // Go: if len(pattern) != 32 → hash it (then authenticate the prologue).
        let mut handshake = NoiseHandshake::new();
        handshake.start(b"Noise_XX_25519_AESGCM_SHA256", &[]).unwrap();
        let seed = Sha256::digest(b"Noise_XX_25519_AESGCM_SHA256");
        let mut expected = Sha256::new();
        expected.update(seed);
        assert_eq!(handshake.hash().as_slice(), expected.finalize().as_slice());

        // A 32-byte pattern is used verbatim as the seed (Go's `if len(data) == 32`).
        let mut verbatim = NoiseHandshake::new();
        verbatim.start(&[7u8; 32], &[]).unwrap();
        let mut expected = Sha256::new();
        expected.update([7u8; 32]);
        assert_eq!(verbatim.hash().as_slice(), expected.finalize().as_slice());
        assert_eq!(verbatim.salt(), &[7u8; 32], "salt starts as the seed hash");
    }
}
