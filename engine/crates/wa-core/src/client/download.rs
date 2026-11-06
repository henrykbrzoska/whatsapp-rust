// Ported from the Go client (`download.go`, `util/cbcutil`, `util/hkdfutil`),
// © Tulir Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Media download: fetch an attachment (or a history-sync blob) and decrypt it.
//!
//! Attachments never travel over the WebSocket. A message carries only a
//! `directPath`, a 32-byte `mediaKey` and two hashes; the bytes are an HTTPS
//! GET against one of the CDN hosts in the [media connection](super::mediaconn).
//!
//! The payload is AES-256-CBC with an HMAC-SHA256 tag, all four keys derived
//! from the one `mediaKey` with HKDF-SHA256 over a per-type "app info" string:
//!
//! ```text
//! HKDF-SHA256(mediaKey, salt = none, info = "WhatsApp Image Keys", 112 bytes)
//!   [ 0..16] iv          [16..48] cipher key
//!   [48..80] mac key     [80..112] ref key (unused here)
//! ```
//!
//! Three checks run before anything is handed back, in this order — a failure
//! of any of them means the bytes are not what the sender sent, so they are
//! dropped rather than stored:
//!
//! 1. `sha256(ciphertext ‖ mac) == fileEncSha256` — the download is intact;
//! 2. `hmac_sha256(macKey, iv ‖ ciphertext)[..10] == mac` — the ciphertext is
//!    authentic;
//! 3. `sha256(plaintext) == fileSha256` — the decryption is correct.
//!
//! The same path serves history-sync blobs ([`MediaType::History`]), which is
//! what makes "fetch older messages from WhatsApp" possible at all: the phone
//! answers such a request with an encrypted blob, not with messages.

use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::client::{Client, ClientError};
use crate::crypto::{cbc, hkdf};
use crate::socket::frame_socket::ORIGIN;

/// The HMAC tag at the end of an encrypted attachment.
const MEDIA_HMAC_LENGTH: usize = 10;
/// Total HKDF output: iv + cipher key + mac key + ref key.
const MEDIA_KEY_EXPANDED_LENGTH: u8 = 112;
/// How long one attempt may take, including the body.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// How many times one host is retried on a network/5xx failure.
const DOWNLOAD_ATTEMPTS: usize = 3;

/// The kind of media being downloaded — the HKDF "app info" string, which is
/// part of the key derivation, plus the `mms-type` the CDN expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    Image,
    Video,
    Audio,
    Document,
    /// A history-sync blob (`HistorySyncNotification`).
    History,
    /// An app-state patch blob (`ExternalBlobReference`).
    AppState,
    StickerPack,
    LinkThumbnail,
}

impl MediaType {
    /// The HKDF info string. These exact bytes are part of the key schedule,
    /// so they are protocol constants, not labels.
    pub fn app_info(self) -> &'static str {
        match self {
            Self::Image => "WhatsApp Image Keys",
            Self::Video => "WhatsApp Video Keys",
            Self::Audio => "WhatsApp Audio Keys",
            Self::Document => "WhatsApp Document Keys",
            Self::History => "WhatsApp History Keys",
            Self::AppState => "WhatsApp App State Keys",
            Self::StickerPack => "WhatsApp Sticker Pack Keys",
            Self::LinkThumbnail => "WhatsApp Link Thumbnail Keys",
        }
    }

    /// The `mms-type` query parameter of the download URL.
    pub fn mms_type(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Document => "document",
            Self::History => "md-msg-hist",
            Self::AppState => "md-app-state",
            Self::StickerPack => "sticker-pack",
            Self::LinkThumbnail => "thumbnail-link",
        }
    }

    /// The media type of a `waE2E` message field name, e.g. `imageMessage`.
    /// A sticker is encrypted with the image keys, like on the Go side.
    pub fn for_message_kind(kind: &str) -> Option<Self> {
        match kind {
            "image" | "sticker" => Some(Self::Image),
            "video" | "gif" => Some(Self::Video),
            "audio" | "ptt" | "voice" => Some(Self::Audio),
            "document" => Some(Self::Document),
            _ => None,
        }
    }
}

