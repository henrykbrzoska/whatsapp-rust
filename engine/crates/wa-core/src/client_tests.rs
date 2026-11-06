// Ported from the Go client (`handshake.go` + the Go reference transcript), © Tulir
// Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Handshake tests. Nothing here touches the network: every test binds
//! `127.0.0.1:0` and wraps its work in `tokio::time::timeout`, so a broken test
//! fails fast instead of hanging.
//!
//! Two complementary proofs:
//!
//! 1. **Synthetic peer** — this file implements the *server* half of the same
//!    XX pattern with [`NoiseHandshake`], so a full transcript is exercised
//!    (including certificate verification and the derived keys) without
//!    WhatsApp.
//! 2. **Go golden transcript** — a transcript produced by running the Go
//!    reference (`go run` against `Go socket`, deleted afterwards) is
//!    replayed byte for byte; the client must derive the same session keys and
//!    produce the same `ClientFinish`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use wa_proto::wa_cert::CertChain;
use wa_proto::wa_cert::cert_chain::NoiseCertificate;
use wa_proto::wa_cert::cert_chain::noise_certificate::Details as CertDetails;
use wa_proto::wa_web_protobufs_wa6::handshake_message::{ClientFinish, ClientHello, ServerHello};
use wa_proto::wa_web_protobufs_wa6::{ClientPayload, HandshakeMessage};

use super::*;
use crate::crypto::keys::{calculate_signature, clamp};
use crate::socket::frame_socket::FRAME_LENGTH_SIZE;
use crate::socket::noise_handshake::generate_iv;
use crate::types::version::{WA_VERSION, build_client_payload};

/// Deterministic key material: `KeyPair::from_private` clamps, exactly like Go.
const CLIENT_EPHEMERAL_PRIV: [u8; 32] = [
    0x77, 0x07, 0x6d, 0x0a, 0x73, 0x18, 0xa5, 0x7d, 0x3c, 0x16, 0xc1, 0x72, 0x51, 0xb2, 0x66, 0x45, 0xdf, 0x4c, 0x2f,
    0x87, 0xeb, 0xc0, 0x99, 0x2a, 0xb1, 0x77, 0xfb, 0xa5, 0x1d, 0xb9, 0x2c, 0x2a,
];
const CLIENT_NOISE_PRIV: [u8; 32] = [
    0x5d, 0xab, 0x08, 0x7e, 0x62, 0x4a, 0x8a, 0x4b, 0x79, 0xe1, 0x7f, 0x8b, 0x83, 0x80, 0x0e, 0xe6, 0x6f, 0x3b, 0xb1,
    0x29, 0x26, 0x18, 0xb6, 0xfd, 0x1c, 0x2f, 0x8b, 0x27, 0xff, 0x88, 0xe0, 0xeb,
];
const SERVER_EPHEMERAL_PRIV: [u8; 32] = [
    0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2a, 0x2b, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x31, 0x32,
    0x33, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x3b, 0x3c, 0x3d, 0x3e, 0x3f,
];
const SERVER_STATIC_PRIV: [u8; 32] = [
    0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x51, 0x52,
    0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x5b, 0x5c, 0x5d, 0x5e, 0x5f,
];
const CA_PRIV: [u8; 32] = [
    0x60, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6a, 0x6b, 0x6c, 0x6d, 0x6e, 0x6f, 0x70, 0x71, 0x72,
    0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e, 0x7f,
];
const INTERMEDIATE_PRIV: [u8; 32] = [
    0x80, 0x81, 0x82, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8a, 0x8b, 0x8c, 0x8d, 0x8e, 0x8f, 0x90, 0x91, 0x92,
    0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0x9b, 0x9c, 0x9d, 0x9e, 0x9f,
];

/// Slack for every await in these tests.
const STEP: Duration = Duration::from_secs(5);

/// The certificate window every fixture uses (`ecc.CalculateSignature` is
/// randomized, so the bytes of the chain change per run; the window must still
/// cover "now").
const NOT_BEFORE: u64 = 1_600_000_000;
const NOT_AFTER: u64 = 2_000_000_000;

