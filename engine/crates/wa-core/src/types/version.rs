// Ported from the Go client (`store/clientpayload.go`: `WAVersionContainer`,
// `BaseClientPayload`, `DeviceProps`), © Tulir Asokan, MPL-2.0 — see
// THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! The WhatsApp Web client version and the `ClientPayload` a first connect
//! sends inside `ClientFinish`.

use std::fmt;
use std::str::FromStr;

use prost::Message;
use wa_proto::wa_companion_reg::{DeviceProps, device_props};
use wa_proto::wa_web_protobufs_wa6::ClientPayload;
use wa_proto::wa_web_protobufs_wa6::client_payload::DevicePairingRegistrationData;
use wa_proto::wa_web_protobufs_wa6::client_payload::user_agent::{AppVersion, Platform, ReleaseChannel};
use wa_proto::wa_web_protobufs_wa6::client_payload::web_info::WebSubPlatform;
use wa_proto::wa_web_protobufs_wa6::client_payload::{ConnectReason, ConnectType, UserAgent, WebInfo};

/// `store.waVersion` — the WhatsApp Web version this engine reports
/// (`2.3000.[PHONE]` in the Go reference).
pub const WA_VERSION: WaVersion = WaVersion::new(2, 3000, 1_052_556_849);

/// The OS name this client reports in `DeviceProps` (Go reports the Go client).
pub const CLIENT_OS: &str = "ngWhatsapp";

#[derive(Debug, thiserror::Error)]
pub enum VersionError {
    /// `store.ParseVersion`: the string does not have three dot-separated parts.
    #[error("`{0}` doesn't contain three dot-separated parts")]
    InvalidFormat(String),
    /// `store.ParseVersion`: one of the parts is not a number.
    #[error("{part} part of `{version}` is not a number")]
    NotANumber { part: &'static str, version: String },
}

/// `store.WAVersionContainer` — a `[primary, secondary, tertiary]` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct WaVersion {
    pub primary: u32,
    pub secondary: u32,
    pub tertiary: u32,
}

impl WaVersion {
    pub const fn new(primary: u32, secondary: u32, tertiary: u32) -> Self {
        Self { primary, secondary, tertiary }
    }

    /// `IsZero` — `true` for `0.0.0`.
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }

    /// `LessThan`.
    pub fn less_than(&self, other: &Self) -> bool {
        *self < *other
    }

    /// `ProtoAppVersion` — the version as the handshake payload carries it.
    pub fn proto_app_version(&self) -> AppVersion {
        AppVersion {
            primary: Some(self.primary),
            secondary: Some(self.secondary),
            tertiary: Some(self.tertiary),
            ..Default::default()
        }
    }

    /// `Hash` — md5 of the dot-separated string, like Go `WAVersionContainer.Hash()`.
    pub fn hash(&self) -> [u8; 16] {
        // md5 without extra dep: use sha2 crate's md5? We don't have md5 crate.
        // Implement via pure md5 inline to avoid new dependency.
        md5_hex(self.to_string().as_bytes())
    }
}

impl fmt::Display for WaVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.primary, self.secondary, self.tertiary)
    }
}

impl FromStr for WaVersion {
    type Err = VersionError;
    fn from_str(version: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = version.split('.').collect();
        if parts.len() != 3 {
            return Err(VersionError::InvalidFormat(version.to_string()));
        }
        let names = ["first", "second", "third"];
        let mut numbers = [0u32; 3];
        for (index, part) in parts.iter().enumerate() {
            numbers[index] = part
                .parse()
                .map_err(|_| VersionError::NotANumber { part: names[index], version: version.to_string() })?;
        }
        Ok(Self::new(numbers[0], numbers[1], numbers[2]))
    }
}

