// Ported from the Go client `socket/framesocket.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! WebSocket transport for complete WhatsApp binary frames.
//!
//! [`FrameSocket`] is byte-transparent with respect to the WhatsApp binary
//! protocol: [`FrameSocket::send_frame`] takes the bytes produced by
//! `binary::marshal` (which already include the data-type byte) and
//! [`FrameSocket::recv`] yields exactly the bytes `binary::unpack` expects.
//! No (un)packing happens here.
//!
//! What this layer *does* own is Go's wire framing:
//! * the one-shot WhatsApp connection header (`WA`, magic `6`, `token::DICT_VERSION`)
//!   prefixed to the very first outgoing message,
//! * the 3-byte big-endian length prefix on every frame,
//! * reassembly of frames that are split across (or packed into) WebSocket messages.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};

use futures::{SinkExt, StreamExt, stream::SplitSink};
use tokio::sync::mpsc;
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{
        Message,
        client::IntoClientRequest,
        http::HeaderValue,
        protocol::{CloseFrame, WebSocketConfig},
    },
};

/// `socket.FrameMaxSize` — the largest frame WhatsApp accepts.
pub const FRAME_MAX_SIZE: usize = 1 << 24;
/// `socket.FrameLengthSize` — width of the big-endian length prefix.
pub const FRAME_LENGTH_SIZE: usize = 3;
/// `socket.URL` — the multi-device WebSocket endpoint.
pub const URL: &str = "wss://web.whatsapp.com/ws/chat";
/// `socket.Origin` — the `Origin` header of every WhatsApp WebSocket.
pub const ORIGIN: &str = "https://web.whatsapp.com";

/// `socket.WAConnHeader` — `WA`, [`WA_MAGIC_VALUE`], [`crate::binary::token::DICT_VERSION`].
pub const WA_CONN_HEADER: [u8; 4] = [b'W', b'A', WA_MAGIC_VALUE, crate::binary::token::DICT_VERSION];
/// `socket.WA_MAGIC_VALUE`.
pub const WA_MAGIC_VALUE: u8 = 6;

/// Size of the channel between the read task and [`FrameSocket::recv`].
const CHANNEL_SIZE: usize = 2048;

type Ws = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type Callback = Box<dyn Fn(bool) + Send + Sync + 'static>;

#[derive(Debug, thiserror::Error)]
pub enum FrameSocketError {
    /// Go: `ErrDialFailed` (the inner error carries tungstenite's details).
    #[error("websocket connection failed: {0}")]
    Dial(Box<tokio_tungstenite::tungstenite::Error>),
    /// Go: `ErrSocketClosed`.
    #[error("frame socket is closed")]
    Closed,
    /// Go: `ErrSocketAlreadyOpen`.
    #[error("frame socket is already open")]
    AlreadyOpen,
    /// Go: `ErrFrameTooLarge`.
    #[error("frame too large (got {got} bytes, max {FRAME_MAX_SIZE} bytes)")]
    FrameTooLarge { got: usize },
    /// The disconnect callback mutex was poisoned by a panicking callback.
    #[error("disconnect callback lock poisoned")]
    CallbackPoisoned,
}

impl From<tokio_tungstenite::tungstenite::Error> for FrameSocketError {
    fn from(err: tokio_tungstenite::tungstenite::Error) -> Self {
        // Boxed so the error enum stays small (clippy::result_large_err).
        Self::Dial(Box::new(err))
    }
}