fn ca() -> KeyPair {
    KeyPair::from_private(CA_PRIV)
}

fn deterministic_random() -> [u8; 64] {
    [0x42; 64]
}

/// A two-level Noise certificate chain signed with `ca` (Go: `waCert.CertChain`).
fn cert_chain(ca: &KeyPair, intermediate: &KeyPair, server_static: &KeyPair) -> Vec<u8> {
    let intermediate_details = CertDetails {
        serial: Some(1),
        issuer_serial: Some(WACERT_ISSUER_SERIAL),
        key: Some(intermediate.public.to_vec()),
        not_before: Some(NOT_BEFORE),
        not_after: Some(NOT_AFTER),
    }
    .encode_to_vec();
    let leaf_details = CertDetails {
        serial: Some(2),
        issuer_serial: Some(1),
        key: Some(server_static.public.to_vec()),
        not_before: Some(NOT_BEFORE),
        not_after: Some(NOT_AFTER),
    }
    .encode_to_vec();
    CertChain {
        intermediate: Some(NoiseCertificate {
            details: Some(intermediate_details.clone()),
            signature: Some(
                calculate_signature(&clamp(ca.private), &intermediate_details, &deterministic_random()).to_vec(),
            ),
        }),
        leaf: Some(NoiseCertificate {
            details: Some(leaf_details.clone()),
            signature: Some(
                calculate_signature(&clamp(intermediate.private), &leaf_details, &deterministic_random()).to_vec(),
            ),
        }),
    }
    .encode_to_vec()
}

fn length_prefixed(payload: &[u8]) -> Vec<u8> {
    let mut out =
        vec![((payload.len() >> 16) & 0xFF) as u8, ((payload.len() >> 8) & 0xFF) as u8, (payload.len() & 0xFF) as u8];
    out.extend_from_slice(payload);
    out
}

/// Read exactly one complete WA frame from the raw WebSocket, stripping the WA
/// connection header if this is the peer's first message.
async fn next_frame<S>(ws: &mut WebSocketStream<S>, first: &mut bool) -> Vec<u8>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let message = tokio::time::timeout(STEP, ws.next()).await.expect("server read timed out");
        match message {
            Some(Ok(WsMessage::Binary(bytes))) => pending.extend_from_slice(&bytes),
            Some(Ok(_)) => continue,
            other => panic!("server could not read a frame: {other:?}"),
        }
        if *first && pending.starts_with(&WA_CONN_HEADER) {
            pending.drain(..WA_CONN_HEADER.len());
            *first = false;
        }
        if pending.len() >= FRAME_LENGTH_SIZE {
            let length = (usize::from(pending[0]) << 16) | (usize::from(pending[1]) << 8) | usize::from(pending[2]);
            if pending.len() >= FRAME_LENGTH_SIZE + length {
                return pending[FRAME_LENGTH_SIZE..FRAME_LENGTH_SIZE + length].to_vec();
            }
        }
    }
}

/// Which required part of the `ServerHello` a truncated-response test drops.
#[derive(Clone, Copy)]
enum Truncate {
    MissingStatic,
    MissingPayload,
    ShortEphemeral,
}

/// The server half of the handshake. Returns the frames the client must have
/// sent, plus the server's `(write, read)` keys.
struct ServerOutcome {
    client_payload: Vec<u8>,
    client_noise_pub: [u8; KEY_SIZE],
    write_key: [u8; KEY_SIZE],
    read_key: [u8; KEY_SIZE],
    cert_chain: Vec<u8>,
}

