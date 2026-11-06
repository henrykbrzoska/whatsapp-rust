// Task A gate tests — `struct Client` + the node loop.
//!
//! Nothing here touches the network: a fake NoiseSocket server speaks the
//! binary protocol over a real `FrameSocket` pair (TCP loopback), so `sendIQ`
//! and `receive_response` are exercised end to end without WhatsApp.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{WebSocketStream, accept_async};

use super::pair::{
    QR_CHANNEL_TIMEOUT, QR_ROTATION_LIMIT, concat_bytes, generate_device_signature, verify_account_signature,
};
use super::*;
use crate::binary::node::{AttrValue, Node};
use crate::crypto::keys::{KeyPair, verify_signature};
use crate::socket::frame_socket::FrameSocket;
use crate::socket::noise_socket::NoiseSocket;
use crate::store::sqlite::Container;
use crate::types::jid::Jid;
use wa_proto::wa_adv::AdvSignedDeviceIdentity;

fn test_events() -> EventSink {
    Arc::new(|_: crate::WaEvent| {})
}

async fn test_client() -> Arc<Client> {
    let container = Container::open_memory().await.expect("in-memory container");
    let device = container.new_device();
    Client::new(device, ClientConfig::default(), test_events())
}

fn length_prefixed(payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(3 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes()[1..]);
    frame.extend_from_slice(payload);
    frame
}

/// Attach a live `NoiseSocket` (zero keys) to the client and return the raw
/// server-side WebSocket the test drives as the "server".
///
/// Decrypted inbound frames are forwarded to `frames_tx` — the test pumps them
/// through `receive_response`, exactly like `handle_frame` does on the real
/// path (`handleFrame` → `receiveResponse`).
async fn attach_noise_socket(
    client: &Arc<Client>,
    frames_tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
) -> WebSocketStream<tokio::net::TcpStream> {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        accept_async(stream).await.expect("ws accept")
    });
    let frames = FrameSocket::connect(Some(&format!("ws://{addr}"))).await.expect("connect");
    let socket = NoiseSocket::new(
        Arc::new(frames),
        &[0u8; KEY_SIZE],
        &[0u8; KEY_SIZE],
        Box::new(move |data| {
            let _ = frames_tx.send(data);
        }),
        None,
    )
    .await
    .expect("noise socket");
    *client.socket.lock().await = Some(Arc::new(socket));
    server.await.expect("server ws")
}

#[tokio::test]
async fn connect_fails_when_device_deleted() {
    let client = test_client().await;
    client.device.lock().await.deleted = true;
    assert!(matches!(client.connect().await, Err(ClientError::DeviceDeleted)));
}

#[tokio::test]
async fn send_iq_round_trip_through_a_fake_server() {
    let client = test_client().await;
    let (frames_tx, mut frames_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let mut server = attach_noise_socket(&client, frames_tx).await;

    // The `handle_frame` half of the real path: decrypted frames go through
    // `receive_response` so the `send_iq` waiter resolves.
    let pump = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            while let Some(data) = frames_rx.recv().await {
                let Ok(decompressed) = crate::binary::unpack(&data) else { continue };
                let Ok(node) = crate::binary::unmarshal(&decompressed) else { continue };
                if client.receive_response(node).await {
                    break;
                }
            }
        })
    };

    let server_task = tokio::spawn(async move {
        // The first client frame carries the 4-byte WA header before the
        // length prefix (`FrameSocket::send_frame`).
        let mut first = server.next().await.expect("frame").expect("ok").into_data().to_vec();
        assert!(first.starts_with(&crate::socket::frame_socket::WA_CONN_HEADER));
        first.drain(..crate::socket::frame_socket::WA_CONN_HEADER.len());
        let length = (usize::from(first[0]) << 16) | (usize::from(first[1]) << 8) | usize::from(first[2]);
        let ciphertext = &first[3..3 + length];
        // The client sealed this frame with the zero session key (counter 0).
        let decrypted = {
            use aes_gcm::aead::Aead;
            use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
            let cipher = Aes256Gcm::new_from_slice(&[0u8; KEY_SIZE]).expect("key");
            cipher
                .decrypt(Nonce::from_slice(&crate::socket::noise_handshake::generate_iv(0)), ciphertext)
                .expect("decrypt request")
        };
        let request = crate::binary::unmarshal(&crate::binary::unpack(&decrypted)?)?;
        assert_eq!(request.tag, "iq");
        let id = request.attrs.get("id").and_then(AttrValue::as_str).unwrap().to_owned();
        let response = Node::new("iq").attr("id", id).attr("type", "result");
        // The reply must be sealed with the session write key (zero key here,
        // first frame → counter 0), or the client's NoiseSocket drops it as
        // undecryptable.
        let plaintext = crate::binary::marshal(&response);
        let sealed = seal(&[0u8; KEY_SIZE], 0, &plaintext);
        let bytes = length_prefixed(&sealed);
        server.send(WsMessage::Binary(bytes)).await?;
        anyhow::Result::<()>::Ok(())
    });

    fn seal(key: &[u8; KEY_SIZE], counter: u32, plaintext: &[u8]) -> Vec<u8> {
        use aes_gcm::aead::{Aead, Payload};
        use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
        let cipher = Aes256Gcm::new_from_slice(key).expect("key");
        cipher
            .encrypt(
                Nonce::from_slice(&crate::socket::noise_handshake::generate_iv(counter)),
                Payload { msg: plaintext, aad: &[] },
            )
            .expect("seal")
    }

    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client.send_iq(InfoQuery { namespace: "test".into(), kind: IqKind::Get, ..Default::default() }),
    )
    .await
    .expect("no hang")
    .expect("send_iq ok");
    assert_eq!(response.tag, "iq");
    assert_eq!(response.attrs.get("type").and_then(AttrValue::as_str), Some("result"));
    server_task.await.expect("server task").expect("server ok");
    pump.abort();
}

