// Ported from the Go client `socket/`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! WebSocket transport for WhatsApp binary frames.
//!
//! Layering, bottom up:
//!
//! 1. [`frame_socket::FrameSocket`] — the WebSocket itself: WA connection header,
//!    3-byte length framing, frame reassembly. Byte-transparent for the binary
//!    protocol: [`frame_socket::FrameSocket::send_frame`] takes what
//!    `binary::marshal` produced and [`frame_socket::FrameSocket::recv`] yields
//!    what `binary::unpack` expects.
//! 2. [`noise_handshake::NoiseHandshake`] — the handshake state machine
//!    (`Noise_XX_25519_AESGCM_SHA256`, WhatsApp variant).
//! 3. [`noise_socket::NoiseSocket`] — the encrypted frame layer: every frame is
//!    sealed with the negotiated keys and a counter nonce.

pub mod frame_socket;
pub mod noise_handshake;
pub mod noise_socket;

pub use frame_socket::{FRAME_LENGTH_SIZE, FRAME_MAX_SIZE, FrameSocket, FrameSocketError};
pub use noise_handshake::{NOISE_START_PATTERN, NoiseError, NoiseHandshake, generate_iv};
pub use noise_socket::{DisconnectHandler, FrameHandler, NoiseSocket};
