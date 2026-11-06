// Signal protocol implementation following the public X3DH / Double Ratchet /
// Sender Keys specifications — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Errors of the Signal layer (semantics of libsignal's `signalerror` package).

use crate::crypto::CryptoError;

/// Every failure the Signal layer can report. Network bytes never panic; they
/// end up in one of these variants.
#[derive(Debug, thiserror::Error)]
pub enum SignalError {
    /// The remote identity key differs from the one we trust for that address.
    /// Go: `signalerror.ErrUntrustedIdentity`.
    #[error("untrusted identity for {0}")]
    UntrustedIdentity(String),
    /// A message counter / iteration older than the chain, with no cached key.
    /// Go: `signalerror.ErrOldCounter`.
    #[error("received message with old counter (current {current}, received {received})")]
    OldCounter { current: u32, received: u32 },
    /// A message whose key was already consumed (replay).
    /// Go: `signalerror.ErrDuplicateMessage`.
    #[error("duplicate message (counter {0})")]
    DuplicateMessage(u32),
    /// Malformed or unsupported message bytes.
    /// Go: `signalerror.ErrInvalidMessage` and friends.
    #[error("invalid message: {0}")]
    InvalidMessage(String),
    /// A malformed key (wrong type byte or length).
    /// Go: `signalerror.ErrInvalidKey` / `ErrBadKeyType`.
    #[error("invalid key: {0}")]
    InvalidKey(String),
    /// The message MAC or signature did not verify.
    /// Go: `signalerror.ErrBadMAC` / `ErrInvalidSignature`.
    #[error("bad MAC or signature")]
    BadMac,
    /// No session exists for the address (Go: `signalerror.ErrNoSessionForUser`).
    #[error("no session for {0}")]
    NoSession(String),
    /// No sender-key state for the group sender (Go: `signalerror.ErrNoSenderKeyForUser`).
    #[error("no sender key for {0}")]
    NoSenderKeyForUser(String),
    /// The signed pre-key referenced by a message is unknown (Go: `ErrNoSignedPreKey`).
    #[error("no signed pre-key with ID {0}")]
    NoSignedPreKey(u32),
    /// The one-time pre-key referenced by a message is unknown (Go: `ErrNoPreKey`).
    #[error("pre-key {0} not found")]
    PreKeyNotFound(u32),
    /// A primitive (AES-CBC, HKDF) failed.
    #[error("crypto error: {0}")]
    Crypto(#[from] CryptoError),
    /// Store passthrough (SQLite and friends).
    #[error("store error: {0}")]
    Store(#[from] anyhow::Error),
}

impl SignalError {
    /// Go: `errors.Is(err, signalerror.ErrUntrustedIdentity)`.
    pub fn is_untrusted_identity(&self) -> bool {
        matches!(self, Self::UntrustedIdentity(_))
    }

    /// Go: `errors.Is(err, signalerror.ErrOldCounter)` — replays count too,
    /// because both mean "the key for this counter is gone".
    pub fn is_old_counter(&self) -> bool {
        matches!(self, Self::OldCounter { .. } | Self::DuplicateMessage(_))
    }

    /// Go: `errors.Is(err, signalerror.ErrNoSenderKeyForUser)`.
    pub fn is_no_sender_key_for_user(&self) -> bool {
        matches!(self, Self::NoSenderKeyForUser(_))
    }

    pub(crate) fn invalid(msg: impl Into<String>) -> Self {
        Self::InvalidMessage(msg.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers_classify_errors() {
        assert!(SignalError::UntrustedIdentity("a:1".into()).is_untrusted_identity());
        assert!(SignalError::OldCounter { current: 3, received: 1 }.is_old_counter());
        assert!(SignalError::DuplicateMessage(1).is_old_counter());
        assert!(SignalError::NoSenderKeyForUser("g".into()).is_no_sender_key_for_user());
        assert!(!SignalError::BadMac.is_old_counter());
        assert_eq!(SignalError::PreKeyNotFound(7).to_string(), "pre-key 7 not found");
        let store: SignalError = anyhow::anyhow!("db locked").into();
        assert!(store.to_string().contains("db locked"));
    }
}