#[tokio::test]
async fn retry_frame_resends_the_bytes_and_returns_the_raw_response() {
    // Go `retryFrame` re-sends the marshalled frame and hands the node back
    // untouched: a retried `<message>` answers with an `<ack>`, not an `iq`.
    let client = test_client().await;
    let (frames_tx, mut frames_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let mut server = attach_noise_socket(&client, frames_tx).await;
    // `retry_frame` waits for a reconnect (`is_connected && is_logged_in`).
    client.is_logged_in.store(true, std::sync::atomic::Ordering::SeqCst);

    let pump = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            while let Some(data) = frames_rx.recv().await {
                let Ok(decompressed) = crate::binary::unpack(&data) else { continue };
                let Ok(node) = crate::binary::unmarshal(&decompressed) else { continue };
                if client.receive_response(node).await {
                    break;
                }
            }
        })
    };

    let frame = crate::binary::marshal(&Node::new("message").attr("id", "RETRY1").attr("type", "text"));
    let server_task = tokio::spawn(async move {
        let mut first = server.next().await.expect("frame").expect("ok").into_data().to_vec();
        first.drain(..crate::socket::frame_socket::WA_CONN_HEADER.len());
        let length = (usize::from(first[0]) << 16) | (usize::from(first[1]) << 8) | usize::from(first[2]);
        let decrypted = {
            use aes_gcm::aead::Aead;
            use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
            let cipher = Aes256Gcm::new_from_slice(&[0u8; KEY_SIZE]).expect("key");
            cipher
                .decrypt(Nonce::from_slice(&crate::socket::noise_handshake::generate_iv(0)), &first[3..3 + length])
                .expect("decrypt request")
        };
        let request = crate::binary::unmarshal(&crate::binary::unpack(&decrypted)?)?;
        assert_eq!(request.tag, "message", "the same frame goes out again");
        assert_eq!(request.attrs.get("id").and_then(AttrValue::as_str), Some("RETRY1"));
        let response = Node::new("ack").attr("id", "RETRY1").attr("class", "message");
        let plaintext = crate::binary::marshal(&response);
        let sealed = {
            use aes_gcm::aead::{Aead, Payload};
            use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
            let cipher = Aes256Gcm::new_from_slice(&[0u8; KEY_SIZE]).expect("key");
            cipher
                .encrypt(
                    Nonce::from_slice(&crate::socket::noise_handshake::generate_iv(0)),
                    Payload { msg: &plaintext, aad: &[] },
                )
                .expect("seal")
        };
        server.send(WsMessage::Binary(length_prefixed(&sealed))).await?;
        anyhow::Result::<()>::Ok(())
    });

    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client.retry_frame("message send", "RETRY1", &frame, &session::xml_stream_end_node(), Duration::from_secs(5)),
    )
    .await
    .expect("no hang")
    .expect("retry_frame ok");
    assert_eq!(response.tag, "ack", "retry_frame does not apply the iq check");
    assert_eq!(response.attrs.get("id").and_then(AttrValue::as_str), Some("RETRY1"));
    server_task.await.expect("server task").expect("server ok");
    pump.abort();
}

#[tokio::test]
async fn receive_response_consumes_only_matching_iq_or_ack() {
    let client = test_client().await;
    let waiter = client.register_waiter("abc.1-3").await;
    // A non-matching node is not consumed.
    assert!(!client.receive_response(Node::new("iq").attr("id", "other").attr("type", "result")).await);
    assert!(!client.receive_response(Node::new("message")).await);
    // The matching ack resolves the waiter.
    assert!(client.receive_response(Node::new("ack").attr("id", "abc.1-3")).await);
    let node = waiter.await.expect("waiter resolved");
    assert_eq!(node.tag, "ack");
    // The waiter is gone: a second node with the same id is not consumed.
    assert!(!client.receive_response(Node::new("ack").attr("id", "abc.1-3")).await);
}

#[tokio::test]
async fn clear_response_waiters_resolves_with_the_disconnect_node() {
    let client = test_client().await;
    let waiter = client.register_waiter("abc.1-9").await;
    client.clear_response_waiters(Some(session::xml_stream_end_node())).await;
    let node = waiter.await.expect("waiter resolved");
    assert!(session::is_disconnect_node(&node));
}

#[tokio::test]
async fn handler_queue_preserves_order_under_load() {
    use tokio::sync::mpsc;
    let (tx, mut rx) = mpsc::channel::<Node>(HANDLER_QUEUE_SIZE);
    // Fill the queue with a burst, then drain: order must be FIFO.
    for i in 0..100u32 {
        tx.send(Node::new("ib").attr("n", i.to_string())).await.expect("send");
    }
    drop(tx);
    let mut order = Vec::new();
    while let Some(node) = rx.recv().await {
        order.push(node.attrs.get("n").and_then(AttrValue::as_str).unwrap().to_owned());
    }
    assert_eq!(order.len(), 100);
    assert!(order.windows(2).all(|w| w[0].parse::<u32>().unwrap() < w[1].parse::<u32>().unwrap()));
}

#[tokio::test]
async fn reconnect_backoff_is_capped() {
    let config = ReconnectConfig::default();
    assert_eq!(config.delay_for(0), Duration::from_secs(2));
    assert_eq!(config.delay_for(1), Duration::from_secs(4));
    assert_eq!(config.delay_for(100), Duration::from_secs(60));
}

#[test]
fn is_retryable_connect_error_splits_correctly() {
    assert!(is_retryable_connect_error(&ClientError::HandshakeTimeout));
    assert!(is_retryable_connect_error(&ClientError::SocketClosed));
    assert!(!is_retryable_connect_error(&ClientError::NotLoggedIn));
    assert!(!is_retryable_connect_error(&ClientError::DeviceDeleted));
    assert!(!is_retryable_connect_error(&ClientError::AlreadyConnected));
}

#[test]
fn known_nodes_cover_the_phase_3_dispatch_table() {
    for tag in ["success", "failure", "stream:error", "iq", "ib"] {
        assert!(is_known_node(tag), "{tag} must be dispatched");
    }
    assert!(!is_known_node("definitely-not-a-node"));
}

#[test]
fn server_jid_is_bare() {
    let jid = server_jid();
    assert_eq!(jid.user, "");
    assert_eq!(jid.server, crate::types::jid::DEFAULT_USER_SERVER);
    assert_eq!(Jid::default(), Jid::new("", ""));
}

// ---------------------------------------------------------------------------
// Task B gate tests — pairing.
// ---------------------------------------------------------------------------

/// Go-generated vector (`/tmp/pairvecmod/main.go`, `ecc.CalculateSignature`
/// over `[6, 0] ‖ details ‖ ikp.pub`, self-verified with `ecc.VerifySignature`).
const VECTOR_IKP_PRIV: &str = "8855732a03e40ca737bddd285d86f21de292ad06c272cb6e0fa6b448c9ca1e6d";
const VECTOR_IKP_PUB: &str = "573efd6821922e519a776b9a746a78ab0c80a210af7da37085dd32f36511bc65";
const VECTOR_IDENTITY: &str = "0a1d746573742d64657461696c732d62797465732d303132333435363738391220cdbbd11ebc52df3edee8085da576051e6fd92bd21cc773102f179d3a61c8157e1a408ab37231511f5c341a4575bb452ba645faa5fe74c2b725ad5c0179197132fc19b18424c3127e6f70e86ec4fd13d117be378a7ff0366ab06a54bd10815893f788";

fn hex_bytes(hex: &str) -> Vec<u8> {
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect()
}

#[test]
fn verify_account_signature_accepts_the_go_vector() {
    let ikp_priv: [u8; 32] = hex_bytes(VECTOR_IKP_PRIV).try_into().unwrap();
    let ikp_pub: [u8; 32] = hex_bytes(VECTOR_IKP_PUB).try_into().unwrap();
    let identity: AdvSignedDeviceIdentity =
        prost::Message::decode(hex_bytes(VECTOR_IDENTITY).as_slice()).expect("decode vector");
    let ikp = KeyPair::from_private(ikp_priv);
    assert_eq!(ikp.public, ikp_pub);
    assert!(verify_account_signature(&identity, &ikp, false));
    // A tampered signature must fail.
    let mut bad = identity.clone();
    let mut sig = bad.account_signature.clone().unwrap();
    sig[0] ^= 0xff;
    bad.account_signature = Some(sig);
    assert!(!verify_account_signature(&bad, &ikp, false));
    // Wrong lengths must fail.
    let mut short = identity.clone();
    short.account_signature_key = Some(vec![0u8; 16]);
    assert!(!verify_account_signature(&short, &ikp, false));
}

