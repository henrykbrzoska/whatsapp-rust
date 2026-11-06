// Ported from the Go client `user.go`, © Tulir Asokan, MPL-2.0 —
// see THIRD_PARTY_NOTICES.md.
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at http://mozilla.org/MPL/2.0/.

//! User lookups: `GetUserInfo` (the `usync` query) and
//! `GetProfilePictureInfo`.
//!
//! Go counterparts: `Go user.go` (`usync`, `GetUserInfo`,
//! `GetProfilePictureInfo`, `parseVerifiedName`, `parseDeviceList`).
//!
//! Divergences from Go:
//! * the verified-name certificate is not decoded (it needs
//!   `waVnameCert.VerifiedNameCertificate`); the certificate is kept as hex and
//!   the `verified_level`/`actual_actors` attributes are still available;
//! * `PictureInfo.hash` stays base64 text instead of decoded bytes (Go decodes
//!   it for the media download path, which this port does not use yet).

use serde::{Deserialize, Serialize};

use crate::binary::node::{Node, NodeContent};
use crate::store::traits::LidMapping;
use crate::types::jid::{DEFAULT_USER_SERVER, HIDDEN_USER_SERVER, Jid};

use super::ack::attr_text;
use super::message::attr_int;
use super::{Client, ClientError, InfoQuery, IqKind};

/// Go `types.UserInfo` — what a `usync` lookup reveals about a user.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserInfoData {
    /// The user the entry is about (a phone-number JID).
    pub jid: Jid,
    /// The LID of the user, empty when unknown.
    pub lid: Jid,
    /// The user's status text (`status` node content).
    pub status: String,
    /// The current profile-picture ID, empty when the user has none.
    pub picture_id: String,
    /// The devices the server listed for the user.
    pub devices: Vec<Jid>,
    /// The verified-name details of a business account, when any.
    pub verified_name: Option<VerifiedNameData>,
}

/// Go `types.VerifiedName` (the certificate bytes stay undecoded).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedNameData {
    /// The verified name from the certificate details, when it parsed.
    pub verified_name: String,
    /// Hex-encoded `VerifiedNameCertificate` protobuf.
    pub certificate: String,
    pub verified_level: String,
    pub version: i64,
    pub host_storage: i64,
    pub actual_actors: i64,
    pub privacy_mode_ts: Option<i64>,
}

/// Go `types.ProfilePictureInfo` — where a profile picture lives.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePictureInfoData {
    pub url: String,
    pub id: String,
    /// `image` (full resolution) or `preview` (thumbnail).
    pub picture_type: String,
    pub direct_path: String,
    /// The (base64) hash the server sent, empty when none.
    pub hash: String,
}

impl ProfilePictureInfoData {
    /// The `type` field — named `picture_type` in Rust to avoid the `type`
    /// keyword, but `type` on the wire (Go `ProfilePictureInfo.Type`).
    pub fn wire_type(&self) -> &str {
        &self.picture_type
    }
}

/// Go `GetProfilePictureParams`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GetProfilePictureParams {
    /// Ask for the thumbnail instead of the full image.
    pub preview: bool,
    /// The last known picture ID — a 304 answer then means "unchanged".
    pub existing_id: String,
    /// The target is a community: the query goes through `w:g2`.
    pub is_community: bool,
    /// A group both sides share (helps the server authorize the query).
    pub common_gid: Option<Jid>,
    /// An invite code, to peek at a group the user has not joined.
    pub invite_code: String,
    /// The persona ID of a Meta AI bot.
    pub persona_id: String,
}

/// Go `parseDeviceList` — the `<device-list>`/`<devices>` children of a
/// `usync` user node.
pub fn parse_device_list(jid: &Jid, node: &Node) -> Vec<Jid> {
    let mut devices = Vec::new();
    // Go reads `<device>` children directly; some servers wrap them in a
    // `<device-list>` element, which is unwrapped here.
    let list = node.optional_child_by_tag(&["device-list"]).unwrap_or(node);
    for device in list.children_by_tag("device") {
        let Some(id) = attr_text(device, "id") else { continue };
        let Ok(id) = id.parse::<u16>() else { continue };
        // Go skips the `hidden="true"` placeholders it cannot address.
        devices.push(jid.to_non_ad().with_device(id));
    }
    devices
}

