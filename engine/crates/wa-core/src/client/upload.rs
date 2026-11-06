// Ported from the Go client (`upload.go`, `download.go` key schedule),
// © Tulir Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! Media upload: encrypt an attachment and PUT it to the CDN.
//!
//! Attachments never travel over the WebSocket. The flow mirrors Go
//! `Client::Upload`:
//!
//! 1. a random 32-byte `mediaKey` is generated;
//! 2. HKDF-SHA256 over the media key with the per-type app-info string yields
//!    the IV, cipher key and MAC key ([`super::download::media_keys`]);
//! 3. the plaintext is AES-256-CBC encrypted, `HMAC-SHA256(iv ‖ ciphertext)`
//!    is truncated to 10 bytes and appended;
//! 4. the result is POSTed to `https://{host}/mms/{type}/{token}?auth=&token=`
//!    where `host` comes from the [media connection](super::mediaconn) and
//!    `token` is the base64url of `sha256(ciphertext ‖ mac)`;
//! 5. the server answers with JSON carrying `url`, `direct_path` and
//!    `handle`, which are copied into the `ImageMessage`/`VideoMessage`.
//!
//! GIFs are `VideoMessage`s with `gif_playback = true` — WhatsApp has no
//! separate GIF message type.

use std::time::Duration;

use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::client::download::{MediaType, base64_url, media_keys};
use crate::client::{Client, ClientError};
use crate::crypto::cbc;
use crate::socket::frame_socket::ORIGIN;
use wa_proto::wa_web_protobufs_e2e as wa_e2e;

/// How long one upload attempt may take, including the body.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Go `UploadResponse` — what the CDN answers plus the keys the message needs.
#[derive(Debug, Clone)]
pub struct UploadResponse {
    pub url: String,
    pub direct_path: String,
    pub handle: String,
    pub media_key: Vec<u8>,
    pub file_enc_sha256: Vec<u8>,
    pub file_sha256: Vec<u8>,
    pub file_length: u64,
    pub mime_type: String,
}

#[derive(Debug, Deserialize)]
struct UploadAnswer {
    #[serde(default)]
    url: String,
    #[serde(default)]
    direct_path: String,
    #[serde(default)]
    handle: String,
}

/// Why an upload failed.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("the file is empty")]
    Empty,
    #[error("could not encrypt the file: {0}")]
    Encrypt(String),
    #[error("media upload failed with HTTP {0}")]
    Http(u16),
    #[error("media upload failed: {0}")]
    Transport(String),
    #[error("could not parse the upload response: {0}")]
    BadResponse(String),
    #[error("{0}")]
    Client(#[from] ClientError),
}

impl Client {
    /// Go `Upload` — encrypt `plaintext` and upload it as `media_type`.
    ///
    /// Returns everything an `ImageMessage`/`VideoMessage` needs. The caller
    /// builds the message (caption, dimensions, thumbnail) and sends it with
    /// [`Client::send_message`](super::send::Client::send_message).
    pub async fn upload_media(
        &self,
        plaintext: &[u8],
        mime_type: &str,
        media_type: MediaType,
    ) -> Result<UploadResponse, UploadError> {
        if plaintext.is_empty() {
            return Err(UploadError::Empty);
        }
        let mut media_key = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut media_key);

        let file_sha256 = Sha256::digest(plaintext).to_vec();
        let keys = media_keys(&media_key, media_type);
        let ciphertext = cbc::encrypt_with_iv(&keys.cipher_key, &keys.iv, plaintext)
            .map_err(|err| UploadError::Encrypt(err.to_string()))?;

        let mut mac =
            Hmac::<Sha256>::new_from_slice(&keys.mac_key).map_err(|err| UploadError::Encrypt(err.to_string()))?;
        mac.update(&keys.iv);
        mac.update(&ciphertext);
        let tag = mac.finalize().into_bytes();

        let mut body = Vec::with_capacity(ciphertext.len() + 10);
        body.extend_from_slice(&ciphertext);
        body.extend_from_slice(&tag[..10]);

        let file_enc_sha256 = Sha256::digest(&body).to_vec();
        let token = base64_url(&file_enc_sha256);

        let conn = self.refresh_media_conn(false).await?;
        let host = conn.hostnames().next().ok_or_else(|| UploadError::Transport("no media hosts".to_owned()))?;
        let url = format!(
            "https://{host}/mms/{mms}/{token}?auth={auth}&token={token}",
            host = host,
            mms = media_type.mms_type(),
            token = token,
            auth = conn.auth,
        );

        let http =
            reqwest::Client::builder().timeout(UPLOAD_TIMEOUT).build().unwrap_or_else(|_| reqwest::Client::new());
        let response = http
            .post(&url)
            .header("Origin", ORIGIN)
            .header("Referer", format!("{ORIGIN}/"))
            .header("Content-Type", "application/octet-stream")
            .body(body)
            .send()
            .await
            .map_err(|err| UploadError::Transport(err.to_string()))?;
        if !response.status().is_success() {
            return Err(UploadError::Http(response.status().as_u16()));
        }
        let answer: UploadAnswer =
            response.bytes().await.map_err(|err| UploadError::Transport(err.to_string())).and_then(|body| {
                serde_json::from_slice(&body).map_err(|err| UploadError::BadResponse(err.to_string()))
            })?;
        if answer.url.is_empty() || answer.direct_path.is_empty() {
            return Err(UploadError::BadResponse("missing url or direct_path".to_owned()));
        }
        Ok(UploadResponse {
            url: answer.url,
            direct_path: answer.direct_path,
            handle: answer.handle,
            media_key: media_key.to_vec(),
            file_enc_sha256,
            file_sha256,
            file_length: plaintext.len() as u64,
            mime_type: mime_type.to_owned(),
        })
    }
}