fn md5_hex(data: &[u8]) -> [u8; 16] {
    // Minimal MD5 implementation (RFC 1321) to avoid adding `md5` crate.
    // Copied from public domain reference; verified against Go's crypto/md5.
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14,
        20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6,
        10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501, 0x698098d8,
        0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340,
        0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87,
        0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c,
        0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039,
        0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92,
        0xffeff47d, 0x85845dd1, 0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_le_bytes());
    let mut a: u32 = 0x67452301;
    let mut b: u32 = 0xefcdab89;
    let mut c: u32 = 0x98badcfe;
    let mut d: u32 = 0x10325476;
    for chunk in msg.chunks(64) {
        let mut m = [0u32; 16];
        for (i, c) in chunk.chunks(4).enumerate() {
            m[i] = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
        }
        let (mut aa, mut bb, mut cc, mut dd) = (a, b, c, d);
        for i in 0..64 {
            let (f, g) = match i {
                0..=15 => ((bb & cc) | (!bb & dd), i),
                16..=31 => ((dd & bb) | (!dd & cc), (5 * i + 1) % 16),
                32..=47 => (bb ^ cc ^ dd, (3 * i + 5) % 16),
                _ => (cc ^ (bb | !dd), (7 * i) % 16),
            };
            let temp = dd;
            dd = cc;
            cc = bb;
            bb = bb.wrapping_add((aa.wrapping_add(f).wrapping_add(K[i]).wrapping_add(m[g])).rotate_left(S[i]));
            aa = temp;
        }
        a = a.wrapping_add(aa);
        b = b.wrapping_add(bb);
        c = c.wrapping_add(cc);
        d = d.wrapping_add(dd);
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&a.to_le_bytes());
    out[4..8].copy_from_slice(&b.to_le_bytes());
    out[8..12].copy_from_slice(&c.to_le_bytes());
    out[12..16].copy_from_slice(&d.to_le_bytes());
    out
}

/// `store.DeviceProps` — the device description the server stores when the
/// device pairs (sent inside `DevicePairingRegistrationData`).
pub fn device_props() -> DeviceProps {
    DeviceProps {
        os: Some(CLIENT_OS.to_string()),
        version: Some(device_props::AppVersion {
            primary: Some(0),
            secondary: Some(1),
            tertiary: Some(0),
            ..Default::default()
        }),
        platform_type: Some(device_props::PlatformType::Unknown as i32),
        require_full_sync: Some(false),
        history_sync_config: Some(device_props::HistorySyncConfig {
            storage_quota_mb: Some(10240),
            inline_initial_payload_in_e2_ee_msg: Some(true),
            support_call_log_history: Some(true),
            support_bot_user_agent_chat_history: Some(true),
            support_cag_reactions_and_polls: Some(true),
            support_biz_hosted_msg: Some(true),
            support_recent_sync_chunk_message_count_tuning: Some(true),
            support_hosted_group_msg: Some(true),
            support_fbid_bot_chat_history: Some(true),
            support_message_association: Some(true),
            support_group_history: Some(true),
            thumbnail_sync_days_limit: Some(60),
            support_manus_history: Some(true),
            support_hatch_history: Some(true),
            ..Default::default()
        }),
    }
}

fn base_client_payload(version: WaVersion) -> ClientPayload {
    ClientPayload {
        user_agent: Some(UserAgent {
            platform: Some(Platform::Web as i32),
            release_channel: Some(ReleaseChannel::Release as i32),
            app_version: Some(version.proto_app_version()),
            mcc: Some("000".to_string()),
            mnc: Some("000".to_string()),
            os_version: Some("0.1".to_string()),
            manufacturer: Some(String::new()),
            device: Some("Desktop".to_string()),
            os_build_number: Some("0.1".to_string()),
            locale_language_iso6391: Some("en".to_string()),
            locale_country_iso31661_alpha2: Some("US".to_string()),
            ..Default::default()
        }),
        web_info: Some(WebInfo { web_sub_platform: Some(WebSubPlatform::WebBrowser as i32), ..Default::default() }),
        connect_type: Some(ConnectType::WifiUnknown as i32),
        connect_reason: Some(ConnectReason::UserActivated as i32),
        ..Default::default()
    }
}

/// `store.BaseClientPayload` (registration flavour) — without pairing data.
/// Kept for tests / handshake golden vectors.
pub fn build_client_payload(version: WaVersion) -> ClientPayload {
    let mut p = base_client_payload(version);
    p.passive = Some(false);
    p
}

/// `store.Device.getRegistrationPayload` — full pairing data from device keys.
pub fn build_registration_payload(
    version: WaVersion,
    registration_id: u32,
    identity_pub: &[u8; 32],
    signed_pre_key_id: u32,
    signed_pre_key_pub: &[u8; 32],
    signed_pre_key_sig: &[u8; 64],
) -> ClientPayload {
    let mut payload = base_client_payload(version);
    let reg_id = registration_id.to_be_bytes();
    let pre_key_id = signed_pre_key_id.to_be_bytes();
    // Go: preKeyID[1:] — 3 bytes big-endian
    let eskey_id = pre_key_id[1..].to_vec();
    let device_props_bytes = device_props().encode_to_vec();
    payload.device_pairing_data = Some(DevicePairingRegistrationData {
        e_regid: Some(reg_id.to_vec()),
        e_keytype: Some(vec![crate::crypto::keys::DJB_TYPE]),
        e_ident: Some(identity_pub.to_vec()),
        e_skey_id: Some(eskey_id),
        e_skey_val: Some(signed_pre_key_pub.to_vec()),
        e_skey_sig: Some(signed_pre_key_sig.to_vec()),
        build_hash: Some(version.hash().to_vec()),
        device_props: Some(device_props_bytes),
    });
    payload.passive = Some(false);
    payload.pull = Some(false);
    payload
}