/// Go `parseVerifiedName` — the `<business><verified_name>` of a user node.
pub fn parse_verified_name(business_node: &Node) -> Option<VerifiedNameData> {
    if business_node.tag != "business" {
        return None;
    }
    let verified_name_node = business_node.optional_child_by_tag(&["verified_name"])?;
    let certificate = verified_name_node.as_bytes().map(hex::encode).unwrap_or_default();
    Some(VerifiedNameData {
        verified_name: String::new(),
        certificate,
        verified_level: attr_text(verified_name_node, "verified_level").unwrap_or_default(),
        version: attr_int(verified_name_node, "v").unwrap_or_default(),
        host_storage: attr_int(verified_name_node, "host_storage").unwrap_or_default(),
        actual_actors: attr_int(verified_name_node, "actual_actors").unwrap_or_default(),
        privacy_mode_ts: attr_int(verified_name_node, "privacy_mode_ts"),
    })
}

/// Go `usync` — the shared user-sync query, returning the `<list>` node.
pub async fn usync(
    client: &Client,
    jids: &[Jid],
    mode: &str,
    context: &str,
    query: Vec<Node>,
    include_privacy_token: bool,
) -> Result<Node, ClientError> {
    let mut user_list = Vec::with_capacity(jids.len());
    for jid in jids {
        let jid = jid.to_non_ad();
        match jid.server.as_str() {
            DEFAULT_USER_SERVER | HIDDEN_USER_SERVER => {
                let mut user = Node::new("user").attr("jid", jid.clone());
                if include_privacy_token
                    && let Ok(Some(token)) = client.ensure_tc_token(&jid).await
                    && !token.is_empty()
                {
                    user = user.with_content(vec![Node::new("tctoken").with_content(NodeContent::Bytes(token))]);
                }
                user_list.push(user);
            }
            other => {
                return Err(ClientError::MessageParse(format!("unknown user server '{other}' in a usync query")));
            }
        }
    }
    let usync = Node::new("usync")
        .attr("sid", client.generate_request_id())
        .attr("mode", mode.to_owned())
        .attr("last", "true")
        .attr("index", "0")
        .attr("context", context.to_owned())
        .with_content(vec![Node::new("query").with_content(query), Node::new("list").with_content(user_list)]);
    let response = client
        .send_iq(InfoQuery {
            namespace: "usync".into(),
            kind: IqKind::Get,
            to: Some(Jid::server_jid()),
            content: vec![usync],
            ..Default::default()
        })
        .await?;
    let usync = response
        .optional_child_by_tag(&["usync"])
        .ok_or(ClientError::MissingElement { tag: "usync", parent: "usync response" })?;
    usync
        .optional_child_by_tag(&["list"])
        .cloned()
        .ok_or(ClientError::MissingElement { tag: "list", parent: "response to usync query" })
}