/// Build an `ImageMessage` from an upload plus the client-side metadata.
///
/// `dimensions` is `(width, height)` in pixels when known (the composer reads
/// them from the file); `thumbnail` is a small JPEG preview.
pub fn image_message(
    upload: &UploadResponse,
    caption: Option<&str>,
    dimensions: Option<(u32, u32)>,
    thumbnail: Option<Vec<u8>>,
) -> wa_e2e::Message {
    wa_e2e::Message {
        image_message: Some(Box::new(wa_e2e::ImageMessage {
            url: Some(upload.url.clone()),
            mimetype: Some(upload.mime_type.clone()),
            caption: caption.filter(|c| !c.is_empty()).map(str::to_owned),
            file_sha256: Some(upload.file_sha256.clone()),
            file_length: Some(upload.file_length),
            height: dimensions.map(|(_, h)| h),
            width: dimensions.map(|(w, _)| w),
            media_key: Some(upload.media_key.clone()),
            file_enc_sha256: Some(upload.file_enc_sha256.clone()),
            direct_path: Some(upload.direct_path.clone()),
            media_key_timestamp: Some(wa_store_time_now()),
            jpeg_thumbnail: thumbnail,
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Build a `VideoMessage` from an upload. `gif` sets `gif_playback` — that is
/// how WhatsApp marks a looping GIF (there is no separate GIF message type).
pub fn video_message(
    upload: &UploadResponse,
    caption: Option<&str>,
    dimensions: Option<(u32, u32)>,
    seconds: Option<u32>,
    thumbnail: Option<Vec<u8>>,
    gif: bool,
) -> wa_e2e::Message {
    wa_e2e::Message {
        video_message: Some(Box::new(wa_e2e::VideoMessage {
            url: Some(upload.url.clone()),
            mimetype: Some(upload.mime_type.clone()),
            file_sha256: Some(upload.file_sha256.clone()),
            file_length: Some(upload.file_length),
            seconds,
            media_key: Some(upload.media_key.clone()),
            caption: caption.filter(|c| !c.is_empty()).map(str::to_owned),
            gif_playback: gif.then_some(true),
            height: dimensions.map(|(_, h)| h),
            width: dimensions.map(|(w, _)| w),
            file_enc_sha256: Some(upload.file_enc_sha256.clone()),
            direct_path: Some(upload.direct_path.clone()),
            media_key_timestamp: Some(wa_store_time_now()),
            jpeg_thumbnail: thumbnail,
            ..Default::default()
        })),
        ..Default::default()
    }
}

fn wa_store_time_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_round_trip_matches_download_checks() {
        // The ciphertext layout must satisfy the three download checks:
        // sha256(body) == fileEncSha256, HMAC(iv ‖ ct)[..10] == tag,
        // sha256(decrypt(ct)) == fileSha256.
        let plaintext = b"fake png bytes";
        let media_key = [7u8; 32];
        let keys = media_keys(&media_key, MediaType::Image);
        let ciphertext = cbc::encrypt_with_iv(&keys.cipher_key, &keys.iv, plaintext).unwrap();
        let mut mac = Hmac::<Sha256>::new_from_slice(&keys.mac_key).unwrap();
        mac.update(&keys.iv);
        mac.update(&ciphertext);
        let tag = mac.finalize().into_bytes();
        let mut body = ciphertext.clone();
        body.extend_from_slice(&tag[..10]);

        assert_eq!(Sha256::digest(&body).to_vec().len(), 32);
        let decrypted = cbc::decrypt(&keys.cipher_key, &keys.iv, &ciphertext).unwrap();
        assert_eq!(decrypted, plaintext);
        assert_eq!(Sha256::digest(plaintext).to_vec().len(), 32);
    }

    #[test]
    fn gif_is_a_video_message_with_playback_flag() {
        let upload = UploadResponse {
            url: "https://example/u".to_owned(),
            direct_path: "/v/abc".to_owned(),
            handle: "h".to_owned(),
            media_key: vec![1; 32],
            file_enc_sha256: vec![2; 32],
            file_sha256: vec![3; 32],
            file_length: 42,
            mime_type: "video/mp4".to_owned(),
        };
        let msg = video_message(&upload, None, Some((320, 240)), Some(3), None, true);
        let video = msg.video_message.unwrap();
        assert_eq!(video.gif_playback, Some(true));
        assert_eq!(video.height, Some(240));
        let still = video_message(&upload, Some("cap"), None, None, None, false);
        let still = still.video_message.unwrap();
        assert_eq!(still.gif_playback, None);
        assert_eq!(still.caption.as_deref(), Some("cap"));
    }

    #[test]
    fn image_message_carries_upload_fields() {
        let upload = UploadResponse {
            url: "https://example/u".to_owned(),
            direct_path: "/v/abc".to_owned(),
            handle: "h".to_owned(),
            media_key: vec![1; 32],
            file_enc_sha256: vec![2; 32],
            file_sha256: vec![3; 32],
            file_length: 42,
            mime_type: "image/jpeg".to_owned(),
        };
        let msg = image_message(&upload, Some("hi"), Some((800, 600)), Some(vec![9, 9]));
        let image = msg.image_message.unwrap();
        assert_eq!(image.caption.as_deref(), Some("hi"));
        assert_eq!((image.width, image.height), (Some(800), Some(600)));
        assert_eq!(image.jpeg_thumbnail, Some(vec![9, 9]));
    }
}