/// The four keys one `mediaKey` expands into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaKeys {
    pub iv: Vec<u8>,
    pub cipher_key: Vec<u8>,
    pub mac_key: Vec<u8>,
    pub ref_key: Vec<u8>,
}

/// `getMediaKeys` — HKDF-SHA256 over the media key with the type's app info.
pub fn media_keys(media_key: &[u8], media_type: MediaType) -> MediaKeys {
    let expanded = hkdf::sha256(media_key, &[], media_type.app_info().as_bytes(), MEDIA_KEY_EXPANDED_LENGTH);
    MediaKeys {
        iv: expanded[..16].to_vec(),
        cipher_key: expanded[16..48].to_vec(),
        mac_key: expanded[48..80].to_vec(),
        ref_key: expanded[80..].to_vec(),
    }
}

/// Everything needed to fetch and verify one attachment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Downloadable {
    /// Server path, always starting with `/` and already carrying a query.
    pub direct_path: String,
    /// The 32-byte key the four media keys derive from.
    pub media_key: Vec<u8>,
    /// `sha256` of the plaintext.
    pub file_sha256: Vec<u8>,
    /// `sha256` of the ciphertext including its HMAC tag.
    pub file_enc_sha256: Vec<u8>,
    /// Declared plaintext length (0 when the sender omitted it).
    pub file_length: u64,
    /// MIME type the sender declared, for serving the bytes back.
    pub mime_type: String,
}

impl Downloadable {
    /// `true` when the bytes on the CDN are the file itself.
    ///
    /// Channel (`@newsletter`) media is public, so it carries neither a media
    /// key nor a ciphertext hash. Both have to be missing: a key without a
    /// hash (or the other way round) is a malformed message, not plaintext.
    pub fn is_plaintext(&self) -> bool {
        self.media_key.is_empty() && self.file_enc_sha256.is_empty()
    }
}

/// Why a download failed. Separate from [`ClientError`] so a caller can tell
/// "the server says it is gone" from "this build is broken".
#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("the message carries no direct path")]
    NoDirectPath,
    #[error("media download path does not start with a slash: {0}")]
    BadDirectPath(String),
    #[error("the media key is {0} bytes, expected 32")]
    BadMediaKey(usize),
    #[error("media download failed with HTTP {0}")]
    Http(u16),
    #[error("media download failed: {0}")]
    Transport(String),
    #[error("the downloaded file is too short ({0} bytes)")]
    TooShort(usize),
    #[error("the download does not match fileEncSha256")]
    InvalidEncSha256,
    #[error("the download does not match its HMAC")]
    InvalidHmac,
    #[error("the decrypted file does not match fileSha256")]
    InvalidSha256,
    #[error("could not decrypt the file: {0}")]
    Decrypt(String),
    #[error("{0}")]
    Client(#[from] ClientError),
}

impl DownloadError {
    /// `true` when trying another host cannot help (the server answered, and
    /// its answer is final).
    fn final_answer(&self) -> bool {
        matches!(
            self,
            Self::Http(403 | 404 | 410)
                | Self::InvalidSha256
                | Self::NoDirectPath
                | Self::BadDirectPath(_)
                | Self::BadMediaKey(_)
        )
    }
}

