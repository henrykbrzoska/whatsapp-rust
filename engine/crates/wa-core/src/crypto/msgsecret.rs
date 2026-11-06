// Ported from the Go client `msgsecret.go`, © Tulir Asokan, MPL-2.0 — see THIRD_PARTY_NOTICES.md.

use crate::crypto::gcm;
use crate::crypto::hkdf;

/// Secret use-cases — Go `MsgSecretType`.
pub const ENC_SECRET_POLL_VOTE: &str = "Poll Vote";
pub const ENC_SECRET_REACTION: &str = "Enc Reaction";
pub const ENC_SECRET_COMMENT: &str = "Enc Comment";
pub const ENC_SECRET_REPORT_TOKEN: &str = "Report Token";
pub const ENC_SECRET_EVENT_RESPONSE: &str = "Event Response";
pub const ENC_SECRET_EVENT_EDIT: &str = "Event Edit";
pub const ENC_SECRET_MESSAGE_EDIT: &str = "Message Edit";
pub const ENC_SECRET_POLL_EDIT: &str = "Poll Edit";
pub const ENC_SECRET_POLL_ADD_OPTION: &str = "Poll Add Option";
pub const ENC_SECRET_BOT_MSG: &str = "Bot Message";

/// Go `applyBotMessageHKDF`.
pub fn apply_bot_message_hkdf(message_secret: &[u8]) -> Vec<u8> {
    hkdf::sha256(message_secret, &[], ENC_SECRET_BOT_MSG.as_bytes(), 32)
}

/// Go `generateMsgSecretKey`.
pub fn generate_msg_secret_key(
    modification_type: &str,
    modification_sender: &str,
    orig_msg_id: &str,
    orig_msg_sender: &str,
    orig_msg_secret: &[u8],
) -> (Vec<u8>, Vec<u8>) {
    let mut use_case_secret = Vec::with_capacity(
        orig_msg_id.len() + orig_msg_sender.len() + modification_sender.len() + modification_type.len(),
    );
    use_case_secret.extend_from_slice(orig_msg_id.as_bytes());
    use_case_secret.extend_from_slice(orig_msg_sender.as_bytes());
    use_case_secret.extend_from_slice(modification_sender.as_bytes());
    use_case_secret.extend_from_slice(modification_type.as_bytes());
    let secret_key = hkdf::sha256(orig_msg_secret, &[], &use_case_secret, 32);
    let additional_data = match modification_type {
        s if s == ENC_SECRET_POLL_VOTE || s == ENC_SECRET_EVENT_RESPONSE || s.is_empty() => {
            let mut ad = Vec::with_capacity(orig_msg_id.len() + 1 + modification_sender.len());
            ad.extend_from_slice(orig_msg_id.as_bytes());
            ad.push(0);
            ad.extend_from_slice(modification_sender.as_bytes());
            ad
        }
        _ => Vec::new(),
    };
    (secret_key, additional_data)
}

/// Decrypt a bot (`msmsg`) message — Go `decryptBotMessage`.
pub fn decrypt_bot_message(
    message_secret: &[u8],
    enc_iv: &[u8],
    enc_payload: &[u8],
    message_id: &str,
    target_sender_jid: &str,
    info_sender: &str,
) -> Result<Vec<u8>, crate::crypto::CryptoError> {
    let (new_key, additional_data) = generate_msg_secret_key(
        "",
        info_sender,
        message_id,
        target_sender_jid,
        &apply_bot_message_hkdf(message_secret),
    );
    gcm::decrypt(&new_key, enc_iv, enc_payload, &additional_data)
}

/// Encrypt helper — Go `encryptMsgSecret` inner HKDF part (without store lookup).
pub fn encrypt_msg_secret_payload(
    orig_msg_secret: &[u8],
    modification_type: &str,
    modification_sender: &str,
    orig_msg_id: &str,
    orig_msg_sender: &str,
    plaintext: &[u8],
    iv: &[u8],
) -> Result<Vec<u8>, crate::crypto::CryptoError> {
    let (secret_key, additional_data) =
        generate_msg_secret_key(modification_type, modification_sender, orig_msg_id, orig_msg_sender, orig_msg_secret);
    gcm::encrypt(&secret_key, iv, plaintext, &additional_data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_bot_hkdf_is_deterministic() {
        let secret = vec![1u8; 32];
        let a = apply_bot_message_hkdf(&secret);
        let b = apply_bot_message_hkdf(&secret);
        assert_eq!(a, b);
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn generate_key_poll_vote_has_additional_data() {
        let (_, ad) =
            generate_msg_secret_key(ENC_SECRET_POLL_VOTE, "sender@lid", "MSG1", "orig@s.whatsapp.net", &[2u8; 32]);
        assert!(!ad.is_empty());
        assert_eq!(ad, {
            let mut v = Vec::new();
            v.extend_from_slice(b"MSG1");
            v.push(0);
            v.extend_from_slice(b"sender@lid");
            v
        });
    }

    #[test]
    fn generate_key_reaction_has_no_additional_data() {
        let (_, ad) = generate_msg_secret_key(ENC_SECRET_REACTION, "a", "MSG1", "b", &[2u8; 32]);
        assert!(ad.is_empty());
    }

    #[test]
    fn bot_message_encrypt_decrypt_roundtrip() {
        let secret = vec![0x42u8; 32];
        let msg_id = "3EB0TEST";
        let target = "12345@s.whatsapp.net";
        let sender = "999@lid";
        let plaintext = b"hello bot";
        let iv = [7u8; 12];
        let (key, ad) = generate_msg_secret_key("", sender, msg_id, target, &apply_bot_message_hkdf(&secret));
        let ct = gcm::encrypt(&key, &iv, plaintext, &ad).expect("encrypt");
        let pt = decrypt_bot_message(&secret, &iv, &ct, msg_id, target, sender).expect("decrypt");
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn msg_secret_encrypt_decrypt_roundtrip() {
        let secret = vec![0x11u8; 32];
        let pt = b"poll vote payload";
        let iv = [9u8; 12];
        let ct = encrypt_msg_secret_payload(
            &secret,
            ENC_SECRET_POLL_VOTE,
            "voter@lid",
            "POLL1",
            "creator@s.whatsapp.net",
            pt,
            &iv,
        )
        .expect("encrypt");
        let (key, ad) =
            generate_msg_secret_key(ENC_SECRET_POLL_VOTE, "voter@lid", "POLL1", "creator@s.whatsapp.net", &secret);
        let decrypted = gcm::decrypt(&key, &iv, &ct, &ad).expect("decrypt");
        assert_eq!(decrypted, pt);
    }
}
