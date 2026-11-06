// Ported from the Go client `socket/noisesocket.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The encrypted frame layer on top of [`FrameSocket`] (`socket.NoiseSocket`).
//!
//! Every outgoing frame is sealed with the write key and the current write
//! counter as nonce; every incoming frame is opened with the read key and the
//! current read counter. No associated data is used at this layer (the
//! handshake already bound the transcript), matching Go.
//!
//! Wire compatibility detail: Go's `SendFrame` seals with
//! `generateIV(writeCounter)` and increments *afterwards*, i.e. frame `n` uses
//! counter `n`. The same holds for the read side.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU32, Ordering},
};

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};

use super::frame_socket::FrameSocket;
use super::noise_handshake::{IV_SIZE, KEY_SIZE, NoiseError, generate_iv};

/// `socket.FrameHandler` — called with every decrypted frame.
pub type FrameHandler = Box<dyn Fn(Vec<u8>) + Send + Sync + 'static>;
/// `socket.DisconnectHandler` — `remote == true` when the peer went away.
pub type DisconnectHandler = Box<dyn Fn(bool) + Send + Sync + 'static>;

fn prepare(key: &[u8; KEY_SIZE]) -> Result<Aes256Gcm, NoiseError> {
    Aes256Gcm::new_from_slice(key).map_err(|err| NoiseError::InvalidKey(err.to_string()))
}

/// Port of the Go `socket.NoiseSocket`.
/// The parts of the socket the read task and the API share.
struct NoiseState {
    write_cipher: Aes256Gcm,
    read_cipher: Aes256Gcm,
    write_counter: AtomicU32,
    read_counter: AtomicU32,
}

impl NoiseState {
    /// `receiveEncryptedFrame` — decrypt with the current read counter, then advance it.
    fn decrypt_frame(&self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let counter = self.read_counter.fetch_add(1, Ordering::SeqCst);
        self.read_cipher
            .decrypt(Nonce::from_slice(&generate_iv(counter)), Payload { msg: ciphertext, aad: &[] })
            .map_err(|_| NoiseError::Decrypt)
    }
}