impl Client {
    /// Download one attachment and return its plaintext bytes.
    ///
    /// Every CDN host from the media connection is tried in order; a `403` is
    /// retried once with a freshly queried media connection, because an
    /// expired auth token looks exactly like a rejected one.
    pub async fn download_media(&self, media: &Downloadable, media_type: MediaType) -> Result<Vec<u8>, DownloadError> {
        if media.direct_path.is_empty() {
            return Err(DownloadError::NoDirectPath);
        }
        if !media.direct_path.starts_with('/') {
            return Err(DownloadError::BadDirectPath(media.direct_path.clone()));
        }
        // A channel (`@newsletter`) post is not Signal-encrypted: it carries a
        // direct path and a plaintext hash, and no media key at all. Go tells
        // the two apart the same way, in `downloadAndDecrypt`.
        if !media.is_plaintext() && media.media_key.len() != 32 {
            return Err(DownloadError::BadMediaKey(media.media_key.len()));
        }

        let mut refreshed = false;
        loop {
            let conn = self.refresh_media_conn(refreshed).await?;
            let hosts: Vec<String> = conn.hostnames().map(str::to_owned).collect();
            let mut last: Option<DownloadError> = None;
            for host in &hosts {
                let url = download_url(host, &media.direct_path, &media.file_enc_sha256, media_type);
                match self.download_and_decrypt(&url, media, media_type).await {
                    Ok(data) => return Ok(data),
                    Err(err) if err.final_answer() => {
                        // A 403 on every host usually means a stale media
                        // connection: re-query once before giving up.
                        if matches!(err, DownloadError::Http(403)) && !refreshed {
                            last = Some(err);
                            break;
                        }
                        return Err(err);
                    }
                    Err(err) => {
                        tracing::warn!("media download from {host} failed: {err}; trying the next host");
                        last = Some(err);
                    }
                }
            }
            let err = last.unwrap_or(DownloadError::Transport("no media hosts available".to_owned()));
            if matches!(err, DownloadError::Http(403)) && !refreshed {
                refreshed = true;
                continue;
            }
            return Err(err);
        }
    }

    /// One host: GET, verify, decrypt.
    ///
    /// Unencrypted media (a channel post) skips the HMAC and the CBC step —
    /// there is no key and no tag appended, so the body *is* the file and the
    /// only check left is its plaintext hash (Go `downloadAndDecrypt`'s
    /// `mediaKey == nil && fileEncSHA256 == nil && mac == nil` branch).
    async fn download_and_decrypt(
        &self,
        url: &str,
        media: &Downloadable,
        media_type: MediaType,
    ) -> Result<Vec<u8>, DownloadError> {
        let body = self.http_get_with_retries(url).await?;
        if media.is_plaintext() {
            verify_sha256(&body, &media.file_sha256)?;
            return Ok(body);
        }
        let keys = media_keys(&media.media_key, media_type);
        let (ciphertext, mac) = split_mac(&body)?;
        verify_enc_sha256(&body, &media.file_enc_sha256)?;
        verify_media_hmac(&keys.iv, ciphertext, &keys.mac_key, mac)?;
        let plaintext =
            cbc::decrypt(&keys.cipher_key, &keys.iv, ciphertext).map_err(|e| DownloadError::Decrypt(e.to_string()))?;
        verify_sha256(&plaintext, &media.file_sha256)?;
        Ok(plaintext)
    }

    /// GET with a small retry budget for network and 5xx failures.
    async fn http_get_with_retries(&self, url: &str) -> Result<Vec<u8>, DownloadError> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.http_get(url).await {
                Ok(body) => return Ok(body),
                Err(err) => {
                    let retryable = matches!(err, DownloadError::Transport(_) | DownloadError::Http(500..=599));
                    if !retryable || attempt >= DOWNLOAD_ATTEMPTS {
                        return Err(err);
                    }
                    tokio::time::sleep(Duration::from_secs(attempt as u64)).await;
                }
            }
        }
    }

    /// The raw GET. Headers mirror a browser session, like the Go client.
    async fn http_get(&self, url: &str) -> Result<Vec<u8>, DownloadError> {
        let client = self.media_http();
        let mut request = client.get(url).header("Origin", ORIGIN).header("Referer", format!("{ORIGIN}/"));
        if let Some(agent) = &self.config.user_agent {
            request = request.header("User-Agent", agent.as_str());
        }
        let response = request.send().await.map_err(|e| DownloadError::Transport(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(DownloadError::Http(status.as_u16()));
        }
        let body = response.bytes().await.map_err(|e| DownloadError::Transport(e.to_string()))?;
        Ok(body.to_vec())
    }

    /// The shared HTTP client for media (connection pooling across downloads).
    fn media_http(&self) -> reqwest::Client {
        self.media_http
            .get_or_init(|| {
                reqwest::Client::builder().timeout(DOWNLOAD_TIMEOUT).build().unwrap_or_else(|_| reqwest::Client::new())
            })
            .clone()
    }
}