#[test]
fn generate_device_signature_verifies_under_the_device_prefix() {
    let ikp = KeyPair::generate();
    let identity = AdvSignedDeviceIdentity {
        details: Some(b"details".to_vec()),
        account_signature_key: Some(ikp.public.to_vec()),
        ..Default::default()
    };
    let signature = generate_device_signature(&identity, &ikp);
    let message = concat_bytes(&[
        super::pair::ADV_DEVICE_SIGNATURE_PREFIX.as_slice(),
        b"details",
        ikp.public.as_slice(),
        ikp.public.as_slice(),
    ]);
    assert!(verify_signature(&ikp.public, &message, &signature));
}

#[test]
fn pair_client_type_wire_values() {
    use super::pair::PairClientType;
    assert_eq!(PairClientType::Chrome.as_str(), "1");
    assert_eq!(PairClientType::Unknown.as_str(), "0");
    assert_eq!(PairClientType::MacOS.as_str(), "c");
    assert_eq!(PairClientType::Android.as_str(), "e");
    assert_eq!(PairClientType::from_user_agent_platform(1).as_str(), "1");
    assert_eq!(PairClientType::from_user_agent_platform(999).as_str(), "0");
}

#[tokio::test]
async fn qr_channel_rejects_connected_or_paired_clients() {
    let client = test_client().await;
    // Paired device → error.
    client.device.lock().await.id = Some(Jid::new("123", crate::types::jid::DEFAULT_USER_SERVER));
    assert!(client.qr_channel().await.is_err());
}

type Seen = Arc<std::sync::Mutex<Vec<crate::WaEvent>>>;

/// A client over an in-memory container plus a collector for its events.
async fn collecting_client() -> (Arc<Client>, Container, Seen) {
    let seen: Seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let events: EventSink = Arc::new(move |event: crate::WaEvent| {
        sink.lock().expect("events lock").push(event);
    });
    let container = Container::open_memory().await.expect("memory");
    let client = Client::new(container.new_device(), ClientConfig::default(), events);
    (client, container, seen)
}

/// Read the `counter`-th frame the client sent (zero session key) and decode it.
async fn read_client_node(server: &mut WebSocketStream<tokio::net::TcpStream>, counter: u32) -> Node {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    let mut data = tokio::time::timeout(Duration::from_secs(5), server.next())
        .await
        .expect("frame in time")
        .expect("frame")
        .expect("ok")
        .into_data()
        .to_vec();
    if counter == 0 {
        assert!(data.starts_with(&crate::socket::frame_socket::WA_CONN_HEADER));
        data.drain(..crate::socket::frame_socket::WA_CONN_HEADER.len());
    }
    let length = (usize::from(data[0]) << 16) | (usize::from(data[1]) << 8) | usize::from(data[2]);
    let cipher = Aes256Gcm::new_from_slice(&[0u8; KEY_SIZE]).expect("key");
    let plain = cipher
        .decrypt(Nonce::from_slice(&crate::socket::noise_handshake::generate_iv(counter)), &data[3..3 + length])
        .expect("decrypt");
    crate::binary::unmarshal(&crate::binary::unpack(&plain).expect("unpack")).expect("unmarshal")
}

/// A `pair-success` iq the way the server sends it: account-signed identity
/// wrapped in an HMAC container keyed with `adv_secret` (or a wrong key).
fn pair_success_iq(identity_key: &KeyPair, adv_secret: &[u8]) -> Node {
    use hmac::{Hmac, Mac};
    use prost::Message;
    use wa_proto::wa_adv::{AdvDeviceIdentity, AdvSignedDeviceIdentityHmac};
    let account = KeyPair::generate();
    let details = AdvDeviceIdentity { raw_id: Some(1), key_index: Some(7), ..Default::default() }.encode_to_vec();
    let message =
        concat_bytes(&[super::pair::ADV_ACCOUNT_SIGNATURE_PREFIX.as_slice(), &details, identity_key.public.as_slice()]);
    let signature = crate::crypto::keys::calculate_signature(&account.private, &message, &[7u8; 64]);
    let signed = AdvSignedDeviceIdentity {
        details: Some(details),
        account_signature_key: Some(account.public.to_vec()),
        account_signature: Some(signature.to_vec()),
        ..Default::default()
    }
    .encode_to_vec();
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(adv_secret).expect("hmac key");
    mac.update(&signed);
    let container = AdvSignedDeviceIdentityHmac {
        details: Some(signed),
        hmac: Some(mac.finalize().into_bytes().to_vec()),
        ..Default::default()
    }
    .encode_to_vec();
    Node::new("iq").attr("from", server_jid()).attr("id", "pair-1").attr("type", "set").with_content(vec![
        Node::new("pair-success").with_content(vec![
            Node::new("device-identity").with_content(container),
            Node::new("device")
                .attr("jid", "4915123456789:12@s.whatsapp.net".parse::<Jid>().expect("jid"))
                .attr("lid", "123456789:12@lid".parse::<Jid>().expect("lid")),
            Node::new("platform").attr("name", "android"),
            Node::new("biz").attr("name", ""),
        ]),
    ])
}

#[tokio::test]
async fn iq_routing_emits_every_ref_as_qr_data() {
    let (client, _container, seen) = collecting_client().await;
    let refs =
        |n: usize| (0..n).map(|i| Node::new("ref").with_content(format!("ref-{i}").into_bytes())).collect::<Vec<_>>();
    // From a non-server JID, or with two children: ignored (Go `handleIQ`).
    let foreign = Node::new("iq")
        .attr("from", "123@s.whatsapp.net".parse::<Jid>().expect("jid"))
        .with_content(vec![Node::new("pair-device").with_content(refs(1))]);
    client.handle_node(&foreign).await;
    let two = Node::new("iq")
        .attr("from", server_jid())
        .with_content(vec![Node::new("pair-device"), Node::new("pair-device")]);
    client.handle_node(&two).await;
    assert!(seen.lock().expect("lock").is_empty());

    let iq = Node::new("iq")
        .attr("from", server_jid())
        .attr("id", "qr-1")
        .with_content(vec![Node::new("pair-device").with_content(refs(3))]);
    client.handle_node(&iq).await;
    // Clone the events out so the mutex guard is dropped before the awaits below.
    let events = seen.lock().expect("lock").clone();
    assert_eq!(events.len(), 1);
    let crate::WaEvent::Qr { codes } = &events[0] else { panic!("expected Qr, got {:?}", events[0]) };
    assert_eq!(codes.len(), 3);
    let device = client.device().await;
    for (i, code) in codes.iter().enumerate() {
        let expected = format!(
            "https://wa.me/settings/linked_devices#ref-{i},{},{},{},9",
            super::pair::base64_std(&device.noise_key.public),
            super::pair::base64_std(&device.identity_key.public),
            super::pair::base64_std(&device.adv_secret_key),
        );
        assert_eq!(code, &expected);
    }
}