/// Port of the Go `socket.FrameSocket`.
pub struct FrameSocket {
    sink: Arc<tokio::sync::Mutex<Option<SplitSink<Ws, Message>>>>,
    frames: Arc<tokio::sync::Mutex<mpsc::Receiver<Vec<u8>>>>,
    callback: Arc<Mutex<Option<Callback>>>,
    /// Go's `closed` atomic flag: set once the socket is going down.
    closed: Arc<AtomicBool>,
    /// Go's `Header` field: `true` while the WA connection header still has to be sent.
    header_pending: Arc<AtomicBool>,
    read_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl FrameSocket {
    /// Connect to `url` (default [`URL`]) and start the read task.
    ///
    /// Mirrors `NewFrameSocket` + `Connect`: the `Origin`, `Cache-Control` and
    /// `Pragma` headers are the ones the Go client sets on every connection.
    pub async fn connect(url: Option<&str>) -> Result<Self, FrameSocketError> {
        let mut request = url.unwrap_or(URL).into_client_request().map_err(FrameSocketError::from)?;
        {
            let headers = request.headers_mut();
            headers.insert("Origin", HeaderValue::from_static(ORIGIN));
            headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
            headers.insert("Pragma", HeaderValue::from_static("no-cache"));
        }
        // Go: conn.SetReadLimit(FrameMaxSize)
        let config = WebSocketConfig {
            max_message_size: Some(FRAME_MAX_SIZE),
            max_frame_size: Some(FRAME_MAX_SIZE),
            ..Default::default()
        };
        let (stream, _) =
            connect_async_with_config(request, Some(config), false).await.map_err(FrameSocketError::from)?;

        let (sink, mut read) = stream.split();
        let (tx, rx) = mpsc::channel(CHANNEL_SIZE);
        let closed = Arc::new(AtomicBool::new(false));
        let callback: Arc<Mutex<Option<Callback>>> = Arc::new(Mutex::new(None));

        let read_closed = Arc::clone(&closed);
        let read_callback = Arc::clone(&callback);
        let read_task = tokio::spawn(async move {
            // Port of `readPump` + `processData`: messages are a byte stream of
            // 3-byte-length-prefixed frames that may be split or packed.
            let mut pending: Vec<u8> = Vec::new();
            while let Some(result) = read.next().await {
                let message = match result {
                    Ok(message) => message,
                    // Go logs the error and lets the deferred Close(0) run.
                    Err(_) => break,
                };
                match message {
                    Message::Binary(bytes) => pending.extend_from_slice(&bytes),
                    // Go: "Got unexpected websocket message type" → continue.
                    Message::Close(_) => break,
                    _ => continue,
                }
                loop {
                    if pending.len() < FRAME_LENGTH_SIZE {
                        break;
                    }
                    let length =
                        (usize::from(pending[0]) << 16) | (usize::from(pending[1]) << 8) | usize::from(pending[2]);
                    if pending.len() < FRAME_LENGTH_SIZE + length {
                        break;
                    }
                    let frame = pending[FRAME_LENGTH_SIZE..FRAME_LENGTH_SIZE + length].to_vec();
                    pending.drain(..FRAME_LENGTH_SIZE + length);
                    if tx.send(frame).await.is_err() {
                        // The consumer is gone; stop reading.
                        break;
                    }
                }
            }
            Self::fire_disconnect(&read_closed, &read_callback, true);
        });

        Ok(Self {
            sink: Arc::new(tokio::sync::Mutex::new(Some(sink))),
            frames: Arc::new(tokio::sync::Mutex::new(rx)),
            callback,
            closed,
            header_pending: Arc::new(AtomicBool::new(true)),
            read_task: Arc::new(tokio::sync::Mutex::new(Some(read_task))),
        })
    }

    /// Fire the `OnDisconnect` callback exactly once (`closed` acts as the latch).
    fn fire_disconnect(closed: &Arc<AtomicBool>, callback: &Arc<Mutex<Option<Callback>>>, remote: bool) {
        if closed.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Ok(guard) = callback.lock()
            && let Some(callback) = guard.as_ref()
        {
            callback(remote);
        }
    }

    /// Go: `IsConnected` — true while the WebSocket is still open.
    pub fn is_connected(&self) -> bool {
        !self.closed.load(Ordering::SeqCst)
    }

    /// Receive the next complete frame (bytes for `binary::unpack`).
    ///
    /// `None` once the read task has finished and every buffered frame was consumed.
    pub async fn recv(&self) -> Option<Vec<u8>> {
        self.frames.lock().await.recv().await
    }

    /// Set the `OnDisconnect` callback (Go's `OnDisconnect func(ctx, remote bool)`).
    ///
    /// `remote == true` means the peer went away on its own; `remote == false`
    /// means [`FrameSocket::close`] was called locally (Go: `Close(0)`).
    pub fn set_on_disconnect(
        &self,
        callback: Box<dyn Fn(bool) + Send + Sync + 'static>,
    ) -> Result<(), FrameSocketError> {
        *self.callback.lock().map_err(|_| FrameSocketError::CallbackPoisoned)? = Some(callback);
        Ok(())
    }

    /// Drop the `OnDisconnect` callback (Go sets `fs.OnDisconnect = nil` before an
    /// expected shutdown, so a local close is not reported as a remote one).
    pub fn clear_on_disconnect(&self) {
        if let Ok(mut guard) = self.callback.lock() {
            *guard = None;
        }
    }

    /// Go: `SendFrame` — prefix the WA connection header (once) and the 3-byte
    /// big-endian length, then write one binary WebSocket message.
    pub async fn send_frame(&self, frame: Vec<u8>) -> Result<(), FrameSocketError> {
        if frame.len() >= FRAME_MAX_SIZE {
            return Err(FrameSocketError::FrameTooLarge { got: frame.len() });
        }
        let header_len = if self.header_pending.swap(false, Ordering::SeqCst) { WA_CONN_HEADER.len() } else { 0 };
        let mut payload = Vec::with_capacity(header_len + FRAME_LENGTH_SIZE + frame.len());
        if header_len > 0 {
            payload.extend_from_slice(&WA_CONN_HEADER);
        }
        payload.extend_from_slice(&[
            ((frame.len() >> 16) & 0xFF) as u8,
            ((frame.len() >> 8) & 0xFF) as u8,
            (frame.len() & 0xFF) as u8,
        ]);
        payload.extend_from_slice(&frame);

        let mut guard = self.sink.lock().await;
        let sink = guard.as_mut().ok_or(FrameSocketError::Closed)?;
        sink.send(Message::Binary(payload)).await.map_err(FrameSocketError::from)
    }

    /// Go: `Close(code)` — `code > 0` sends a graceful Close frame, `code == 0`
    /// force-closes. Idempotent; the disconnect callback fires at most once.
    pub async fn close(&self, code: u16) {
        {
            let mut guard = self.sink.lock().await;
            match guard.take() {
                Some(mut sink) => {
                    if code > 0 {
                        let _ =
                            sink.send(Message::Close(Some(CloseFrame { code: code.into(), reason: "".into() }))).await;
                    }
                    let _ = sink.close().await;
                }
                // Go returns early when there is no connection, so a second
                // close() must not fire the callback again.
                None => return,
            }
        }
        if let Some(task) = self.read_task.lock().await.take() {
            task.abort();
        }
        Self::fire_disconnect(&self.closed, &self.callback, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    fn prefix(payload: &[u8]) -> Vec<u8> {
        let mut out = vec![
            ((payload.len() >> 16) & 0xFF) as u8,
            ((payload.len() >> 8) & 0xFF) as u8,
            (payload.len() & 0xFF) as u8,
        ];
        out.extend_from_slice(payload);
        out
    }

    /// Spawn a server that drops the connection as soon as it gets one message.
    async fn spawn_dropping_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let _ = ws.next().await;
            drop(ws);
        });
        format!("ws://{addr}")
    }