async fn serve<S>(ws: &mut WebSocketStream<S>, ca: &KeyPair) -> ServerOutcome
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let server_ephemeral = KeyPair::from_private(SERVER_EPHEMERAL_PRIV);
    let server_static = KeyPair::from_private(SERVER_STATIC_PRIV);
    let intermediate = KeyPair::from_private(INTERMEDIATE_PRIV);
    let cert_chain = cert_chain(ca, &intermediate, &server_static);

    let mut first = true;
    // ClientHello { ephemeral }.
    let hello = HandshakeMessage::decode(next_frame(ws, &mut first).await.as_slice()).expect("client hello");
    let client_ephemeral = hello.client_hello.expect("client hello").ephemeral.expect("ephemeral");
    let client_ephemeral: [u8; KEY_SIZE] = client_ephemeral.try_into().expect("32-byte ephemeral");

    let mut nh = NoiseHandshake::new();
    nh.start(NOISE_START_PATTERN, &WA_CONN_HEADER).expect("start");
    nh.authenticate(&client_ephemeral);
    nh.authenticate(&server_ephemeral.public);
    nh.mix_shared_secret_into_key(&server_ephemeral.private, &client_ephemeral).expect("mix ee");
    let static_ciphertext = nh.encrypt(&server_static.public).expect("encrypt static");
    nh.mix_shared_secret_into_key(&server_static.private, &client_ephemeral).expect("mix es");
    let payload_ciphertext = nh.encrypt(&cert_chain).expect("encrypt cert chain");

    let hello = HandshakeMessage {
        server_hello: Some(ServerHello {
            ephemeral: Some(server_ephemeral.public.to_vec()),
            r#static: Some(static_ciphertext),
            payload: Some(payload_ciphertext),
            ..Default::default()
        }),
        ..Default::default()
    };
    ws.send(WsMessage::Binary(length_prefixed(&hello.encode_to_vec()))).await.expect("send server hello");

    // ClientFinish { static, payload }.
    let finish = HandshakeMessage::decode(next_frame(ws, &mut first).await.as_slice()).expect("client finish");
    let finish: ClientFinish = finish.client_finish.expect("client finish");
    let client_noise_pub: [u8; KEY_SIZE] =
        nh.decrypt(&finish.r#static.expect("static")).expect("decrypt client static").try_into().expect("32 bytes");
    nh.mix_shared_secret_into_key(&server_ephemeral.private, &client_noise_pub).expect("mix se");
    let client_payload = nh.decrypt(&finish.payload.expect("payload")).expect("decrypt client payload");
    let (write_key, read_key) = nh.finish_keys().expect("finish");

    ServerOutcome { client_payload, client_noise_pub, write_key, read_key, cert_chain }
}

/// Start a local server that runs `serve` and then does whatever `after` says.
async fn spawn_server<T, F, Fut>(ca: KeyPair, after: F) -> (String, tokio::task::JoinHandle<T>)
where
    T: Send + 'static,
    F: FnOnce(WebSocketStream<tokio::net::TcpStream>, ServerOutcome) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("addr");
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept_async(stream).await.expect("ws accept");
        let outcome = serve(&mut ws, &ca).await;
        after(ws, outcome).await
    });
    (format!("ws://{addr}"), task)
}

fn options(ca: &KeyPair) -> HandshakeOptions {
    HandshakeOptions { response_timeout: Duration::from_secs(5), cert_authority: ca.public, check_cert_validity: true }
}

fn seal(key: &[u8; KEY_SIZE], counter: u32, plaintext: &[u8]) -> Vec<u8> {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    let cipher = Aes256Gcm::new_from_slice(key).expect("key");
    cipher.encrypt(Nonce::from_slice(&generate_iv(counter)), Payload { msg: plaintext, aad: &[] }).expect("seal")
}

fn open(key: &[u8; KEY_SIZE], counter: u32, ciphertext: &[u8]) -> Vec<u8> {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    let cipher = Aes256Gcm::new_from_slice(key).expect("key");
    cipher.decrypt(Nonce::from_slice(&generate_iv(counter)), Payload { msg: ciphertext, aad: &[] }).expect("open")
}