#[test]
fn base64_std_matches_rfc4648_vectors() {
    use super::pair::base64_std;
    for (input, expected) in
        [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("foobar", "Zm9vYmFy")]
    {
        assert_eq!(base64_std(input.as_bytes()), expected);
    }
}

#[tokio::test]
async fn qr_client_type_is_other_web_client() {
    let client = test_client().await;
    assert_eq!(client.qr_client_type(), super::pair::PairClientType::OtherWebClient);
}

#[tokio::test]
async fn pair_success_persists_device() {
    use prost::Message;
    let (client, container, seen) = collecting_client().await;
    let (identity_key, adv_secret) = {
        let device = client.device.lock().await;
        (device.identity_key.clone(), device.adv_secret_key.clone())
    };
    let (frames_tx, _frames_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let mut server = attach_noise_socket(&client, frames_tx).await;

    client.handle_node(&pair_success_iq(&identity_key, &adv_secret)).await;

    // The confirmation: `iq type=result id=<req>` with the self-signed identity.
    let reply = read_client_node(&mut server, 0).await;
    assert_eq!(reply.tag, "iq");
    assert_eq!(reply.attrs.get("type").and_then(AttrValue::as_str), Some("result"));
    assert_eq!(reply.attrs.get("id").and_then(AttrValue::as_str), Some("pair-1"));
    assert_eq!(reply.attrs.get("to").and_then(AttrValue::as_jid), Some(&server_jid()));
    let sign = reply.optional_child_by_tag(&["pair-device-sign", "device-identity"]).expect("device-identity");
    assert_eq!(sign.attrs.get("key-index").map(ToString::to_string).as_deref(), Some("7"));
    let self_signed = AdvSignedDeviceIdentity::decode(sign.as_bytes().expect("bytes")).expect("decode");
    assert!(self_signed.account_signature_key.is_none(), "account key must be stripped");
    assert_eq!(self_signed.device_signature.as_ref().map(Vec::len), Some(64));

    // Persisted: in memory and in the container.
    let jid: Jid = "4915123456789:12@s.whatsapp.net".parse().expect("jid");
    let device = client.device().await;
    assert_eq!(device.id.as_ref(), Some(&jid));
    assert_eq!(device.lid.to_string(), "123456789:12@lid");
    assert_eq!(device.platform, "android");
    assert!(device.account.as_ref().and_then(|a| a.account_signature_key.as_ref()).is_some());
    let stored = container.get_device(&jid).await.expect("query").expect("stored");
    assert_eq!(stored.platform, "android");
    assert!(stored.account.is_some());

    assert!(client.paired.load(std::sync::atomic::Ordering::SeqCst));
    assert!(!client.is_logged_in(), "logged in only after the next `success`");
    assert!(client.is_expected_disconnect().await);
    let events = seen.lock().expect("lock");
    assert!(
        matches!(events.as_slice(), [crate::WaEvent::PairSuccess { id, platform, .. }] if id == &jid.to_string() && platform == "android"),
        "{events:?}"
    );
}

#[tokio::test]
async fn pair_success_hmac_mismatch_sends_401_and_does_not_persist() {
    let (client, container, seen) = collecting_client().await;
    let identity_key = client.device.lock().await.identity_key.clone();
    let (frames_tx, _frames_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let mut server = attach_noise_socket(&client, frames_tx).await;

    client.handle_node(&pair_success_iq(&identity_key, &[0xAA; 32])).await;

    let reply = read_client_node(&mut server, 0).await;
    assert_eq!(reply.attrs.get("type").and_then(AttrValue::as_str), Some("error"));
    assert_eq!(reply.attrs.get("id").and_then(AttrValue::as_str), Some("pair-1"));
    let error = reply.optional_child_by_tag(&["error"]).expect("error child");
    assert_eq!(error.attrs.get("code").map(ToString::to_string).as_deref(), Some("401"));
    assert_eq!(error.attrs.get("text").and_then(AttrValue::as_str), Some("hmac-mismatch"));

    assert!(client.device().await.id.is_none());
    let jid: Jid = "4915123456789:12@s.whatsapp.net".parse().expect("jid");
    assert!(container.get_device(&jid).await.expect("query").is_none());
    assert!(!client.paired.load(std::sync::atomic::Ordering::SeqCst));
    let events = seen.lock().expect("lock");
    assert!(
        matches!(events.as_slice(), [crate::WaEvent::PairError { error, .. }] if error.contains("HMAC")),
        "{events:?}"
    );
}

#[tokio::test]
async fn rotate_adv_secret_reemits_codes_with_the_new_secret() {
    let (client, _container, seen) = collecting_client().await;
    let iq = Node::new("iq")
        .attr("from", server_jid())
        .with_content(vec![Node::new("pair-device").with_content(vec![Node::new("ref").with_content(b"r".to_vec())])]);
    client.handle_node(&iq).await;
    let old = client.device().await.adv_secret_key;
    client.rotate_adv_secret().await.expect("rotate");
    let new = client.device().await.adv_secret_key;
    assert_ne!(old, new);
    let events = seen.lock().expect("lock");
    assert_eq!(events.len(), 2);
    let crate::WaEvent::Qr { codes } = &events[1] else { panic!("expected Qr") };
    assert!(codes[0].ends_with(&format!(",{},9", super::pair::base64_std(&new))));
}

#[test]
fn qr_rotation_policy_constants_match_go() {
    assert_eq!(QR_CHANNEL_TIMEOUT, Duration::from_secs(20));
    assert_eq!(QR_ROTATION_LIMIT, 3);
}

// ---------------------------------------------------------------------------
// Task C gate tests — the incoming message path (no Signal layer needed).
// ---------------------------------------------------------------------------

use crate::types::jid::{DEFAULT_USER_SERVER, HIDDEN_USER_SERVER};

/// A client whose device looks paired (`own_id`/`own_lid` are needed by
/// `parse_message_source`), plus the event collector.
async fn paired_collecting_client() -> (Arc<Client>, Seen) {
    let (client, _container, seen) = collecting_client().await;
    {
        let mut device = client.device.lock().await;
        device.id = Some(Jid::new("48111111111", DEFAULT_USER_SERVER));
        device.lid = Jid::new("99999999999", HIDDEN_USER_SERVER);
    }
    (client, seen)
}

fn group_message_node() -> Node {
    Node::new("message")
        .attr("id", "3EB0C767D097B7C7C030")
        .attr("from", "120363000000000001@g.us")
        .attr("participant", "48123456789:7@s.whatsapp.net")
        .attr("participant_pn", "48123456789@s.whatsapp.net")
        .attr("t", "1700000000")
        .attr("server_id", 77i64)
        .attr("notify", "Alice")
        .attr("category", "peer")
        .attr("type", "text")
        .attr("addressing_mode", "lid")
}

#[tokio::test]
async fn parse_message_info_reads_every_source_field() {
    let (client, _seen) = paired_collecting_client().await;
    let info = client.parse_message_info(&group_message_node()).await.expect("parse");

    assert_eq!(info.id.as_str(), "3EB0C767D097B7C7C030");
    assert_eq!(info.chat.to_string(), "120363000000000001@g.us");
    assert_eq!(info.sender.to_string(), "48123456789:7@s.whatsapp.net");
    // Go backfills the sender alt's device from the sender (`device == 0`).
    assert_eq!(info.sender_alt.to_string(), "48123456789:7@s.whatsapp.net");
    assert!(info.is_group);
    assert!(!info.is_from_me);
    assert_eq!(info.push_name, "Alice");
    assert_eq!(info.category, "peer");
    assert_eq!(info.message_type, "text");
    assert_eq!(info.timestamp, 1_700_000_000);
    assert_eq!(info.server_id.0, 77);
    assert_eq!(info.addressing_mode, crate::types::message::AddressingMode::Lid);
    assert_eq!(info.source_string(), "48123456789:7@s.whatsapp.net in 120363000000000001@g.us");
}

#[tokio::test]
async fn parse_message_info_detects_own_direct_message() {
    let (client, _seen) = paired_collecting_client().await;
    let node = Node::new("message")
        .attr("id", "SELF1")
        .attr("from", "48111111111@s.whatsapp.net")
        .attr("recipient", "4915112345678@s.whatsapp.net")
        .attr("peer_recipient_lid", "12345@lid")
        .attr("t", "1700000001")
        .attr("category", "peer");
    let info = client.parse_message_info(&node).await.expect("parse");
    assert!(info.is_from_me);
    assert!(!info.is_group);
    assert_eq!(info.chat.to_string(), "4915112345678@s.whatsapp.net");
    assert_eq!(info.sender.to_string(), "48111111111@s.whatsapp.net");
    assert_eq!(info.recipient_alt.to_string(), "12345@lid");
}

#[tokio::test]
async fn parse_message_info_rejects_missing_attributes() {
    let (client, _seen) = paired_collecting_client().await;
    // No `from`.
    let err = client.parse_message_info(&Node::new("message").attr("id", "x")).await;
    assert!(matches!(err, Err(ClientError::MessageParse(_))), "got {err:?}");
    // Group message without `participant`.
    let no_participant = Node::new("message").attr("id", "x").attr("from", "120363@g.us");
    let err = client.parse_message_info(&no_participant).await;
    assert!(matches!(err, Err(ClientError::MessageParse(_))), "got {err:?}");
    // No `id`.
    let no_id = Node::new("message").attr("from", "48123456789@s.whatsapp.net");
    let err = client.parse_message_info(&no_id).await;
    assert!(matches!(err, Err(ClientError::MessageParse(_))), "got {err:?}");
}

#[tokio::test]
async fn send_ack_matches_the_go_ack_shape() {
    let client = test_client().await;
    let (frames_tx, _frames_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let mut server = attach_noise_socket(&client, frames_tx).await;

    let node = Node::new("message")
        .attr("id", "ACK1")
        .attr("from", "120363@g.us")
        .attr("participant", "48123456789@s.whatsapp.net")
        .attr("recipient", "48111111111@s.whatsapp.net")
        .attr("type", "text");
    client.send_ack(&node, Some(super::ack::NACK_PARSING_ERROR)).await.expect("ack");
    let ack = read_client_node(&mut server, 0).await;
    assert_eq!(ack.tag, "ack");
    assert_eq!(ack.attrs.get("class").map(|v| v.to_string()), Some("message".to_owned()));
    assert_eq!(ack.attrs.get("id").map(|v| v.to_string()), Some("ACK1".to_owned()));
    assert_eq!(ack.attrs.get("to").map(|v| v.to_string()), Some("120363@g.us".to_owned()));
    assert_eq!(ack.attrs.get("participant").map(|v| v.to_string()), Some("48123456789@s.whatsapp.net".to_owned()));
    assert_eq!(ack.attrs.get("recipient").map(|v| v.to_string()), Some("48111111111@s.whatsapp.net".to_owned()));
    assert_eq!(ack.attrs.get("error").map(|v| v.to_string()), Some("487".to_owned()));
    // Go drops the `type` attribute for `<message>` nodes only.
    assert!(!ack.attrs.contains_key("type"));

    // A success ack for another class keeps `type` and has no `error`.
    let status =
        Node::new("status").attr("id", "STAT1").attr("from", "48123456789@s.whatsapp.net").attr("type", "media");
    client.send_ack(&status, None).await.expect("ack");
    let ack = read_client_node(&mut server, 1).await;
    assert_eq!(ack.attrs.get("class").map(|v| v.to_string()), Some("status".to_owned()));
    assert_eq!(ack.attrs.get("type").map(|v| v.to_string()), Some("media".to_owned()));
    assert!(!ack.attrs.contains_key("error"));
}

#[tokio::test]
async fn send_retry_receipt_sends_the_go_receipt_node() {
    let client = test_client().await;
    let (frames_tx, _frames_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let mut server = attach_noise_socket(&client, frames_tx).await;
    {
        let mut device = client.device.lock().await;
        device.id = Some(Jid::new("48111111111", DEFAULT_USER_SERVER));
    }
    let registration_id = client.device.lock().await.registration_id;

    let node = Node::new("message")
        .attr("id", "RETRY1")
        .attr("from", "120363@g.us")
        .attr("participant", "48123456789@s.whatsapp.net")
        .attr("t", "1700000000")
        .with_content(vec![Node::new("enc").attr("type", "pkmsg").attr("v", 2i64).with_content(vec![1u8, 2, 3])]);

    // First retry: no `<keys>` child.
    client.send_retry_receipt(&node).await.expect("retry receipt 1");
    let receipt = read_client_node(&mut server, 0).await;
    assert_eq!(receipt.tag, "receipt");
    assert_eq!(receipt.attrs.get("id").map(|v| v.to_string()), Some("RETRY1".to_owned()));
    assert_eq!(receipt.attrs.get("to").map(|v| v.to_string()), Some("120363@g.us".to_owned()));
    assert_eq!(receipt.attrs.get("type").map(|v| v.to_string()), Some("retry".to_owned()));
    assert_eq!(receipt.attrs.get("participant").map(|v| v.to_string()), Some("48123456789@s.whatsapp.net".to_owned()));
    let retry = receipt.optional_child_by_tag(&["retry"]).expect("<retry> child");
    assert_eq!(retry.attrs.get("count").map(|v| v.to_string()), Some("1".to_owned()));
    assert_eq!(retry.attrs.get("id").map(|v| v.to_string()), Some("RETRY1".to_owned()));
    assert_eq!(retry.attrs.get("t").map(|v| v.to_string()), Some("1700000000".to_owned()));
    assert_eq!(retry.attrs.get("v").map(|v| v.to_string()), Some("1".to_owned()));
    let registration = receipt.optional_child_by_tag(&["registration"]).expect("<registration> child");
    assert_eq!(registration.as_bytes(), Some(registration_id.to_be_bytes().as_slice()));
    assert!(receipt.optional_child_by_tag(&["keys"]).is_none(), "first retry carries no keys");

    // Second retry: the sender needs our identity bundle again.
    client.send_retry_receipt(&node).await.expect("retry receipt 2");
    let receipt = read_client_node(&mut server, 1).await;
    assert_eq!(
        receipt.optional_child_by_tag(&["retry"]).unwrap().attrs.get("count").map(|v| v.to_string()),
        Some("2".to_owned())
    );
    let keys = receipt.optional_child_by_tag(&["keys"]).expect("<keys> child");
    assert!(keys.optional_child_by_tag(&["type"]).is_some());
    assert!(keys.optional_child_by_tag(&["identity"]).is_some());
    assert!(keys.optional_child_by_tag(&["skey"]).is_some());
    assert_eq!(client.retry_count_for("RETRY1").await, 2);
}

#[tokio::test]
async fn retry_receipts_stop_at_the_go_limit() {
    let (client, _seen) = paired_collecting_client().await;
    let node = Node::new("message")
        .attr("id", "CAP1")
        .attr("from", "48123456789@s.whatsapp.net")
        .attr("t", "1700000000")
        .with_content(vec![Node::new("enc").attr("type", "msg")]);
    // No socket: every send fails, but the counter still advances (Go does the
    // same) and the receipt is dropped at the limit.
    for _ in 0..(super::retry_receipt::MAX_MESSAGE_RETRIES - 1) {
        let _ = client.send_retry_receipt(&node).await;
    }
    assert_eq!(client.retry_count_for("CAP1").await, super::retry_receipt::MAX_MESSAGE_RETRIES - 1);
    // Go increments first and then bails out at the limit, so the counter keeps
    // growing while no further receipt is sent.
    client.send_retry_receipt(&node).await.expect("fifth retry is the last one sent");
    assert_eq!(client.retry_count_for("CAP1").await, super::retry_receipt::MAX_MESSAGE_RETRIES);
    client.send_retry_receipt(&node).await.expect("capped send is a no-op");
    assert_eq!(client.retry_count_for("CAP1").await, super::retry_receipt::MAX_MESSAGE_RETRIES + 1);
    client.clear_retry_count("CAP1").await;
    assert_eq!(client.retry_count_for("CAP1").await, 0);
}

#[tokio::test]
async fn unknown_enc_type_emits_undecryptable_message() {
    let (client, seen) = paired_collecting_client().await;
    let node = Node::new("message")
        .attr("id", "UNK1")
        .attr("from", "48123456789@s.whatsapp.net")
        .attr("t", "1700000000")
        .with_content(vec![Node::new("enc").attr("type", "weirdmsg").with_content(vec![9u8])]);
    assert!(client.handle_message_node(&node).await);
    let events = seen.lock().expect("events lock").clone();
    assert_eq!(events.len(), 1);
    let crate::WaEvent::UndecryptableMessage { info, is_unavailable, unavailable_type, decrypt_fail_mode } = &events[0]
    else {
        panic!("expected UndecryptableMessage, got {:?}", events[0]);
    };
    assert_eq!(info.id.as_str(), "UNK1");
    assert!(!is_unavailable);
    assert!(unavailable_type.is_empty());
    assert_eq!(decrypt_fail_mode, "decrypt");
}

#[tokio::test]
async fn garbage_signal_payloads_are_undecryptable() {
    let (client, seen) = paired_collecting_client().await;
    for (enc_type, expected_mode) in [("pkmsg", "decrypt"), ("msg", "decrypt")] {
        let node = Node::new("message")
            .attr("id", format!("STUB-{enc_type}"))
            .attr("from", "48123456789@s.whatsapp.net")
            .attr("t", "1700000000")
            .with_content(vec![Node::new("enc").attr("type", enc_type).with_content(vec![1u8])]);
        client.handle_message_node(&node).await;
        let events = seen.lock().expect("events lock").clone();
        let last = events.last().expect("event");
        let crate::WaEvent::UndecryptableMessage { decrypt_fail_mode, is_unavailable, .. } = last else {
            panic!("expected UndecryptableMessage, got {last:?}");
        };
        assert_eq!(decrypt_fail_mode, expected_mode, "enc type {enc_type}");
        assert!(!is_unavailable);
    }
}

#[tokio::test]
async fn group_skmsg_is_reported_as_a_sender_key_failure() {
    let (client, seen) = paired_collecting_client().await;
    {
        // Unsaved devices run on `NoopStore`, which fails every lookup.
        let mut device = client.device.lock().await;
        device.account = Some(AdvSignedDeviceIdentity {
            details: Some(b"details".to_vec()),
            account_signature_key: Some(vec![1; 32]),
            account_signature: Some(vec![2; 64]),
            device_signature: Some(vec![3; 64]),
        });
        device.save().await.expect("save device");
    }
    // A well-formed sender-key message whose sender we have no key for.
    let message = crate::crypto::signal::SenderKeyMessage::new(1, 0, vec![0; 16], &[3u8; 32]);
    let node = group_message_node()
        .with_content(vec![Node::new("enc").attr("type", "skmsg").attr("v", "2").with_content(message.serialized)]);
    client.handle_node(&node).await;
    let events = seen.lock().expect("events lock").clone();
    let crate::WaEvent::UndecryptableMessage { decrypt_fail_mode, info, .. } = &events[0] else {
        panic!("expected UndecryptableMessage, got {:?}", events[0]);
    };
    assert_eq!(decrypt_fail_mode, "sender_key");
    assert!(info.is_group);
}

#[tokio::test]
async fn unavailable_message_without_enc_is_reported_as_unavailable() {
    let (client, seen) = paired_collecting_client().await;
    let node = Node::new("message")
        .attr("id", "UNAVAIL1")
        .attr("from", "48123456789@s.whatsapp.net")
        .attr("t", "1700000000")
        .with_content(vec![Node::new("unavailable").attr("type", "view_once")]);
    client.handle_message_node(&node).await;
    let events = seen.lock().expect("events lock").clone();
    let crate::WaEvent::UndecryptableMessage { is_unavailable, unavailable_type, .. } = &events[0] else {
        panic!("expected UndecryptableMessage, got {:?}", events[0]);
    };
    assert!(is_unavailable);
    assert_eq!(unavailable_type, "view_once");
}

#[tokio::test]
async fn handle_node_routes_status_and_rejects_unknown_classes() {
    let (client, _seen) = paired_collecting_client().await;
    // `status` is owned by the message path.
    assert!(client.handle_message_node(&Node::new("status").attr("id", "S1")).await);
    // Anything else is not.
    assert!(!client.handle_message_node(&Node::new("receipt").attr("id", "R1")).await);
}

#[tokio::test]
async fn plaintext_newsletter_message_emits_a_message_event() {
    use prost::Message as _;
    let (client, seen) = paired_collecting_client().await;
    let body = crate::types::message::WaMessage { conversation: Some("newsletter hello".into()), ..Default::default() }
        .encode_to_vec();
    let node = Node::new("message")
        .attr("id", "NL1")
        .attr("from", "120363000000000001@newsletter")
        .attr("t", "1700000000")
        .with_content(vec![Node::new("plaintext").with_content(body)]);
    client.handle_message_node(&node).await;
    let events = seen.lock().expect("events lock").clone();
    let crate::WaEvent::Message { info, text, is_ephemeral, .. } = &events[0] else {
        panic!("expected Message, got {:?}", events[0]);
    };
    assert_eq!(info.id.as_str(), "NL1");
    assert_eq!(text.as_deref(), Some("newsletter hello"));
    assert!(!is_ephemeral);
}

#[tokio::test]
async fn auto_trust_identity_and_event_buffer_defaults_match_go() {
    let client = test_client().await;
    assert!(!client.is_auto_trust_identity(), "Go's zero value is false");
    assert!(!client.enable_decrypted_event_buffer());
    client.set_auto_trust_identity(true);
    assert!(client.is_auto_trust_identity());
}

/// The LID map is what lets a PN-addressed history blob join its
/// LID-addressed chat, and the `_alt` attributes of a live message are where
/// a companion sees the two halves of an identity together (Go
/// `StoreLIDPNMapping`).
#[tokio::test]
async fn a_lid_and_a_phone_number_are_remembered_as_one_person() {
    // A saved device: the LID map is a real store, not the noop one.
    let container = Container::open_memory().await.expect("memory container");
    let mut device = container.new_device();
    device.id = Some(Jid::parse("48513483482:16@s.whatsapp.net").expect("own pn"));
    device.account = Some(AdvSignedDeviceIdentity {
        details: Some(b"details".to_vec()),
        account_signature_key: Some(vec![1; 32]),
        account_signature: Some(vec![2; 64]),
        device_signature: Some(vec![3; 64]),
    });
    device.save().await.expect("save the device");
    let client = Client::new(device, ClientConfig::default(), test_events());
    let lid = Jid::parse("192680906768485@lid").expect("lid");
    let pn = Jid::parse("48600765779@s.whatsapp.net").expect("pn");
    let lids = { client.device.lock().await.lids.clone() };

    // Either order works — the caller does not have to know which is which.
    client.store_lid_pn_mapping(&lid, &pn).await;
    assert_eq!(lids.get_lid_for_pn(&pn).await.expect("lookup"), Some(lid.clone()));

    let other_lid = Jid::parse("135124049993983@lid").expect("lid");
    let other_pn = Jid::parse("48513483482@s.whatsapp.net").expect("pn");
    client.store_lid_pn_mapping(&other_pn, &other_lid).await;
    assert_eq!(lids.get_lid_for_pn(&other_pn).await.expect("lookup"), Some(other_lid));

    // The device index belongs to a session, not to the identity map.
    let device_lid = Jid::parse("777@lid").expect("lid");
    let device_pn = Jid::parse("48999:8@s.whatsapp.net").expect("pn");
    client.store_lid_pn_mapping(&device_lid, &device_pn).await;
    assert_eq!(
        lids.get_lid_for_pn(&Jid::parse("48999@s.whatsapp.net").unwrap()).await.expect("lookup"),
        Some(device_lid)
    );

    // Pairs that are not one of each are ignored, not guessed at.
    let group = Jid::parse("1203@g.us").expect("group");
    client.store_lid_pn_mapping(&lid, &group).await;
    client.store_lid_pn_mapping(&pn, &Jid::default()).await;
    client.store_lid_pn_mapping(&Jid::default(), &pn).await;
    assert_eq!(lids.get_lid_for_pn(&pn).await.expect("lookup"), Some(lid), "the real mapping is untouched");
}

/// Every `<enc>` node carries `v="2"`. A peer message builds its attributes
/// from nothing (no fan-out caller to set them), and the server answers
/// `<error code="479">` (`smax-invalid`) for an `<enc>` without `v` — which is
/// how "Fetch older messages from WhatsApp" failed after the LID fix.
#[test]
fn every_enc_node_is_version_two() {
    use crate::binary::node::{AttrValue, Attrs};
    use crate::client::send::enc_attrs;

    let peer = enc_attrs("msg", &Attrs::new());
    assert_eq!(peer.get("v").and_then(AttrValue::as_str), Some("2"));
    assert_eq!(peer.get("type").and_then(AttrValue::as_str), Some("msg"));

    // A pre-key message keeps its own type.
    assert_eq!(enc_attrs("pkmsg", &Attrs::new()).get("type").and_then(AttrValue::as_str), Some("pkmsg"));

    // The caller's attributes are copied on top, like Go's `maps.Copy`.
    let mut extra = Attrs::new();
    extra.insert("mediatype".to_owned(), AttrValue::Text("image".to_owned()));
    extra.insert("v".to_owned(), AttrValue::Text("3".to_owned()));
    let fanout = enc_attrs("msg", &extra);
    assert_eq!(fanout.get("mediatype").and_then(AttrValue::as_str), Some("image"));
    assert_eq!(fanout.get("v").and_then(AttrValue::as_str), Some("3"), "an explicit version wins");
}

/// A peer message (a history-sync request, an app-state key request) is
/// addressed to our own phone number but encrypted for our own **LID**
/// identity — that is where the Signal session with our other devices lives.
/// Encrypting for the PN address fails with `no signal session with
/// <own pn>`, which is how "Fetch older messages" failed in the field
/// (Go `preparePeerMessageNode`, `send.go:1113`).
#[tokio::test]
async fn a_peer_message_is_encrypted_for_our_own_lid() {
    let client = test_client().await;
    {
        let mut device = client.device.lock().await;
        device.id = Some(Jid::parse("48513483482:16@s.whatsapp.net").expect("own pn"));
        device.lid = Jid::parse("135124049993983:16@lid").expect("own lid");
    }

    // The usual `ToNonAD` form: device 0 is the primary phone.
    let own_pn = Jid::parse("48513483482@s.whatsapp.net").expect("pn");
    let identity = client.encryption_identity(&own_pn).await;
    assert_eq!(identity.to_string(), "135124049993983@lid", "the LID of our own account, primary device");

    // A device index is kept, so a request to one companion stays addressed.
    let own_device = Jid::parse("48513483482:8@s.whatsapp.net").expect("pn device");
    assert_eq!(client.encryption_identity(&own_device).await.to_string(), "135124049993983:8@lid");

    // A LID is already an encryption identity.
    let lid = Jid::parse("135124049993983@lid").expect("lid");
    assert_eq!(client.encryption_identity(&lid).await, lid);

    // Somebody else's PN is not our LID; with an empty LID map the PN stands.
    let other = Jid::parse("48999999999@s.whatsapp.net").expect("other pn");
    assert_eq!(client.encryption_identity(&other).await, other);
}

/// The fan-out covers our other devices but not the one doing the sending:
/// there is no session with ourselves, and the message is already here.
#[test]
fn the_fan_out_skips_only_this_device() {
    use crate::client::send::fan_out_devices;

    let own_pn = Jid::parse("48513483482:16@s.whatsapp.net").expect("own pn");
    let own_lid = Jid::parse("135124049993983:16@lid").expect("own lid");
    let devices: Vec<Jid> = [
        "6468271108175:35@lid",          // the recipient
        "48513483482:16@s.whatsapp.net", // us, by phone number
        "135124049993983:16@lid",        // us, by LID
        "135124049993983:8@lid",         // our phone
        "48513483482:17@s.whatsapp.net", // another of our web sessions
    ]
    .iter()
    .map(|jid| Jid::parse(jid).expect("jid"))
    .collect();

    let kept: Vec<String> = fan_out_devices(&devices, &own_pn, &own_lid).iter().map(Jid::to_string).collect();
    assert_eq!(
        kept,
        ["6468271108175:35@lid", "135124049993983:8@lid", "48513483482:17@s.whatsapp.net"],
        "our other devices stay — they are how the message reaches our phone"
    );

    // Without a known own identity nothing is dropped.
    assert_eq!(fan_out_devices(&devices, &Jid::default(), &Jid::default()).len(), devices.len());
}

/// Our own devices must resolve to our own LID **without** the LID map: the
/// map is empty until a message arrives, sessions are stored under the LID,
/// and a device that resolves to a PN finds no session and silently drops out
/// of the fan-out — the message then reaches the recipient but never our own
/// phone.
#[tokio::test]
async fn our_own_devices_resolve_without_the_lid_map() {
    let container = Container::open_memory().await.expect("memory container");
    let mut device = container.new_device();
    device.id = Some(Jid::parse("48513483482:16@s.whatsapp.net").expect("own pn"));
    device.lid = Jid::parse("135124049993983:16@lid").expect("own lid");
    device.account = Some(AdvSignedDeviceIdentity {
        details: Some(b"details".to_vec()),
        account_signature_key: Some(vec![1; 32]),
        account_signature: Some(vec![2; 64]),
        device_signature: Some(vec![3; 64]),
    });
    device.save().await.expect("save the device");
    let client = Client::new(device, ClientConfig::default(), test_events());

    let lids = { client.device.lock().await.lids.clone() };
    assert!(lids.get_lid_for_pn(&Jid::parse("48513483482@s.whatsapp.net").unwrap()).await.unwrap().is_none());

    for (pn, expected) in [
        ("48513483482@s.whatsapp.net", "135124049993983@lid"),
        ("48513483482:8@s.whatsapp.net", "135124049993983:8@lid"),
        ("48513483482:16@s.whatsapp.net", "135124049993983:16@lid"),
    ] {
        let resolved = client.encryption_identity(&Jid::parse(pn).unwrap()).await;
        assert_eq!(resolved.to_string(), expected, "{pn}");
    }

    // Somebody else's PN is still unknown — guessing would encrypt for the
    // wrong person.
    let stranger = Jid::parse("48600765779@s.whatsapp.net").expect("pn");
    assert_eq!(client.encryption_identity(&stranger).await, stranger);
}

/// The request that unlocks the address book: without the primary phone's
/// app-state sync keys every mutation stays unapplied, so the engine has to
/// ask for them (Go `requestAppStateKeys`).
#[test]
fn an_app_state_key_request_names_every_missing_key() {
    let message = Client::build_app_state_key_request(&[vec![1, 2, 3], vec![4, 5, 6]]);
    let protocol = message.protocol_message.as_deref().expect("a protocol message");
    assert_eq!(
        protocol.r#type(),
        wa_proto::wa_web_protobufs_e2e::protocol_message::Type::AppStateSyncKeyRequest,
        "the phone dispatches on the type"
    );
    let request = protocol.app_state_sync_key_request.as_ref().expect("a key request");
    assert_eq!(
        request.key_i_ds.iter().map(|id| id.key_id.clone().unwrap_or_default()).collect::<Vec<_>>(),
        [vec![1, 2, 3], vec![4, 5, 6]]
    );
}

/// A DM stanza must not carry `phash`: WhatsApp answers `smax-invalid`
/// (`<error code="479">`) for it, which is how the composer failed in the
/// field. A group stanza must carry it (Go `sendGroup`, `send.go:821`).
#[tokio::test]
async fn only_a_group_stanza_carries_the_participant_hash() {
    use crate::client::ack::attr_text;
    use crate::client::send::SendRequestExtra;
    use crate::types::message::{MessageID, text_message};

    let client = test_client().await;
    let id = MessageID::new("MSG1".to_owned());
    let message = text_message("hi");
    // An explicit device list skips the `usync` round trip.
    let extra = SendRequestExtra {
        devices: Some(vec![Jid::parse("48111:3@s.whatsapp.net").expect("device jid")]),
        ..Default::default()
    };

    let dm = client
        .prepare_message_node(&Jid::parse("48111@s.whatsapp.net").unwrap(), &id, &message, &[], &extra)
        .await
        .expect("a dm node");
    assert!(dm.phash.is_some(), "the hash is still computed for the ack comparison");
    assert_eq!(attr_text(&dm.node, "phash"), None, "a DM stanza with phash is rejected with 479");

    let group = client
        .prepare_message_node(&Jid::parse("1203@g.us").unwrap(), &id, &message, &[], &extra)
        .await
        .expect("a group node");
    assert_eq!(attr_text(&group.node, "phash").as_deref(), group.phash.as_deref());
    assert!(group.phash.is_some());
}

/// A DM to a LID carries `peer_recipient_pn` — the PN behind the LID from the
/// LID map (Go `SendMessage`, `send.go:335`). Without it the server answers
/// `smax-invalid` (`<error code="479">`), which is how the composer failed in
/// the field. `addressing_mode` stays off a DM stanza: Go sets it only for
/// groups (`send.go:317-323`).
#[tokio::test]
async fn a_lid_dm_carries_the_peer_recipient_pn() {
    use crate::client::ack::attr_text;
    use crate::client::send::SendRequestExtra;
    use crate::types::message::{MessageID, text_message};

    let client = test_client().await;
    let lid = Jid::parse("6468271108175@lid").expect("lid");
    let pn = Jid::parse("48111111111@s.whatsapp.net").expect("pn");
    // `new_device` leaves `lids` uninitialised; point it at a real map.
    {
        let container = Container::open_memory().await.expect("memory container");
        client.device.lock().await.lids = container.lid_map().clone();
    }
    client.store_lid_pn_mapping(&lid, &pn).await;
    let id = MessageID::new("MSG2".to_owned());
    let message = text_message("hi");
    let extra = SendRequestExtra {
        devices: Some(vec![Jid::parse("48111:[EMAIL]").expect("device jid")]),
        ..Default::default()
    };

    let dm = client.prepare_message_node(&lid, &id, &message, &[], &extra).await.expect("a dm node");
    assert_eq!(attr_text(&dm.node, "peer_recipient_pn").as_deref(), Some(pn.to_non_ad().to_string()).as_deref());
    assert_eq!(attr_text(&dm.node, "addressing_mode"), None, "a DM stanza carries no addressing_mode");
}