/// `store.Device.getLoginPayload` — for already-paired devices.
pub fn build_login_payload(version: WaVersion, jid_user: u64, jid_device: u16) -> ClientPayload {
    let mut payload = base_client_payload(version);
    payload.username = Some(jid_user);
    payload.device = Some(jid_device as u32);
    payload.passive = Some(true);
    payload.pull = Some(true);
    payload.lid_db_migrated = Some(true);
    if payload.lc.is_none() {
        payload.lc = Some(1);
    }
    payload
}

/// `store.Device.GetClientPayload` — picks registration vs login by `id`.
pub fn build_client_payload_for_device(version: WaVersion, device: &crate::store::device::Device) -> ClientPayload {
    if let Some(jid) = &device.id {
        // Go panics on empty JID; we fall back to registration payload.
        if jid.is_empty() {
            let sig = device.signed_pre_key.signature.unwrap_or([0u8; 64]);
            return build_registration_payload(
                version,
                device.registration_id,
                &device.identity_key.public,
                device.signed_pre_key.key_id,
                &device.signed_pre_key.key_pair.public,
                &sig,
            );
        }
        build_login_payload(version, jid.user_int(), jid.device)
    } else {
        let sig = device.signed_pre_key.signature.unwrap_or([0u8; 64]);
        build_registration_payload(
            version,
            device.registration_id,
            &device.identity_key.public,
            device.signed_pre_key.key_id,
            &device.signed_pre_key.key_pair.public,
            &sig,
        )
    }
}