#[tokio::test]
async fn completes_the_handshake_against_a_synthetic_server() {
    let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&received);

    let (url, server) = spawn_server(ca(), move |mut ws, outcome| async move {
        // The client must now be able to talk on the encrypted socket.
        let mut first = false; // the WA header was already consumed by ClientHello
        let frame = next_frame(&mut ws, &mut first).await;
        // Mirror side: the client's write key is the server's read key (`Finish`
        // returns the same pair for both; each side interprets it the other way
        // round — see the Go client's `noisehandshake.go:Finish`).
        assert_eq!(open(&outcome.write_key, 0, &frame), b"ping from the client");
        let reply = seal(&outcome.read_key, 0, b"pong from the server");
        ws.send(WsMessage::Binary(length_prefixed(&reply))).await.expect("send reply");
        outcome
    })
    .await;

    let frames = FrameSocket::connect(Some(&url)).await.expect("connect");
    let sink_events = Arc::clone(&sink);
    let outcome = tokio::time::timeout(
        STEP,
        do_handshake_with(
            frames,
            &KeyPair::from_private(CLIENT_EPHEMERAL_PRIV),
            &KeyPair::from_private(CLIENT_NOISE_PRIV),
            build_client_payload(WA_VERSION),
            options(&ca()),
            Box::new(move |frame| sink_events.lock().expect("sink").push(frame)),
            None,
        ),
    )
    .await
    .expect("handshake timed out");
    let socket = match outcome {
        Ok(socket) => socket,
        Err(err) => panic!("handshake failed: {err}"),
    };

    assert!(socket.is_connected(), "the encrypted socket is live once the handshake completed");

    // The server is waiting for a frame on the encrypted socket: prove both
    // directions with real traffic before collecting its report.
    socket.send_frame(b"ping from the client").await.expect("send");

    let outcome = tokio::time::timeout(STEP, server).await.expect("server timed out").expect("server task");

    // The certificate chain and the client payload arrived intact.
    assert_eq!(
        outcome.cert_chain,
        cert_chain(&ca(), &KeyPair::from_private(INTERMEDIATE_PRIV), &KeyPair::from_private(SERVER_STATIC_PRIV))
    );
    assert_eq!(outcome.client_noise_pub, KeyPair::from_private(CLIENT_NOISE_PRIV).public);
    assert_eq!(outcome.client_payload, build_client_payload(WA_VERSION).encode_to_vec());

    // Both sides derived compatible keys: the server's read key is the client's
    // write key and the other way around.
    assert_ne!(outcome.write_key, outcome.read_key);
    for _ in 0..100 {
        if !received.lock().expect("sink").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(*received.lock().expect("sink"), vec![b"pong from the server".to_vec()]);
    assert_eq!(socket.write_counter(), 1);
    assert_eq!(socket.read_counter(), 1);
    socket.stop(true, false).await;
}

#[tokio::test]
async fn times_out_when_the_server_never_answers() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept_async(stream).await.expect("ws accept");
        // Read the ClientHello and stay silent until the client gives up.
        let _ = ws.next().await;
        while let Some(Ok(_)) = ws.next().await {}
    });

    let frames = FrameSocket::connect(Some(&format!("ws://{addr}"))).await.expect("connect");
    let start = std::time::Instant::now();
    let result = tokio::time::timeout(
        STEP,
        do_handshake_with(
            frames,
            &KeyPair::from_private(CLIENT_EPHEMERAL_PRIV),
            &KeyPair::from_private(CLIENT_NOISE_PRIV),
            build_client_payload(WA_VERSION),
            HandshakeOptions {
                response_timeout: Duration::from_millis(150),
                cert_authority: ca().public,
                check_cert_validity: true,
            },
            Box::new(|_| {}),
            None,
        ),
    )
    .await
    .expect("the test must not hang");

    match result {
        Err(ClientError::HandshakeTimeout) => {}
        Err(other) => panic!("expected a timeout, got {other}"),
        Ok(_) => panic!("the handshake must not succeed without a ServerHello"),
    }
    assert!(start.elapsed() >= Duration::from_millis(150));
    server.abort();
}