/// `https://{host}{directPath}&hash={b64url(encSha)}&mms-type={type}&__wa-mms=`
///
/// `directPath` already contains a query string, which is why the extra
/// parameters are appended with `&`.
pub fn download_url(host: &str, direct_path: &str, file_enc_sha256: &[u8], media_type: MediaType) -> String {
    format!(
        "https://{host}{direct_path}&hash={}&mms-type={}&__wa-mms=",
        base64_url(file_enc_sha256),
        media_type.mms_type()
    )
}

/// Split the HMAC tag off the end of the download.
fn split_mac(body: &[u8]) -> Result<(&[u8], &[u8]), DownloadError> {
    if body.len() <= MEDIA_HMAC_LENGTH {
        return Err(DownloadError::TooShort(body.len()));
    }
    Ok(body.split_at(body.len() - MEDIA_HMAC_LENGTH))
}

/// `sha256(ciphertext ‖ mac) == fileEncSha256` — an intact download.
fn verify_enc_sha256(body: &[u8], expected: &[u8]) -> Result<(), DownloadError> {
    if expected.is_empty() {
        return Ok(());
    }
    if expected.len() != 32 || Sha256::digest(body).as_slice() != expected {
        return Err(DownloadError::InvalidEncSha256);
    }
    Ok(())
}

/// `hmac_sha256(macKey, iv ‖ ciphertext)[..10] == mac` — an authentic payload.
fn verify_media_hmac(iv: &[u8], ciphertext: &[u8], mac_key: &[u8], mac: &[u8]) -> Result<(), DownloadError> {
    let mut hmac = <Hmac<Sha256> as Mac>::new_from_slice(mac_key).map_err(|_| DownloadError::InvalidHmac)?;
    hmac.update(iv);
    hmac.update(ciphertext);
    let computed = hmac.finalize().into_bytes();
    // Constant-time comparison of the truncated tag.
    let expected = &computed[..MEDIA_HMAC_LENGTH];
    if mac.len() != MEDIA_HMAC_LENGTH || !constant_time_eq(expected, mac) {
        return Err(DownloadError::InvalidHmac);
    }
    Ok(())
}