    #[tokio::test]
    async fn sends_connection_header_once_then_length_prefixed_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let mut received = Vec::new();
            while let Some(Ok(message)) = ws.next().await {
                if let Message::Binary(bytes) = message {
                    received.push(bytes.to_vec());
                    if received.len() == 2 {
                        break;
                    }
                }
            }
            received
        });

        let socket = FrameSocket::connect(Some(&format!("ws://{addr}"))).await.expect("connect");

        let mut first = WA_CONN_HEADER.to_vec();
        first.extend_from_slice(&prefix(b"first"));
        socket.send_frame(b"first".to_vec()).await.unwrap();
        socket.send_frame(b"second".to_vec()).await.unwrap();

        let received =
            tokio::time::timeout(Duration::from_secs(5), server).await.expect("server timed out").expect("server task");
        assert_eq!(received.len(), 2);
        assert_eq!(received[0], first, "the WA header goes out exactly once");
        assert_eq!(received[1], prefix(b"second"), "subsequent frames carry no header");
    }

    #[tokio::test]
    async fn reassembles_multiple_frames_from_one_message() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let mut packed = prefix(b"one");
            packed.extend_from_slice(&prefix(b"two"));
            ws.send(Message::Binary(packed)).await.unwrap();
            // Keep the connection open until the client closes it.
            while let Some(Ok(_)) = ws.next().await {}
        });

        let socket = FrameSocket::connect(Some(&format!("ws://{addr}"))).await.expect("connect");
        let first =
            tokio::time::timeout(Duration::from_secs(5), socket.recv()).await.expect("timed out").expect("frame");
        let second =
            tokio::time::timeout(Duration::from_secs(5), socket.recv()).await.expect("timed out").expect("frame");
        assert_eq!(first, b"one");
        assert_eq!(second, b"two");
    }

    #[tokio::test]
    async fn reassembles_a_frame_split_across_messages() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            let whole = prefix(b"hello");
            // Split even inside the 3-byte length header.
            ws.send(Message::Binary(whole[..2].to_vec())).await.unwrap();
            ws.send(Message::Binary(whole[2..5].to_vec())).await.unwrap();
            ws.send(Message::Binary(whole[5..].to_vec())).await.unwrap();
            while let Some(Ok(_)) = ws.next().await {}
        });

        let socket = FrameSocket::connect(Some(&format!("ws://{addr}"))).await.expect("connect");
        let frame =
            tokio::time::timeout(Duration::from_secs(5), socket.recv()).await.expect("timed out").expect("frame");
        assert_eq!(frame, b"hello");
    }

    #[tokio::test]
    async fn fires_on_disconnect_once_when_the_peer_drops() {
        let url = spawn_dropping_server().await;
        let socket = FrameSocket::connect(Some(&url)).await.expect("connect");
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits_writer = Arc::clone(&hits);
        socket
            .set_on_disconnect(Box::new(move |remote| {
                hits_writer.lock().unwrap().push(remote);
            }))
            .unwrap();

        // Signal the server to drop the connection.
        socket.send_frame(b"drop".to_vec()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;

        let recorded = hits.lock().unwrap().clone();
        assert_eq!(recorded, vec![true], "remote disconnect reported once");
        assert!(!socket.is_connected());
    }

    #[tokio::test]
    async fn close_is_idempotent_and_send_after_close_fails() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(_)) = ws.next().await {}
        });
        let socket = FrameSocket::connect(Some(&format!("ws://{addr}"))).await.expect("connect");
        let hits = Arc::new(Mutex::new(0usize));
        let counter = Arc::clone(&hits);
        socket
            .set_on_disconnect(Box::new(move |_remote| {
                *counter.lock().unwrap() += 1;
            }))
            .unwrap();

        socket.close(0).await;
        socket.close(0).await;
        assert_eq!(*hits.lock().unwrap(), 1, "close() is idempotent");
        assert!(!socket.is_connected());
        assert!(matches!(socket.send_frame(b"nope".to_vec()).await, Err(FrameSocketError::Closed)));
    }

    #[tokio::test]
    async fn rejects_oversized_frames() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            while let Some(Ok(_)) = ws.next().await {}
        });
        let socket = FrameSocket::connect(Some(&format!("ws://{addr}"))).await.expect("connect");
        let oversized = vec![0u8; FRAME_MAX_SIZE];
        assert!(matches!(socket.send_frame(oversized).await, Err(FrameSocketError::FrameTooLarge { .. })));
    }

    #[test]
    fn connection_header_matches_go() {
        // Go: WAConnHeader = []byte{'W', 'A', WA_MAGIC_VALUE, token.DictVersion}
        assert_eq!(WA_CONN_HEADER, [b'W', b'A', 6, 3]);
        assert_eq!(WA_MAGIC_VALUE, 6);
        assert_eq!(crate::binary::token::DICT_VERSION, 3);
        assert_eq!(FRAME_LENGTH_SIZE, 3);
        assert_eq!(FRAME_MAX_SIZE, 1 << 24);
        assert_eq!(URL, "wss://web.whatsapp.com/ws/chat");
        assert_eq!(ORIGIN, "https://web.whatsapp.com");
    }
}