async fn truncated_server_hello(truncate: Truncate) -> ClientError {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept_async(stream).await.expect("ws accept");
        let _ = ws.next().await;
        let reply = HandshakeMessage {
            server_hello: Some(ServerHello {
                ephemeral: Some(match truncate {
                    Truncate::ShortEphemeral => vec![7u8; 31],
                    _ => vec![7u8; 32],
                }),
                r#static: match truncate {
                    Truncate::MissingStatic => None,
                    _ => Some(vec![1u8; 48]),
                },
                payload: match truncate {
                    Truncate::MissingPayload => None,
                    _ => Some(vec![2u8; 48]),
                },
                ..Default::default()
            }),
            ..Default::default()
        };
        ws.send(WsMessage::Binary(length_prefixed(&reply.encode_to_vec()))).await.expect("send");
        while let Some(Ok(_)) = ws.next().await {}
    });

    let frames = FrameSocket::connect(Some(&format!("ws://{addr}"))).await.expect("connect");
    let result = tokio::time::timeout(
        STEP,
        do_handshake_with(
            frames,
            &KeyPair::from_private(CLIENT_EPHEMERAL_PRIV),
            &KeyPair::from_private(CLIENT_NOISE_PRIV),
            build_client_payload(WA_VERSION),
            options(&ca()),
            Box::new(|_| {}),
            None,
        ),
    )
    .await
    .expect("the test must not hang");
    match result {
        Err(err) => err,
        Ok(_) => panic!("a truncated ServerHello must be rejected"),
    }
}

#[tokio::test]
async fn rejects_a_server_hello_without_static() {
    assert!(matches!(truncated_server_hello(Truncate::MissingStatic).await, ClientError::MissingHandshakeParts));
}

#[tokio::test]
async fn rejects_a_server_hello_without_payload() {
    assert!(matches!(truncated_server_hello(Truncate::MissingPayload).await, ClientError::MissingHandshakeParts));
}

#[tokio::test]
async fn rejects_a_31_byte_server_ephemeral() {
    assert!(matches!(truncated_server_hello(Truncate::ShortEphemeral).await, ClientError::MissingHandshakeParts));
}

/// The `ServerHello` parts are all present but the certificate is signed by
/// somebody else: verification must fail *before* the ClientFinish goes out.
#[tokio::test]
async fn rejects_a_certificate_from_a_foreign_ca() {
    let (url, server) = spawn_server(ca(), |mut ws, _outcome| async move {
        // Stay quiet; the client aborts after the cert check.
        while let Some(Ok(_)) = ws.next().await {}
    })
    .await;

    let frames = FrameSocket::connect(Some(&url)).await.expect("connect");
    let foreign = KeyPair::from_private([0xAA; 32]);
    let result = tokio::time::timeout(
        STEP,
        do_handshake_with(
            frames,
            &KeyPair::from_private(CLIENT_EPHEMERAL_PRIV),
            &KeyPair::from_private(CLIENT_NOISE_PRIV),
            build_client_payload(WA_VERSION),
            options(&foreign),
            Box::new(|_| {}),
            None,
        ),
    )
    .await
    .expect("the test must not hang");

    match result {
        Err(ClientError::Cert(CertError::IntermediateSignature)) => {}
        Err(other) => panic!("expected a certificate failure, got {other}"),
        Ok(_) => panic!("a foreign CA must not be accepted"),
    }
    server.abort();
}

#[test]
fn verify_server_cert_accepts_a_well_formed_chain() {
    let certificate =
        cert_chain(&ca(), &KeyPair::from_private(INTERMEDIATE_PRIV), &KeyPair::from_private(SERVER_STATIC_PRIV));
    let server_static = KeyPair::from_private(SERVER_STATIC_PRIV);
    verify_server_cert_with(&ca().public, &certificate, &server_static.public, true).expect("valid chain");
}