/// Port of the Go `socket.NoiseSocket`.
pub struct NoiseSocket {
    frames: Arc<FrameSocket>,
    state: Arc<NoiseState>,
    destroyed: AtomicBool,
    consumer: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl NoiseSocket {
    /// `newNoiseSocket` — installs the disconnect handler on the frame socket and
    /// starts the frame consumer with `on_frame`.
    pub async fn new(
        frames: Arc<FrameSocket>,
        write_key: &[u8; KEY_SIZE],
        read_key: &[u8; KEY_SIZE],
        on_frame: FrameHandler,
        on_disconnect: Option<DisconnectHandler>,
    ) -> Result<Self, NoiseError> {
        let state = Arc::new(NoiseState {
            write_cipher: prepare(write_key)?,
            read_cipher: prepare(read_key)?,
            write_counter: AtomicU32::new(0),
            read_counter: AtomicU32::new(0),
        });
        if let Some(handler) = on_disconnect {
            frames.set_on_disconnect(handler).map_err(|err| NoiseError::Transport(err.to_string()))?;
        }
        let consumer_frames = Arc::clone(&frames);
        let consumer_state = Arc::clone(&state);
        let task = tokio::spawn(async move {
            // Go: consumeFrames — decrypt each frame and hand it to on_frame.
            while let Some(ciphertext) = consumer_frames.recv().await {
                match consumer_state.decrypt_frame(&ciphertext) {
                    Ok(plaintext) => on_frame(plaintext),
                    // Go: "Failed to decrypt frame" and keeps reading.
                    Err(err) => tracing::warn!("failed to decrypt frame: {err}"),
                }
            }
        });
        Ok(Self { frames, state, destroyed: AtomicBool::new(false), consumer: Mutex::new(Some(task)) })
    }

    /// `SendFrame` — encrypt with the current write counter and send.
    pub async fn send_frame(&self, plaintext: &[u8]) -> Result<(), NoiseError> {
        if self.destroyed.load(Ordering::SeqCst) {
            return Err(NoiseError::Closed);
        }
        let counter = self.state.write_counter.fetch_add(1, Ordering::SeqCst);
        let nonce = generate_iv(counter);
        let ciphertext = self
            .state
            .write_cipher
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plaintext, aad: &[] })
            .map_err(|_| NoiseError::Decrypt)?;
        self.frames.send_frame(ciphertext).await.map_err(|err| NoiseError::Transport(err.to_string()))
    }

    /// `IsConnected` — delegates to the frame socket.
    pub fn is_connected(&self) -> bool {
        self.frames.is_connected()
    }

    /// `Stop(disconnect, allowOnDisconnect)` — idempotent teardown.
    ///
    /// With `disconnect` the WebSocket is closed gracefully; unless
    /// `allow_on_disconnect` is set the disconnect callback is removed first, so
    /// an expected shutdown does not look like a remote disconnect.
    pub async fn stop(&self, disconnect: bool, allow_on_disconnect: bool) {
        if self.destroyed.swap(true, Ordering::SeqCst) {
            return;
        }
        if !allow_on_disconnect {
            self.frames.clear_on_disconnect();
        }
        if let Some(task) = self.consumer.lock().expect("consumer mutex").take() {
            task.abort();
        }
        if disconnect {
            self.frames.close(1000).await;
        }
    }

    /// The number of frames encrypted so far (next write nonce).
    pub fn write_counter(&self) -> u32 {
        self.state.write_counter.load(Ordering::SeqCst)
    }

    /// The number of frames decrypted so far (next read nonce).
    pub fn read_counter(&self) -> u32 {
        self.state.read_counter.load(Ordering::SeqCst)
    }

    /// Nonce used for the `count`-th frame, exposed for tests/diagnostics.
    pub fn nonce_for(count: u32) -> [u8; IV_SIZE] {
        generate_iv(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::socket::frame_socket::FrameSocket;
    use futures::{SinkExt, StreamExt};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;
    use tokio_tungstenite::tungstenite::Message;

    const WRITE_KEY: [u8; 32] = [1u8; 32];
    const READ_KEY: [u8; 32] = [2u8; 32];
    /// The server side uses the client's keys the other way around.
    const SERVER_WRITE_KEY: [u8; 32] = READ_KEY;
    const SERVER_READ_KEY: [u8; 32] = WRITE_KEY;

    fn seal(key: &[u8; 32], counter: u32, plaintext: &[u8]) -> Vec<u8> {
        let cipher = Aes256Gcm::new_from_slice(key).unwrap();
        cipher.encrypt(Nonce::from_slice(&generate_iv(counter)), Payload { msg: plaintext, aad: &[] }).unwrap()
    }

    fn open(key: &[u8; 32], counter: u32, ciphertext: &[u8]) -> Vec<u8> {
        let cipher = Aes256Gcm::new_from_slice(key).unwrap();
        cipher.decrypt(Nonce::from_slice(&generate_iv(counter)), Payload { msg: ciphertext, aad: &[] }).unwrap()
    }

    #[tokio::test]
    async fn encrypts_outgoing_and_decrypts_incoming_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            // The FrameSocket prefixes the WA header once, then 3-byte lengths.
            let mut seen: Vec<Vec<u8>> = Vec::new();
            while let Some(Ok(message)) = ws.next().await {
                if let Message::Binary(bytes) = message {
                    // strip WA header + 3-byte length
                    let (_, payload) = split_frame(&bytes);
                    seen.push(payload);
                    if seen.len() == 2 {
                        break;
                    }
                }
            }
            // Reply with two frames of its own (counters 0 and 1).
            let mut reply = length_prefixed(&seal(&SERVER_WRITE_KEY, 0, b"server-1"));
            reply.extend_from_slice(&length_prefixed(&seal(&SERVER_WRITE_KEY, 1, b"server-2")));
            ws.send(Message::Binary(reply)).await.unwrap();
            // Give the client a moment to consume both replies before the
            // connection is dropped by the end of this task.
            tokio::time::sleep(Duration::from_millis(200)).await;
            seen
        });

        let frames = Arc::new(FrameSocket::connect(Some(&format!("ws://{addr}"))).await.expect("connect"));
        let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&received);
        let socket = NoiseSocket::new(
            Arc::clone(&frames),
            &WRITE_KEY,
            &READ_KEY,
            Box::new(move |plaintext| sink.lock().unwrap().push(plaintext)),
            None,
        )
        .await
        .expect("noise socket");

        socket.send_frame(b"client-1").await.unwrap();
        socket.send_frame(b"client-2").await.unwrap();

        let seen =
            tokio::time::timeout(Duration::from_secs(5), server).await.expect("server timed out").expect("server task");
        assert_eq!(seen.len(), 2);
        // Counter 0 then 1, sealed with the write key.
        assert_eq!(open(&SERVER_READ_KEY, 0, &seen[0]), b"client-1");
        assert_eq!(open(&SERVER_READ_KEY, 1, &seen[1]), b"client-2");

        // And the client decrypts the server's frames in order.
        for _ in 0..50 {
            if received.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let decrypted = received.lock().unwrap().clone();
        assert_eq!(decrypted, vec![b"server-1".to_vec(), b"server-2".to_vec()]);
        assert_eq!(socket.write_counter(), 2);
        assert_eq!(socket.read_counter(), 2);
    }

    fn split_frame(message: &[u8]) -> (Vec<u8>, Vec<u8>) {
        // WA header (4 bytes) may prefix the very first message.
        let body = if message.starts_with(b"WA") { &message[4..] } else { message };
        let length = (usize::from(body[0]) << 16) | (usize::from(body[1]) << 8) | usize::from(body[2]);
        (body[..3].to_vec(), body[3..3 + length].to_vec())
    }

    fn length_prefixed(payload: &[u8]) -> Vec<u8> {
        let mut out = vec![
            ((payload.len() >> 16) & 0xFF) as u8,
            ((payload.len() >> 8) & 0xFF) as u8,
            (payload.len() & 0xFF) as u8,
        ];
        out.extend_from_slice(payload);
        out
    }

    #[tokio::test]
    async fn stop_is_idempotent_and_fires_the_disconnect_handler() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(_)) = ws.next().await {}
        });
        let frames = Arc::new(FrameSocket::connect(Some(&format!("ws://{addr}"))).await.expect("connect"));
        let hits = Arc::new(Mutex::new(Vec::new()));
        let sinks = Arc::clone(&hits);
        let socket = NoiseSocket::new(
            Arc::clone(&frames),
            &WRITE_KEY,
            &READ_KEY,
            Box::new(|_| {}),
            Some(Box::new(move |remote| sinks.lock().unwrap().push(remote))),
        )
        .await
        .expect("noise socket");

        // allow_on_disconnect = true → the handler observes the local close.
        socket.stop(true, true).await;
        socket.stop(true, true).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(hits.lock().unwrap().len(), 1, "stop() is idempotent");

        assert!(
            matches!(socket.send_frame(b"after stop").await, Err(NoiseError::Closed)),
            "sending after stop() fails"
        );
    }

    #[tokio::test]
    async fn expected_close_does_not_report_a_disconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(_)) = ws.next().await {}
        });
        let frames = Arc::new(FrameSocket::connect(Some(&format!("ws://{addr}"))).await.expect("connect"));
        let hits = Arc::new(Mutex::new(Vec::new()));
        let sinks = Arc::clone(&hits);
        let socket = NoiseSocket::new(
            Arc::clone(&frames),
            &WRITE_KEY,
            &READ_KEY,
            Box::new(|_| {}),
            Some(Box::new(move |remote| sinks.lock().unwrap().push(remote))),
        )
        .await
        .expect("noise socket");

        // allow_on_disconnect = false → handler removed before the close.
        socket.stop(true, false).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(hits.lock().unwrap().is_empty(), "an expected shutdown must not report a remote disconnect");
    }

    #[test]
    fn nonce_matches_go_generate_iv() {
        assert_eq!(hex::encode(NoiseSocket::nonce_for(0)), "000000000000000000000000");
        assert_eq!(hex::encode(NoiseSocket::nonce_for(258)), "000000000000000000000102");
    }
}