/// `sha256(plaintext) == fileSha256` — a correct decryption.
fn verify_sha256(plaintext: &[u8], expected: &[u8]) -> Result<(), DownloadError> {
    if expected.is_empty() {
        return Ok(());
    }
    if expected.len() != 32 || Sha256::digest(plaintext).as_slice() != expected {
        return Err(DownloadError::InvalidSha256);
    }
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `base64.URLEncoding` — URL-safe alphabet, with padding (Go's encoder for
/// the `hash` parameter). Hand-rolled like the other base64 helpers in this
/// crate, so no extra dependency is pulled in for 20 lines.
pub fn base64_url(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(ALPHABET[((n >> shift) & 0x3f) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The attachment of a `waE2E.Message`, if it has one.
///
/// This is what `GET /media/{id}` needs: the stored message keeps the raw
/// protobuf, and these are the fields that turn it back into a download.
pub fn downloadable_from_message(msg: &wa_proto::wa_web_protobufs_e2e::Message) -> Option<(Downloadable, MediaType)> {
    if let Some(m) = &msg.image_message {
        return Some((
            Downloadable {
                direct_path: m.direct_path.clone().unwrap_or_default(),
                media_key: m.media_key.clone().unwrap_or_default(),
                file_sha256: m.file_sha256.clone().unwrap_or_default(),
                file_enc_sha256: m.file_enc_sha256.clone().unwrap_or_default(),
                file_length: m.file_length.unwrap_or_default(),
                mime_type: m.mimetype.clone().unwrap_or_else(|| "image/jpeg".to_owned()),
            },
            MediaType::Image,
        ));
    }
    if let Some(m) = &msg.video_message {
        return Some((
            Downloadable {
                direct_path: m.direct_path.clone().unwrap_or_default(),
                media_key: m.media_key.clone().unwrap_or_default(),
                file_sha256: m.file_sha256.clone().unwrap_or_default(),
                file_enc_sha256: m.file_enc_sha256.clone().unwrap_or_default(),
                file_length: m.file_length.unwrap_or_default(),
                mime_type: m.mimetype.clone().unwrap_or_else(|| "video/mp4".to_owned()),
            },
            MediaType::Video,
        ));
    }
    if let Some(m) = &msg.audio_message {
        return Some((
            Downloadable {
                direct_path: m.direct_path.clone().unwrap_or_default(),
                media_key: m.media_key.clone().unwrap_or_default(),
                file_sha256: m.file_sha256.clone().unwrap_or_default(),
                file_enc_sha256: m.file_enc_sha256.clone().unwrap_or_default(),
                file_length: m.file_length.unwrap_or_default(),
                mime_type: m.mimetype.clone().unwrap_or_else(|| "audio/ogg".to_owned()),
            },
            MediaType::Audio,
        ));
    }
    if let Some(m) = &msg.document_message {
        return Some((
            Downloadable {
                direct_path: m.direct_path.clone().unwrap_or_default(),
                media_key: m.media_key.clone().unwrap_or_default(),
                file_sha256: m.file_sha256.clone().unwrap_or_default(),
                file_enc_sha256: m.file_enc_sha256.clone().unwrap_or_default(),
                file_length: m.file_length.unwrap_or_default(),
                mime_type: m.mimetype.clone().unwrap_or_else(|| "application/octet-stream".to_owned()),
            },
            MediaType::Document,
        ));
    }
    if let Some(m) = &msg.sticker_message {
        return Some((
            Downloadable {
                direct_path: m.direct_path.clone().unwrap_or_default(),
                media_key: m.media_key.clone().unwrap_or_default(),
                file_sha256: m.file_sha256.clone().unwrap_or_default(),
                file_enc_sha256: m.file_enc_sha256.clone().unwrap_or_default(),
                file_length: m.file_length.unwrap_or_default(),
                // Stickers are WebP and are encrypted with the image keys.
                mime_type: m.mimetype.clone().unwrap_or_else(|| "image/webp".to_owned()),
            },
            MediaType::Image,
        ));
    }
    None
}

/// [`downloadable_from_message`] over the stored payload bytes.
///
/// `bebok-server` keeps the raw `waE2E.Message` of every message (hex in
/// `wa-store.raw_json`) but does not speak protobuf — decoding stays here, in
/// the protocol crate.
pub fn downloadable_from_bytes(raw: &[u8]) -> Option<(Downloadable, MediaType)> {
    use prost::Message as _;
    let message = wa_proto::wa_web_protobufs_e2e::Message::decode(raw).ok()?;
    downloadable_from_message(&message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an encrypted attachment the way WhatsApp does, so the verify
    /// chain can be exercised without the network.
    fn encrypted(media_key: &[u8], media_type: MediaType, plaintext: &[u8]) -> (Vec<u8>, Downloadable) {
        let keys = media_keys(media_key, media_type);
        let ciphertext = cbc::encrypt_with_iv(&keys.cipher_key, &keys.iv, plaintext).expect("encrypt");
        let mut hmac = <Hmac<Sha256> as Mac>::new_from_slice(&keys.mac_key).unwrap();
        hmac.update(&keys.iv);
        hmac.update(&ciphertext);
        let mac = hmac.finalize().into_bytes()[..MEDIA_HMAC_LENGTH].to_vec();
        let mut body = ciphertext;
        body.extend_from_slice(&mac);
        let media = Downloadable {
            direct_path: "/v/t62.7118-24/123?ccb=11-4&oh=01_Q5&oe=68A".to_owned(),
            media_key: media_key.to_vec(),
            file_sha256: Sha256::digest(plaintext).to_vec(),
            file_enc_sha256: Sha256::digest(&body).to_vec(),
            file_length: plaintext.len() as u64,
            mime_type: "image/jpeg".to_owned(),
        };
        (body, media)
    }

    /// The whole verify + decrypt chain, as `download_and_decrypt` runs it.
    fn verify_and_decrypt(body: &[u8], media: &Downloadable, media_type: MediaType) -> Result<Vec<u8>, DownloadError> {
        let keys = media_keys(&media.media_key, media_type);
        let (ciphertext, mac) = split_mac(body)?;
        verify_enc_sha256(body, &media.file_enc_sha256)?;
        verify_media_hmac(&keys.iv, ciphertext, &keys.mac_key, mac)?;
        let plaintext =
            cbc::decrypt(&keys.cipher_key, &keys.iv, ciphertext).map_err(|e| DownloadError::Decrypt(e.to_string()))?;
        verify_sha256(&plaintext, &media.file_sha256)?;
        Ok(plaintext)
    }

    #[test]
    fn key_expansion_splits_into_iv_cipher_mac_ref() {
        let keys = media_keys(&[7u8; 32], MediaType::Image);
        assert_eq!((keys.iv.len(), keys.cipher_key.len(), keys.mac_key.len(), keys.ref_key.len()), (16, 32, 32, 32));
        // The app info is part of the schedule, so another type derives other keys.
        assert_ne!(keys.cipher_key, media_keys(&[7u8; 32], MediaType::Video).cipher_key);
        assert_ne!(keys.iv, media_keys(&[7u8; 32], MediaType::History).iv);
    }

    #[test]
    fn a_well_formed_attachment_round_trips() {
        let plaintext = b"the bytes of a tiny jpeg".repeat(7);
        let (body, media) = encrypted(&[3u8; 32], MediaType::Image, &plaintext);
        assert_eq!(verify_and_decrypt(&body, &media, MediaType::Image).expect("decrypts"), plaintext);
    }

    #[test]
    fn every_check_rejects_its_own_kind_of_tampering() {
        let plaintext = b"history blob".repeat(9);
        let (body, media) = encrypted(&[9u8; 32], MediaType::History, &plaintext);

        // A flipped ciphertext byte fails the enc hash first.
        let mut flipped = body.clone();
        flipped[2] ^= 0xff;
        assert!(matches!(
            verify_and_decrypt(&flipped, &media, MediaType::History),
            Err(DownloadError::InvalidEncSha256)
        ));

        // Tampering that keeps the declared enc hash still fails the HMAC.
        let mut forged = media.clone();
        forged.file_enc_sha256 = Sha256::digest(&flipped).to_vec();
        assert!(matches!(verify_and_decrypt(&flipped, &forged, MediaType::History), Err(DownloadError::InvalidHmac)));

        // The wrong media type derives the wrong keys → the HMAC fails.
        assert!(matches!(verify_and_decrypt(&body, &media, MediaType::Image), Err(DownloadError::InvalidHmac)));

        // A lie about the plaintext hash is caught after decryption.
        let mut lying = media.clone();
        lying.file_sha256 = vec![0u8; 32];
        assert!(matches!(verify_and_decrypt(&body, &lying, MediaType::History), Err(DownloadError::InvalidSha256)));

        // Truncated to the tag length or shorter: nothing to verify.
        assert!(matches!(verify_and_decrypt(&body[..8], &media, MediaType::History), Err(DownloadError::TooShort(8))));
    }

    #[test]
    fn the_download_url_appends_to_the_direct_path_query() {
        let url =
            download_url("mmg.whatsapp.net", "/v/t62/abc?ccb=11-4&oh=01_Q5", &[0xff, 0x00, 0xfe], MediaType::History);
        assert_eq!(
            url,
            "https://mmg.whatsapp.net/v/t62/abc?ccb=11-4&oh=01_Q5&hash=_wD-&mms-type=md-msg-hist&__wa-mms="
        );
    }

    #[test]
    fn base64_url_uses_the_url_safe_alphabet_with_padding() {
        // The two bytes that differ between the standard and URL alphabets.
        assert_eq!(base64_url(&[0xfb, 0xff, 0xbf]), "-_-_");
        assert_eq!(base64_url(b"a"), "YQ==");
        assert_eq!(base64_url(b"ab"), "YWI=");
        assert_eq!(base64_url(b"abc"), "YWJj");
        assert_eq!(base64_url(b""), "");
    }

    #[test]
    fn an_attachment_is_extracted_from_its_message() {
        use wa_proto::wa_web_protobufs_e2e::{ImageMessage, Message as WaMessage, StickerMessage};

        let msg = WaMessage {
            image_message: Some(Box::new(ImageMessage {
                direct_path: Some("/v/t62/abc?ccb=11".into()),
                media_key: Some(vec![4; 32]),
                file_sha256: Some(vec![5; 32]),
                file_enc_sha256: Some(vec![6; 32]),
                file_length: Some(2048),
                mimetype: Some("image/png".into()),
                ..Default::default()
            })),
            ..Default::default()
        };
        let (media, media_type) = downloadable_from_message(&msg).expect("an image");
        assert_eq!(media_type, MediaType::Image);
        assert_eq!(media.direct_path, "/v/t62/abc?ccb=11");
        assert_eq!((media.media_key.len(), media.file_length), (32, 2048));
        assert_eq!(media.mime_type, "image/png");

        // A sticker is image-encrypted and defaults to WebP.
        let sticker = WaMessage { sticker_message: Some(Box::new(StickerMessage::default())), ..Default::default() };
        let (sticker_media, sticker_type) = downloadable_from_message(&sticker).expect("a sticker");
        assert_eq!(sticker_type, MediaType::Image);
        assert_eq!(sticker_media.mime_type, "image/webp");

        // A plain text message has nothing to download.
        assert!(downloadable_from_message(&crate::types::message::text_message("hi")).is_none());

        // The same, straight from the stored payload bytes.
        use prost::Message as _;
        let (from_bytes, _) = downloadable_from_bytes(&msg.encode_to_vec()).expect("decodes");
        assert_eq!(from_bytes.direct_path, media.direct_path);
        assert!(downloadable_from_bytes(b"not a protobuf at all").is_none());
    }

    #[test]
    fn mms_types_and_app_info_match_the_protocol_constants() {
        assert_eq!(MediaType::History.mms_type(), "md-msg-hist");
        assert_eq!(MediaType::AppState.app_info(), "WhatsApp App State Keys");
        assert_eq!(MediaType::for_message_kind("sticker"), Some(MediaType::Image));
        assert_eq!(MediaType::for_message_kind("gif"), Some(MediaType::Video));
        assert_eq!(MediaType::for_message_kind("ptt"), Some(MediaType::Audio));
        assert_eq!(MediaType::for_message_kind("text"), None);
    }

    #[test]
    fn a_bad_direct_path_or_key_is_refused_before_any_request() {
        let media = Downloadable { direct_path: "v/no-slash".to_owned(), media_key: vec![1; 32], ..Default::default() };
        assert!(super::DownloadError::BadDirectPath(media.direct_path.clone()).final_answer());
        assert!(DownloadError::Http(404).final_answer());
        assert!(!DownloadError::Http(500).final_answer());
        assert!(!DownloadError::Transport("reset".to_owned()).final_answer());
    }

    /// Channel media is public: a direct path and a plaintext hash, no key.
    /// Everything else has to go through the HMAC and the CBC step.
    #[test]
    fn plaintext_media_is_the_one_without_a_key_or_a_ciphertext_hash() {
        let channel = Downloadable {
            direct_path: "/m1/v/t24/abc".to_owned(),
            file_sha256: vec![1; 32],
            file_length: 270_354,
            mime_type: "image/jpeg".to_owned(),
            ..Default::default()
        };
        assert!(channel.is_plaintext());

        let encrypted = Downloadable { media_key: vec![7; 32], file_enc_sha256: vec![2; 32], ..channel.clone() };
        assert!(!encrypted.is_plaintext());

        // Half a key pair is a malformed message, not plaintext — it must not
        // skip the integrity checks.
        assert!(!Downloadable { media_key: vec![7; 32], ..channel.clone() }.is_plaintext());
        assert!(!Downloadable { file_enc_sha256: vec![2; 32], ..channel }.is_plaintext());
    }
}