#[test]
fn verify_server_cert_rejects_a_tampered_leaf() {
    let server_static = KeyPair::from_private(SERVER_STATIC_PRIV);
    let mut chain = CertChain::decode(
        cert_chain(&ca(), &KeyPair::from_private(INTERMEDIATE_PRIV), &KeyPair::from_private(SERVER_STATIC_PRIV))
            .as_slice(),
    )
    .expect("decode");
    let leaf = chain.leaf.as_mut().expect("leaf");
    let mut details = CertDetails::decode(leaf.details.clone().expect("details").as_slice()).expect("details");
    details.serial = Some(999);
    leaf.details = Some(details.encode_to_vec());

    let result = verify_server_cert_with(&ca().public, &chain.encode_to_vec(), &server_static.public, true);
    assert!(matches!(result, Err(CertError::LeafSignature)), "got {result:?}");
}

#[test]
fn verify_server_cert_rejects_a_static_key_mismatch() {
    let certificate =
        cert_chain(&ca(), &KeyPair::from_private(INTERMEDIATE_PRIV), &KeyPair::from_private(SERVER_STATIC_PRIV));
    let other = KeyPair::from_private([0x11; 32]);
    let result = verify_server_cert_with(&ca().public, &certificate, &other.public, true);
    assert!(matches!(result, Err(CertError::KeyMismatch)), "got {result:?}");
}

#[test]
fn verify_server_cert_rejects_an_expired_certificate() {
    let server_static = KeyPair::from_private(SERVER_STATIC_PRIV);
    let intermediate = KeyPair::from_private(INTERMEDIATE_PRIV);
    let ca = ca();
    // A chain whose whole window is in the past.
    let details_of = |serial: u32, key: &[u8; 32]| {
        CertDetails {
            serial: Some(serial),
            issuer_serial: Some(serial - 1),
            key: Some(key.to_vec()),
            not_before: Some(1000),
            not_after: Some(2000),
        }
        .encode_to_vec()
    };
    let intermediate_details = details_of(1, &intermediate.public);
    let leaf_details = details_of(2, &server_static.public);
    let expired = CertChain {
        intermediate: Some(NoiseCertificate {
            details: Some(intermediate_details.clone()),
            signature: Some(
                calculate_signature(&clamp(ca.private), &intermediate_details, &deterministic_random()).to_vec(),
            ),
        }),
        leaf: Some(NoiseCertificate {
            details: Some(leaf_details.clone()),
            signature: Some(
                calculate_signature(&clamp(intermediate.private), &leaf_details, &deterministic_random()).to_vec(),
            ),
        }),
    };

    // `intermediate.issuer_serial` must be WACertIssuerSerial in production, but
    // with a custom CA the check still compares against the constant — so use a
    // chain whose intermediate issuer serial is 0 instead.
    let mut details = CertDetails::decode(intermediate_details.as_slice()).expect("details");
    details.issuer_serial = Some(WACERT_ISSUER_SERIAL);
    let intermediate_details = details.encode_to_vec();
    let expired = CertChain {
        intermediate: Some(NoiseCertificate {
            details: Some(intermediate_details.clone()),
            signature: Some(
                calculate_signature(&clamp(ca.private), &intermediate_details, &deterministic_random()).to_vec(),
            ),
        }),
        ..expired
    };

    let result = verify_server_cert_with(&ca.public, &expired.encode_to_vec(), &server_static.public, true);
    assert!(matches!(result, Err(CertError::Expired { kind: CertKind::Intermediate, .. })), "got {result:?}");

    // …and it verifies once the validity window is not checked.
    verify_server_cert_with(&ca.public, &expired.encode_to_vec(), &server_static.public, false)
        .expect("structure is sound");
}

#[test]
fn rejects_a_certificate_missing_its_parts() {
    let result = verify_server_cert_with(&ca().public, &[], &[0u8; 32], true);
    assert!(matches!(result, Err(CertError::MissingParts)), "got {result:?}");
}

#[test]
fn signature_lengths_are_checked() {
    let server_static = KeyPair::from_private(SERVER_STATIC_PRIV);
    let mut chain =
        CertChain::decode(cert_chain(&ca(), &KeyPair::from_private(INTERMEDIATE_PRIV), &server_static).as_slice())
            .expect("decode");
    chain.intermediate.as_mut().expect("intermediate").signature = Some(vec![0u8; 63]);

    let result = verify_server_cert_with(&ca().public, &chain.encode_to_vec(), &server_static.public, true);
    assert!(matches!(result, Err(CertError::IntermediateSignatureLength(63))), "got {result:?}");
}