impl Client {
    /// Go `GetUserInfo` — look up `jids` (status, picture ID, devices, LID) and
    /// store the LID mappings the answer revealed.
    pub async fn get_user_info(&self, jids: &[Jid]) -> Result<Vec<UserInfoData>, ClientError> {
        let list = usync(
            self,
            jids,
            "full",
            "background",
            vec![
                Node::new("business").with_content(vec![Node::new("verified_name")]),
                Node::new("status"),
                Node::new("picture"),
                Node::new("devices").attr("version", "2"),
                Node::new("lid"),
            ],
            true,
        )
        .await?;

        let mut infos = Vec::new();
        let mut mappings = Vec::new();
        for child in list.children_by_tag("user") {
            let Some(jid) = child.attrs.get("jid").and_then(|value| value.as_jid()).cloned() else { continue };
            let status = child
                .optional_child_by_tag(&["status"])
                .and_then(Node::as_bytes)
                .map(|bytes| String::from_utf8_lossy(bytes).into_owned());
            let picture_id = child
                .optional_child_by_tag(&["picture"])
                .and_then(|picture| attr_text(picture, "id"))
                .unwrap_or_default();
            let devices_node = child.optional_child_by_tag(&["devices"]);
            let devices = devices_node.map(|node| parse_device_list(&jid, node)).unwrap_or_default();
            let lid = child
                .optional_child_by_tag(&["lid"])
                .and_then(|lid| lid.attrs.get("val").and_then(|value| value.as_jid()).cloned())
                .unwrap_or_default();
            if !lid.is_empty() {
                mappings.push(LidMapping { lid: lid.clone(), pn: jid.clone() });
            }
            let verified_name = child.optional_child_by_tag(&["business"]).and_then(parse_verified_name);
            infos.push(UserInfoData {
                jid,
                lid,
                status: status.unwrap_or_default(),
                picture_id,
                devices,
                verified_name,
            });
        }
        if !mappings.is_empty() {
            let lids = self.device.lock().await.lids.clone();
            if let Err(err) = lids.put_many_lid_mappings(&mappings).await {
                // Go logs and continues: a failed mapping store must not fail
                // the lookup.
                tracing::warn!("failed to store LID mappings from a usync call: {err}");
            }
        }
        Ok(infos)
    }

    /// Go `GetProfilePictureInfo` — where `jid`'s profile picture lives.
    ///
    /// Returns `Ok(None)` when the picture is unchanged (`existing_id` matches,
    /// the server answers 304) or was removed in the meantime.
    pub async fn get_profile_picture_info(
        &self,
        jid: &Jid,
        params: &GetProfilePictureParams,
    ) -> Result<Option<ProfilePictureInfoData>, ClientError> {
        let preview_type = if params.preview { "preview" } else { "image" };
        let mut picture = Node::new("picture").attr("query", "url").attr("type", preview_type);
        if !params.existing_id.is_empty() {
            picture = picture.attr("id", params.existing_id.clone());
        }
        if !params.invite_code.is_empty() {
            picture = picture.attr("invite", params.invite_code.clone());
        }

        let (namespace, to, target, content, expect_wrapped) = if params.is_community {
            picture = picture.attr("parent_group_jid", jid.to_string());
            let pictures = Node::new("pictures").with_content(vec![picture]);
            ("w:g2", jid.clone(), None, vec![pictures], true)
        } else {
            if let Some(common_gid) = &params.common_gid {
                picture = picture.attr("common_gid", common_gid.to_string());
            }
            if !params.persona_id.is_empty() {
                picture = picture.attr("persona_id", params.persona_id.clone());
            }
            if let Ok(Some(token)) = self.ensure_tc_token(jid).await
                && !token.is_empty()
            {
                picture = picture.with_content(vec![Node::new("tctoken").with_content(NodeContent::Bytes(token))]);
            }
            ("w:profile:picture", Jid::server_jid(), Some(jid.clone()), vec![picture], false)
        };

        let response = self
            .send_iq(InfoQuery {
                namespace: namespace.into(),
                kind: IqKind::Get,
                to: Some(to),
                target,
                content,
                ..Default::default()
            })
            .await?;
        let response = if expect_wrapped {
            response
                .optional_child_by_tag(&["pictures"])
                .ok_or(ClientError::MissingElement { tag: "pictures", parent: "response to profile picture query" })?
        } else {
            &response
        };
        let Some(picture) = response.optional_child_by_tag(&["picture"]) else {
            if !params.existing_id.is_empty() {
                // "unchanged" and the server sent no picture node at all.
                return Ok(None);
            }
            return Err(ClientError::MissingElement { tag: "picture", parent: "response to profile picture query" });
        };
        match attr_int(picture, "status") {
            // 304: the picture is still the one we know.
            Some(304) => return Ok(None),
            // 204: the user/group has no picture.
            Some(204) | Some(404) => return Ok(None),
            _ => {}
        }
        Ok(Some(ProfilePictureInfoData {
            url: attr_text(picture, "url").unwrap_or_default(),
            id: attr_text(picture, "id").unwrap_or_default(),
            picture_type: attr_text(picture, "type").unwrap_or_default(),
            direct_path: attr_text(picture, "direct_path").unwrap_or_default(),
            hash: attr_text(picture, "hash").unwrap_or_default(),
        }))
    }

