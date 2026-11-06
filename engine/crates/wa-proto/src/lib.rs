//! Protobuf types generated from `Go proto/**/*.proto`.
//!
//! Module names follow the proto `package` (e.g. `WAWebProtobufsE2E` →
//! `wa_web_protobufs_e2e`).

#![allow(clippy::all, clippy::pedantic, non_camel_case_types, rustdoc::all)]

include!(concat!(env!("OUT_DIR"), "/_includes.rs"));

#[cfg(test)]
mod tests {
    use prost::Message;

    #[test]
    fn e2e_message_roundtrip() {
        let msg = super::wa_web_protobufs_e2e::Message { conversation: Some("hello".into()), ..Default::default() };
        let bytes = msg.encode_to_vec();
        let back = super::wa_web_protobufs_e2e::Message::decode(bytes.as_slice()).unwrap();
        assert_eq!(back.conversation.as_deref(), Some("hello"));
    }
}