/// `ClientHello` must carry the ephemeral key the state machine authenticated —
/// the same bytes Go puts on the wire.
#[test]
fn client_hello_carries_the_ephemeral_public_key() {
    let hello = HandshakeMessage {
        client_hello: Some(ClientHello {
            ephemeral: Some(KeyPair::from_private(CLIENT_EPHEMERAL_PRIV).public.to_vec()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let decoded = HandshakeMessage::decode(hello.encode_to_vec().as_slice()).expect("roundtrip");
    assert_eq!(
        decoded.client_hello.expect("hello").ephemeral.expect("ephemeral"),
        KeyPair::from_private(CLIENT_EPHEMERAL_PRIV).public.to_vec()
    );
}

#[test]
fn wa_constants_match_go() {
    assert_eq!(NOISE_HANDSHAKE_RESPONSE_TIMEOUT, Duration::from_secs(20));
    assert_eq!(WACERT_ISSUER_SERIAL, 0);
    assert_eq!(
        hex::encode(WACERT_PUB_KEY),
        "14237557 4d0a5871 66aae71e be516437 c4a28b73 e3695c6c e1f7f954 5da8ee6b".replace(' ', "")
    );
    // `(serverHello let some)` default keeps `ClientPayload` untouched.
    assert!(ClientPayload::default().user_agent.is_none());
}

// ---------------------------------------------------------------------------
// Go golden transcript
// ---------------------------------------------------------------------------
//
// Produced by running the Go reference implementation (`Go socket` +
// `waCert` + libsignal) over the same deterministic key material this file
// uses, with the temporary generator `Go zz-handshake-vector/` — deleted
// after it printed the numbers below (never committed).
//
// `ecc.CalculateSignature` is randomized, so the *bytes* of the certificate
// chain (and therefore `SERVER_HELLO`/`CLIENT_FINISH`) are one consistent
// snapshot of a single run; the keys and the payload are reproducible, and
// `types::version` pins the payload against its own golden vector. The test below replays this transcript byte for byte: if
// the Rust handshake ever reorders a single hash update, mix or encryption, the
// derived keys stop matching and the client's `ClientFinish` stops being
// identical.

const CA_PUB: &str = "675dd574ed7789310b3d2e7681f3790b466c773b1521fecf36577958371ea52f";

const CLIENT_HELLO: &str = "12220a208520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a";

const SERVER_HELLO: &str = concat!(
    "1ad7020a20358072d6365880d1aeea329adf9121383851ed21a28e3b75e965d0d2cd16625412305d6cb47834b618ef1b",
    "479bcc00c5ad5fbec6feb81f4a8cdb9062e601e6c0399c0d0f65e174af69296764295a3ae02e341a8002339592da4937",
    "bbbfb42cf8b52cfdc799b60d28f68a24881be4d1eb8dd28d2a51507bc0136e565a4dbfd154dc8502967025581dac4797",
    "68546860b85ad88813ec88d0ac47ce91d4db3829480ef38c91ef4ff265e73d86d6bfb21f6dbbe5986bd1cb16631da3d0",
    "63d36a3a256c27c6c2f2ab4a661846444fd1c7d3324d9959fc883135dc5d4efdf61bdd9b1cf878fdca2fa5569da17183",
    "98c6cfd4e0e21998823992b362e36f4ada715c7fbf06e9c4d330305a3e84277db12a7efd16e124889d21bf72fe8e65d4",
    "85a6fe6a2a58a1bcac5396534516d938af3288d67a818eba768362ec1f69567d81f0bbea944bc1bf1188e830091d4b5a",
    "4077cb69761c7ca42007",
);

const CLIENT_FINISH: &str = concat!(
    "2288010a30b60352665e7d2bedbdc51c80ac468a790ef3e3e67311b6b1bf48ff566b6caf8633318a0eaa0fb143109c87",
    "73d509d9e21254a8a0108d64f6ae4140bf023c9fb4c30c064f9b1a55f0a545648a1cea9e8048e992986a5bb3a504ec59",
    "be95449e04d320885be6420c42614938df1f24c0969b1963ea00629fc53a777b3b8a1ca5bc3083510ec2c4",
);

const CLIENT_WRITE_KEY: &str = "fe6b36fa3d7654fb4a2facf58abacc676fc7048171a441bca58cb1d2232bcdf6";

const CLIENT_READ_KEY: &str = "284a185b971b939a89a19340af0e5ac6140804807b5afe9f4083a3ff580bc443";

fn unhex32(text: &str) -> [u8; 32] {
    hex::decode(text).expect("hex").try_into().expect("32 bytes")
}

/// The full Go transcript, replayed over a local socket: the client must accept
/// the golden `ServerHello`, emit the golden `ClientFinish` and derive the
/// golden session keys.
#[tokio::test]
async fn replays_the_go_golden_transcript() {
    let server_hello = hex::decode(SERVER_HELLO.replace(char::is_whitespace, "")).expect("hex");
    let expected_client_hello = hex::decode(CLIENT_HELLO.replace(char::is_whitespace, "")).expect("hex");
    let expected_client_finish = hex::decode(CLIENT_FINISH.replace(char::is_whitespace, "")).expect("hex");
    let write_key = unhex32(CLIENT_WRITE_KEY);
    let read_key = unhex32(CLIENT_READ_KEY);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut ws = accept_async(stream).await.expect("ws accept");
        let mut first = true;

        // 1. The ClientHello must be byte-identical to Go's.
        let hello = next_frame(&mut ws, &mut first).await;
        assert_eq!(hello, expected_client_hello, "ClientHello differs from the Go reference");

        // 2. Answer with the recorded ServerHello.
        ws.send(WsMessage::Binary(length_prefixed(&server_hello))).await.expect("send server hello");

        // 3. The ClientFinish must be byte-identical to Go's.
        let finish = next_frame(&mut ws, &mut first).await;
        assert_eq!(finish, expected_client_finish, "ClientFinish differs from the Go reference");

        // 4. Prove the derived keys: open the client's first sealed frame with
        //    the golden write key, then seal a reply with the golden read key.
        let frame = next_frame(&mut ws, &mut first).await;
        assert_eq!(open(&write_key, 0, &frame), b"golden ping");
        ws.send(WsMessage::Binary(length_prefixed(&seal(&read_key, 0, b"golden pong")))).await.expect("send reply");
        while let Some(Ok(_)) = ws.next().await {}
    });

    let frames = FrameSocket::connect(Some(&format!("ws://{addr}"))).await.expect("connect");
    let received: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&received);
    let handshake = tokio::time::timeout(
        STEP,
        do_handshake_with(
            frames,
            &KeyPair::from_private(CLIENT_EPHEMERAL_PRIV),
            &KeyPair::from_private(CLIENT_NOISE_PRIV),
            build_client_payload(WA_VERSION),
            HandshakeOptions {
                response_timeout: Duration::from_secs(5),
                cert_authority: unhex32(CA_PUB),
                // The recorded chain's window is 1.6e9..2e9 (unix seconds).
                check_cert_validity: true,
            },
            Box::new(move |frame| sink.lock().expect("sink").push(frame)),
            None,
        ),
    )
    .await
    .expect("the test must not hang");
    let socket = match handshake {
        Ok(socket) => socket,
        Err(err) => panic!("the golden transcript must be accepted: {err}"),
    };

    socket.send_frame(b"golden ping").await.expect("send");

    for _ in 0..100 {
        if !received.lock().expect("sink").is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        *received.lock().expect("sink"),
        vec![b"golden pong".to_vec()],
        "the golden read key must decrypt the server frame"
    );
    socket.stop(true, false).await;
    server.abort();
}