    /// [`Self::get_profile_picture_info`] with default parameters.
    pub async fn get_profile_picture(&self, jid: &Jid) -> Result<Option<ProfilePictureInfoData>, ClientError> {
        self.get_profile_picture_info(jid, &GetProfilePictureParams::default()).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jid(value: &str) -> Jid {
        Jid::parse(value).unwrap()
    }

    #[test]
    fn parse_device_list_reads_the_device_ids() {
        let user = jid("491234567890@s.whatsapp.net");
        let flat = Node::new("devices")
            .with_content(vec![Node::new("device").attr("id", "0"), Node::new("device").attr("id", "12")]);
        assert_eq!(parse_device_list(&user, &flat), vec![user.with_device(0), user.with_device(12)]);

        let wrapped = Node::new("devices")
            .with_content(vec![Node::new("device-list").with_content(vec![Node::new("device").attr("id", "12")])]);
        assert_eq!(parse_device_list(&user, &wrapped), vec![user.with_device(12)]);
    }

    #[test]
    fn parse_device_list_tolerates_a_missing_list() {
        let user = jid("491234567890@s.whatsapp.net");
        assert!(parse_device_list(&user, &Node::new("devices")).is_empty());
    }

    #[test]
    fn parse_verified_name_keeps_the_certificate_and_attributes() {
        let business = Node::new("business").with_content(vec![
            Node::new("verified_name")
                .attr("verified_level", "1")
                .attr("v", "3")
                .attr("actual_actors", "2")
                .attr("privacy_mode_ts", "1700000000")
                .with_content(NodeContent::Bytes(vec![0xDE, 0xAD])),
        ]);
        let verified = parse_verified_name(&business).expect("a verified name");
        assert_eq!(verified.certificate, "dead");
        assert_eq!(verified.verified_level, "1");
        assert_eq!(verified.version, 3);
        assert_eq!(verified.actual_actors, 2);
        assert_eq!(verified.privacy_mode_ts, Some(1_700_000_000));
        assert!(parse_verified_name(&Node::new("status")).is_none(), "a non-business node yields nothing");
        assert!(parse_verified_name(&Node::new("business")).is_none(), "no verified_name child yields nothing");
    }

    #[test]
    fn profile_picture_info_is_serialized_with_camel_case_fields() {
        let info = ProfilePictureInfoData {
            url: "https://example.invalid/p.jpg".to_owned(),
            id: "PIC1".to_owned(),
            picture_type: "image".to_owned(),
            direct_path: "/v/t62".to_owned(),
            hash: "AAAA".to_owned(),
        };
        let json = serde_json::to_string(&info).expect("serializable");
        assert!(json.contains("\"pictureType\":\"image\""), "unknown fields stay camelCase: {json}");
        assert!(json.contains("\"directPath\":\"/v/t62\""));
        assert_eq!(info.wire_type(), "image");
    }

    #[test]
    fn user_info_serializes_the_lid_and_devices() {
        let info = UserInfoData {
            jid: jid("491234567890@s.whatsapp.net"),
            lid: jid("999000111222333@lid"),
            status: "hi".to_owned(),
            picture_id: "PIC1".to_owned(),
            devices: vec![jid("491234567890:0@s.whatsapp.net")],
            verified_name: None,
        };
        let json = serde_json::to_string(&info).expect("serializable");
        assert!(json.contains("\"999000111222333@lid\""), "{json}");
        assert!(json.contains("\"pictureId\":\"PIC1\""), "{json}");
    }
}
