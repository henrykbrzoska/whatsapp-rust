// Signal protocol implementation following the public X3DH / Double Ratchet /
// Sender Keys specifications — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The Signal protocol layer used by WhatsApp: X3DH session setup, the Double
//! Ratchet for pairwise messages and Sender Keys for groups.
//!
//! This is an **independent implementation of the public Signal
//! specifications** (X3DH, Double Ratchet, Sender Keys). Its wire and storage
//! formats follow the protobuf definitions published in `libsignal-protocol-c`
//! (`WhisperTextProtocol.proto`, `LocalStorageProtocol.proto`) so that it
//! interoperates with WhatsApp and reads/writes the Go client-compatible
//! `wa_sessions` / `wa_sender_keys` rows. No GPL-3.0 source was copied.
//!
//! * [`address`] — `SignalAddress` (`name:device`), `SenderKeyName`.
//! * [`ecc`] — X25519 agreement, XEdDSA, `0x05` key encoding.
//! * [`kdf`] — root / chain / message key derivation.
//! * [`ratchet`] — chains, skipped keys, X3DH initialisation.
//! * [`session`] — `SessionRecord`, `SessionBuilder`, `SessionCipher`.
//! * [`groups`] — `SenderKeyRecord`, `GroupSessionBuilder`, `GroupCipher`.
//! * [`store`] — the async `SignalStore` and its `Device` adapter.
//! * [`proto`] — hand-written `prost` mirrors of the libsignal protobufs.
//!
//! The layer never pads: WhatsApp padding is `crypto::keys::{pad_message,
//! unpad_message}` around it.

pub mod address;
pub mod ecc;
pub mod error;
pub mod groups;
pub mod kdf;
pub mod proto;
pub mod ratchet;
pub mod session;
pub mod store;

pub use address::{SenderKeyName, SignalAddress};
pub use error::SignalError;
pub use groups::{
    GroupCipher, GroupSessionBuilder, SenderKeyDistributionMessage, SenderKeyMessage, SenderKeyRecord, SenderKeyState,
};
pub use session::{
    CiphertextMessage, PreKeyBundle, PreKeySignalMessage, SessionBuilder, SessionCipher, SessionRecord, SessionState,
    SignalMessage,
};
pub use store::{DeviceStore, SessionlessSignalStore, SignalStore};