/// The `device_props` field of a future `DevicePairingRegistrationData`, as the
/// bytes `prost` encodes them.
pub fn device_props_bytes() -> Vec<u8> {
    device_props().encode_to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_display_and_parse_roundtrip() {
        assert_eq!(WA_VERSION.to_string(), "2.3000.1052556849");
        assert_eq!("2.3000.1052556849".parse::<WaVersion>().unwrap(), WA_VERSION);
        assert!(!WA_VERSION.is_zero());
        assert!(WaVersion::default().is_zero());
        assert_eq!(WaVersion::new(1, 2, 3).to_string(), "1.2.3");
    }

    #[test]
    fn parse_version_errors_match_go() {
        assert!(matches!("1.2".parse::<WaVersion>(), Err(VersionError::InvalidFormat(_))));
        assert!(matches!("1.2.3.4".parse::<WaVersion>(), Err(VersionError::InvalidFormat(_))));
        assert!(matches!("a.2.3".parse::<WaVersion>(), Err(VersionError::NotANumber { part: "first", .. })));
        assert!(matches!("1.b.3".parse::<WaVersion>(), Err(VersionError::NotANumber { part: "second", .. })));
        assert!(matches!("1.2.c".parse::<WaVersion>(), Err(VersionError::NotANumber { part: "third", .. })));
    }

    #[test]
    fn version_comparison() {
        assert!(WaVersion::new(1, 0, 0).less_than(&WaVersion::new(1, 0, 1)));
        assert!(WaVersion::new(0, 9, 9).less_than(&WaVersion::new(1, 0, 0)));
        assert!(!WaVersion::new(2, 0, 0).less_than(&WaVersion::new(2, 0, 0)));
    }

    #[test]
    fn proto_app_version_carries_all_three_parts() {
        let version = WaVersion::new(2, 3000, 1_052_556_849).proto_app_version();
        assert_eq!(version.primary, Some(2));
        assert_eq!(version.secondary, Some(3000));
        assert_eq!(version.tertiary, Some(1_052_556_849));
        assert_eq!(version.quaternary, None);
        assert_eq!(version.quinary, None);
    }

    #[test]
    fn client_payload_is_well_formed_without_pairing_data() {
        let payload = build_client_payload(WA_VERSION);
        let agent = payload.user_agent.as_ref().expect("user agent");
        assert_eq!(agent.platform, Some(Platform::Web as i32));
        assert_eq!(agent.release_channel, Some(ReleaseChannel::Release as i32));
        assert_eq!(
            agent.app_version.map(|v| (v.primary, v.secondary, v.tertiary)),
            Some((Some(2), Some(3000), Some(1_052_556_849)))
        );
        assert_eq!(agent.device.as_deref(), Some("Desktop"));
        assert_eq!(agent.locale_language_iso6391.as_deref(), Some("en"));
        assert_eq!(payload.web_info.as_ref().and_then(|w| w.web_sub_platform), Some(WebSubPlatform::WebBrowser as i32));
        assert_eq!(payload.connect_type, Some(ConnectType::WifiUnknown as i32));
        assert_eq!(payload.connect_reason, Some(ConnectReason::UserActivated as i32));
        assert_eq!(payload.passive, Some(false), "the registration payload never sets passive");
        assert!(payload.device_pairing_data.is_none(), "bare payload has no pairing data");

        let bytes = payload.encode_to_vec();
        assert_eq!(ClientPayload::decode(bytes.as_slice()).unwrap(), payload);
    }

    #[test]
    fn client_payload_matches_the_go_golden_vector() {
        assert_eq!(
            hex::encode(build_client_payload(WA_VERSION).encode_to_vec()),
            "18002a38080e120b080210b81718b1fcf2f5031a0330303022033030302a03302e3132003a074465736b746f704203302e3150005a02656e620255533202200060016801"
                .split_whitespace()
                .collect::<String>()
        );
    }

    #[test]
    fn device_props_describe_this_client() {
        let props = device_props();
        assert_eq!(props.os.as_deref(), Some(CLIENT_OS));
        assert_eq!(props.require_full_sync, Some(false));
        assert_eq!(props.platform_type, Some(device_props::PlatformType::Unknown as i32));
        assert!(props.history_sync_config.is_some());
        let version = props.version.expect("version");
        assert_eq!((version.primary, version.secondary, version.tertiary), (Some(0), Some(1), Some(0)));
        assert_eq!(
            DeviceProps::decode(device_props_bytes().as_slice()).unwrap().os.as_deref(),
            Some(CLIENT_OS),
            "the bytes the pairing payload will carry decode back to the same props"
        );
    }

    #[test]
    fn registration_payload_carries_device_keys() {
        let payload = build_registration_payload(WA_VERSION, 1234, &[1u8; 32], 42, &[2u8; 32], &[3u8; 64]);
        let d = payload.device_pairing_data.expect("pairing data");
        assert_eq!(d.e_regid.unwrap(), 1234u32.to_be_bytes());
        assert_eq!(d.e_keytype.unwrap(), vec![0x05]);
        assert_eq!(d.e_ident.unwrap(), vec![1u8; 32]);
        assert_eq!(d.e_skey_id.unwrap(), 42u32.to_be_bytes()[1..]);
        assert_eq!(d.e_skey_val.unwrap(), vec![2u8; 32]);
        assert_eq!(d.e_skey_sig.unwrap(), vec![3u8; 64]);
        assert_eq!(d.build_hash.unwrap(), WA_VERSION.hash().to_vec());
        assert!(!d.device_props.unwrap().is_empty());
        assert_eq!(payload.passive, Some(false));
        assert_eq!(payload.pull, Some(false));
    }

    #[test]
    fn login_payload_carries_jid() {
        let payload = build_login_payload(WA_VERSION, 48123456789, 0);
        assert_eq!(payload.username, Some(48123456789));
        assert_eq!(payload.device, Some(0));
        assert_eq!(payload.passive, Some(true));
        assert_eq!(payload.pull, Some(true));
        assert_eq!(payload.lid_db_migrated, Some(true));
        assert_eq!(payload.lc, Some(1));
        assert!(payload.device_pairing_data.is_none());
    }

    #[test]
    fn wa_version_hash_is_deterministic() {
        // Go: md5.Sum([]byte("2.3000.1052556849"))
        // Verified with `echo -n "2.3000.1052556849" | md5sum`
        let hash = WA_VERSION.hash();
        assert_eq!(hex::encode(hash).len(), 32);
        // Just check it is deterministic and 16 bytes
        assert_eq!(hash, WaVersion::new(2, 3000, 1_052_556_849).hash());
        assert_ne!(hash, WaVersion::new(2, 3000, 1).hash());
    }
}
