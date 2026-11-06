//! Store tests: device roundtrip, migrations, foreign keys, global LID map
//! and the per-device `SqlStore`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use wa_proto::wa_adv::AdvSignedDeviceIdentity;

use super::*;
use crate::crypto::keys::{djb_public, verify_signature};
use crate::store::traits::*;
use crate::store::{Device, device};
use crate::types::jid::{DEFAULT_USER_SERVER, HIDDEN_USER_SERVER, Jid};

fn account() -> AdvSignedDeviceIdentity {
    AdvSignedDeviceIdentity {
        details: Some(b"details".to_vec()),
        account_signature_key: Some(vec![1; 32]),
        account_signature: Some(vec![2; 64]),
        device_signature: Some(vec![3; 64]),
    }
}

/// A paired device saved in `c` as `<user>.0:<dev>@s.whatsapp.net`.
async fn saved_device(c: &Container, user: &str, dev: u8) -> Device {
    let mut d = c.new_device();
    d.id = Some(Jid::new_ad(user, 0, dev));
    d.lid = Jid::new_ad(format!("9{user}"), 1, dev);
    d.account = Some(account());
    d.platform = "android".into();
    d.push_name = "Alice".into();
    d.facebook_uuid = Some("123e4567-e89b-12d3-a456-426614174000".into());
    d.save().await.unwrap();
    d
}

fn pn(user: &str) -> Jid {
    Jid::new(user, DEFAULT_USER_SERVER)
}

fn lid(user: &str) -> Jid {
    Jid::new(user, HIDDEN_USER_SERVER)
}

// ---- container: migrations / foreign keys --------------------------------

#[tokio::test]
async fn migrates_and_reports_version() {
    let c = Container::open_memory().await.unwrap();
    assert_eq!(c.version().await.unwrap(), SCHEMA_VERSION);
    assert!(c.device_jids().await.unwrap().is_empty());
    // Idempotent upgrade.
    c.upgrade().await.unwrap();
    c.upgrade().await.unwrap();
    assert_eq!(c.version().await.unwrap(), SCHEMA_VERSION);
    let (rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM wa_version").fetch_one(c.pool()).await.unwrap();
    assert_eq!(rows, 1, "re-running migrations must not insert another version row");
}

#[tokio::test]
async fn schema_has_all_expected_tables() {
    // Every table from the reference schema exists with the `wa_` prefix.
    // (The full list lives in migrations/0001_latest_schema.sql.)
    const EXPECTED: &[&str] = &[
        "wa_device",
        "wa_identity_keys",
        "wa_pre_keys",
        "wa_sessions",
        "wa_sender_keys",
        "wa_app_state_sync_keys",
        "wa_app_state_version",
        "wa_app_state_mutation_macs",
        "wa_contacts",
        "wa_chat_settings",
        "wa_message_secrets",
        "wa_lid_map",
        "wa_event_buffer",
        "wa_retry_buffer",
        "wa_privacy_tokens",
        "wa_nct_salt",
        "wa_version",
    ];
    let c = Container::open_memory().await.unwrap();
    let tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type='table'").fetch_all(c.pool()).await.unwrap();
    for name in EXPECTED {
        assert!(tables.contains(&name.to_string()), "missing table {name}");
    }
}

#[tokio::test]
async fn foreign_keys_enforced() {
    let c = Container::open_memory().await.unwrap();
    let (fk,): (i64,) = sqlx::query_as("PRAGMA foreign_keys").fetch_one(c.pool()).await.unwrap();
    assert_eq!(fk, 1);
    let err = sqlx::query("INSERT INTO wa_sessions (our_jid, their_id, session) VALUES ('nobody', 'x', x'00')")
        .execute(c.pool())
        .await;
    assert!(err.is_err(), "insert without device must violate FK");
}

#[tokio::test]
async fn upgrade_refuses_without_foreign_keys() {
    let opts = SqliteConnectOptions::from_str("sqlite::memory:").unwrap().foreign_keys(false);
    let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();
    let err = Container::with_pool(pool).upgrade().await.unwrap_err();
    assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::ForeignKeysDisabled));
}

#[tokio::test]
async fn file_database_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("wa.db");
    let c = Container::open(&path).await.unwrap();
    let d = saved_device(&c, "48100", 3).await;
    drop(c);
    let c = Container::open(&path).await.unwrap();
    assert_eq!(c.version().await.unwrap(), SCHEMA_VERSION);
    let back = c.get_device(d.id.as_ref().unwrap()).await.unwrap().unwrap();
    assert_eq!(back.noise_key, d.noise_key);
}

// ---- container: devices --------------------------------------------------

#[tokio::test]
async fn new_device_has_fresh_valid_keys() {
    let c = Container::open_memory().await.unwrap();
    let a = c.new_device();
    let b = c.new_device();
    assert_ne!(a.noise_key, b.noise_key);
    assert_ne!(a.identity_key, b.identity_key);
    assert_eq!(a.adv_secret_key.len(), 32);
    assert_eq!(a.signed_pre_key.key_id, 1);
    let sig = a.signed_pre_key.signature.unwrap();
    assert!(verify_signature(&a.identity_key.public, &djb_public(&a.signed_pre_key.key_pair.public), &sig));
    assert!(!a.initialized && a.id.is_none());
    // Unsaved devices have inert stores and nothing is written.
    let err = a.sessions.get_session("x").await.unwrap_err();
    assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::NotInitialized));
    assert!(c.device_jids().await.unwrap().is_empty());
}

#[tokio::test]
async fn put_get_delete_device_roundtrip() {
    let c = Container::open_memory().await.unwrap();
    let mut d = saved_device(&c, "48123", 7).await;
    assert!(d.initialized);
    let jid = d.get_jid();
    assert_eq!(jid.to_string(), "48123:7@s.whatsapp.net");

    let back = c.get_device(&jid).await.unwrap().expect("device stored");
    assert_eq!(back.id, d.id);
    assert_eq!(back.lid, d.lid);
    assert_eq!(back.noise_key, d.noise_key);
    assert_eq!(back.identity_key, d.identity_key);
    assert_eq!(back.signed_pre_key, d.signed_pre_key);
    assert_eq!(back.registration_id, d.registration_id);
    assert_eq!(back.adv_secret_key, d.adv_secret_key);
    assert_eq!(back.account, d.account);
    assert_eq!(back.platform, "android");
    assert_eq!(back.push_name, "Alice");
    assert_eq!(back.facebook_uuid, d.facebook_uuid);
    assert!(back.initialized);

    // Upsert updates the mutable columns only.
    d.push_name = "Alice B".into();
    d.business_name = "Shop".into();
    d.save().await.unwrap();
    let back = c.get_device(&jid).await.unwrap().unwrap();
    assert_eq!((back.push_name.as_str(), back.business_name.as_str()), ("Alice B", "Shop"));
    assert_eq!(c.get_all_devices().await.unwrap().len(), 1);

    d.delete().await.unwrap();
    assert!(d.deleted && d.id.is_none() && d.lid.is_empty());
    assert!(c.get_device(&jid).await.unwrap().is_none());
    let err = d.save().await.unwrap_err();
    assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::DeviceDeleted));
    let err = d.sessions.has_session("x").await.unwrap_err();
    assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::DeviceDeleted));
    d.delete().await.unwrap(); // idempotent
}

#[tokio::test]
async fn put_device_requires_id_and_account() {
    let c = Container::open_memory().await.unwrap();
    let mut d = c.new_device();
    let err = d.save().await.unwrap_err();
    assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::DeviceIdMustBeSet));
    d.id = Some(Jid::new_ad("1", 0, 1));
    let err = d.save().await.unwrap_err();
    assert_eq!(err.downcast_ref::<StoreError>(), Some(&StoreError::AccountMissing));
}

#[tokio::test]
async fn get_first_device_and_missing_device() {
    let c = Container::open_memory().await.unwrap();
    let fresh = c.get_first_device().await.unwrap();
    assert!(fresh.id.is_none());
    assert!(c.get_device(&Jid::new_ad("404", 0, 1)).await.unwrap().is_none());
    let d = saved_device(&c, "1", 1).await;
    assert_eq!(c.get_first_device().await.unwrap().id, d.id);
}

#[tokio::test]
async fn deleting_device_cascades_to_its_rows() {
    let c = Container::open_memory().await.unwrap();
    let mut a = saved_device(&c, "1", 1).await;
    let b = saved_device(&c, "2", 1).await;
    a.sessions.put_session("x:1", b"sa").await.unwrap();
    a.identities.put_identity("x:1", [7; 32]).await.unwrap();
    a.pre_keys.get_or_gen_pre_keys(3).await.unwrap();
    a.contacts.put_push_name(&pn("5"), "Bob").await.unwrap();
    b.sessions.put_session("x:1", b"sb").await.unwrap();

    a.delete().await.unwrap();
    for table in ["wa_sessions", "wa_identity_keys", "wa_contacts"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE our_jid='1:1@s.whatsapp.net'"))
            .fetch_one(c.pool())
            .await
            .unwrap();
        assert_eq!(n, 0, "{table} rows must cascade");
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wa_pre_keys WHERE jid='1:1@s.whatsapp.net'")
        .fetch_one(c.pool())
        .await
        .unwrap();
    assert_eq!(n, 0);
    assert_eq!(b.sessions.get_session("x:1").await.unwrap().as_deref(), Some(&b"sb"[..]));
}

// ---- LID map (global) ----------------------------------------------------

#[tokio::test]
async fn lid_map_is_global_across_devices() {
    let c = Container::open_memory().await.unwrap();
    let a = saved_device(&c, "1", 1).await;
    let b = saved_device(&c, "2", 1).await;
    let loaded = c.get_device(&a.get_jid()).await.unwrap().unwrap();
    let as_ptr = |d: &Device| Arc::as_ptr(&d.lids) as *const ();
    assert_eq!(as_ptr(&a), as_ptr(&b));
    assert_eq!(as_ptr(&a), as_ptr(&loaded));
    assert_eq!(as_ptr(&a), Arc::as_ptr(c.lid_map()) as *const ());

    a.lids.put_lid_mapping(&lid("777"), &pn("48500")).await.unwrap();
    assert_eq!(b.lids.get_lid_for_pn(&pn("48500")).await.unwrap(), Some(lid("777")));
    assert_eq!(loaded.get_alt_jid(&lid("777")).await.unwrap(), Some(pn("48500")));
}

#[tokio::test]
async fn lid_lookups_carry_device_and_reject_wrong_servers() {
    let c = Container::open_memory().await.unwrap();
    let m = c.lid_map();
    m.put_lid_mapping(&lid("100"), &pn("200")).await.unwrap();

    let pn_dev = Jid { device: 5, ..pn("200") };
    assert_eq!(m.get_lid_for_pn(&pn_dev).await.unwrap(), Some(Jid { device: 5, ..lid("100") }));
    assert_eq!(m.get_pn_for_lid(&lid("100")).await.unwrap(), Some(pn("200")));
    assert_eq!(m.get_pn_for_lid(&lid("999")).await.unwrap(), None);
    assert!(m.get_lid_for_pn(&lid("100")).await.is_err());
    assert!(m.get_pn_for_lid(&pn("200")).await.is_err());
    assert!(m.put_lid_mapping(&pn("1"), &lid("2")).await.is_err());
    assert_eq!(m.get_alt_jid_server_none().await, None);
}

#[tokio::test]
async fn lid_map_remaps_and_reads_from_db_with_fresh_cache() {
    let c = Container::open_memory().await.unwrap();
    let m = c.lid_map();
    m.put_lid_mapping(&lid("1"), &pn("10")).await.unwrap();
    // The PN moves to a new LID: the old row is deleted (pn is UNIQUE).
    m.put_lid_mapping(&lid("2"), &pn("10")).await.unwrap();
    assert_eq!(m.get_lid_for_pn(&pn("10")).await.unwrap(), Some(lid("2")));
    assert_eq!(m.get_pn_for_lid(&lid("1")).await.unwrap(), None);
    // The LID gets a new PN.
    m.put_lid_mapping(&lid("2"), &pn("11")).await.unwrap();
    assert_eq!(m.get_pn_for_lid(&lid("2")).await.unwrap(), Some(pn("11")));
    assert_eq!(m.get_lid_for_pn(&pn("10")).await.unwrap(), None);

    // A second map over the same DB (cold cache) sees the persisted state.
    let cold = CachedLidMap::new(c.pool().clone());
    assert_eq!(cold.get_lid_for_pn(&pn("11")).await.unwrap(), Some(lid("2")));
    assert_eq!(cold.get_lid_for_pn(&pn("10")).await.unwrap(), None);
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM wa_lid_map").fetch_one(c.pool()).await.unwrap();
    assert_eq!(rows, 1);
}

#[tokio::test]
async fn lid_map_bulk_put_and_many_lookup() {
    let c = Container::open_memory().await.unwrap();
    c.lid_map()
        .put_many_lid_mappings(&[
            LidMapping { lid: lid("1"), pn: pn("10") },
            LidMapping { lid: pn("bad"), pn: pn("x") }, // ignored
            LidMapping { lid: lid("2"), pn: pn("20") },
            LidMapping { lid: lid("3"), pn: pn("20") }, // last one per PN wins
        ])
        .await
        .unwrap();

    let cold = CachedLidMap::new(c.pool().clone());
    let pns = [pn("10"), Jid { device: 2, ..pn("20") }, pn("30"), lid("1")];
    let got = cold.get_many_lids_for_pns(&pns).await.unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!(got[&pn("10")], lid("1"));
    assert_eq!(got[&Jid { device: 2, ..pn("20") }], Jid { device: 2, ..lid("3") });

    cold.fill_cache().await.unwrap();
    assert_eq!(cold.get_pn_for_lid(&lid("3")).await.unwrap(), Some(pn("20")));
    assert_eq!(cold.get_pn_for_lid(&lid("2")).await.unwrap(), None);
}

// ---- SqlStore ------------------------------------------------------------

#[tokio::test]
async fn identities_and_sessions() {
    let c = Container::open_memory().await.unwrap();
    let d = saved_device(&c, "1", 1).await;
    let ids = &d.identities;
    assert!(ids.is_trusted_identity("48:0", [1; 32]).await.unwrap(), "unknown identities are trusted");
    ids.put_identity("48:0", [1; 32]).await.unwrap();
    assert!(ids.is_trusted_identity("48:0", [1; 32]).await.unwrap());
    assert!(!ids.is_trusted_identity("48:0", [2; 32]).await.unwrap());
    ids.put_identity("48:1", [1; 32]).await.unwrap();
    ids.delete_identity("48:0").await.unwrap();
    assert!(ids.is_trusted_identity("48:0", [2; 32]).await.unwrap());
    ids.delete_all_identities("48").await.unwrap();
    assert!(ids.is_trusted_identity("48:1", [9; 32]).await.unwrap());

    let s = &d.sessions;
    assert!(!s.has_session("48:0").await.unwrap());
    s.put_session("48:0", b"one").await.unwrap();
    s.put_session("48:0", b"two").await.unwrap();
    assert!(s.has_session("48:0").await.unwrap());
    assert_eq!(s.get_session("48:0").await.unwrap().as_deref(), Some(&b"two"[..]));
    s.put_many_sessions(HashMap::from([("48:1".into(), b"a".to_vec()), ("49:0".into(), b"b".to_vec())])).await.unwrap();
    let many = s.get_many_sessions(&["48:1".into(), "49:0".into(), "50:0".into()]).await.unwrap();
    assert_eq!(many.len(), 3);
    assert_eq!(many["48:1"].as_deref(), Some(&b"a"[..]));
    assert_eq!(many["50:0"], None);
    s.delete_all_sessions("48").await.unwrap();
    assert!(!s.has_session("48:0").await.unwrap() && !s.has_session("48:1").await.unwrap());
    assert!(s.has_session("49:0").await.unwrap());
    s.delete_session("49:0").await.unwrap();
    assert_eq!(s.get_session("49:0").await.unwrap(), None);
}

#[tokio::test]
async fn migrate_pn_to_lid_moves_sessions_identities_and_sender_keys() {
    let c = Container::open_memory().await.unwrap();
    let d = saved_device(&c, "1", 1).await;
    d.sessions.put_session("48:0", b"s0").await.unwrap();
    d.sessions.put_session("48:3", b"s3").await.unwrap();
    d.identities.put_identity("48:0", [4; 32]).await.unwrap();
    d.sender_keys.put_sender_key("g@g.us", "48:0", b"sk").await.unwrap();

    d.sessions.migrate_pn_to_lid(&pn("48"), &lid("900")).await.unwrap();
    assert_eq!(d.sessions.get_session("900_1:3").await.unwrap().as_deref(), Some(&b"s3"[..]));
    assert_eq!(d.sessions.get_session("48:0").await.unwrap(), None);
    assert!(!d.identities.is_trusted_identity("900_1:0", [5; 32]).await.unwrap());
    assert_eq!(d.sender_keys.get_sender_key("g@g.us", "900_1:0").await.unwrap().as_deref(), Some(&b"sk"[..]));
    assert_eq!(d.sender_keys.get_sender_key("g@g.us", "48:0").await.unwrap(), None);

    // Cached: a second call is a no-op even if new PN sessions appear.
    d.sessions.put_session("48:0", b"new").await.unwrap();
    d.sessions.migrate_pn_to_lid(&pn("48"), &lid("900")).await.unwrap();
    assert!(d.sessions.has_session("48:0").await.unwrap());
}

#[tokio::test]
async fn pre_keys_generation_and_upload() {
    let c = Container::open_memory().await.unwrap();
    let d = saved_device(&c, "1", 1).await;
    let p = &d.pre_keys;
    let first = p.get_or_gen_pre_keys(5).await.unwrap();
    assert_eq!(first.iter().map(|k| k.key_id).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5]);
    // Existing un-uploaded keys are returned again, extra ones generated.
    let again = p.get_or_gen_pre_keys(7).await.unwrap();
    assert_eq!(again[..5], first[..]);
    assert_eq!(again.iter().map(|k| k.key_id).collect::<Vec<_>>(), vec![1, 2, 3, 4, 5, 6, 7]);

    p.mark_pre_keys_as_uploaded(5).await.unwrap();
    assert_eq!(p.uploaded_pre_key_count().await.unwrap(), 5);
    assert_eq!(p.get_or_gen_pre_keys(2).await.unwrap().iter().map(|k| k.key_id).collect::<Vec<_>>(), vec![6, 7]);
    let one = p.gen_one_pre_key().await.unwrap();
    assert_eq!(one.key_id, 8);
    assert_eq!(p.uploaded_pre_key_count().await.unwrap(), 6);

    assert_eq!(p.get_pre_key(3).await.unwrap().unwrap().key_pair, first[2].key_pair);
    p.remove_pre_key(3).await.unwrap();
    assert_eq!(p.get_pre_key(3).await.unwrap(), None);
}

#[tokio::test]
async fn app_state_keys_versions_and_macs() {
    let c = Container::open_memory().await.unwrap();
    let d = saved_device(&c, "1", 1).await;
    let k = &d.app_state_keys;
    assert_eq!(k.get_latest_app_state_sync_key_id().await.unwrap(), None);
    k.put_app_state_sync_key(
        b"k1",
        AppStateSyncKey { data: b"d1".to_vec(), fingerprint: b"f".to_vec(), timestamp: 10 },
    )
    .await
    .unwrap();
    k.put_app_state_sync_key(
        b"k2",
        AppStateSyncKey { data: b"d2".to_vec(), fingerprint: b"f".to_vec(), timestamp: 20 },
    )
    .await
    .unwrap();
    // Older timestamps never overwrite.
    k.put_app_state_sync_key(
        b"k2",
        AppStateSyncKey { data: b"old".to_vec(), fingerprint: b"f".to_vec(), timestamp: 5 },
    )
    .await
    .unwrap();
    assert_eq!(k.get_app_state_sync_key(b"k2").await.unwrap().unwrap().data, b"d2");
    assert_eq!(k.get_latest_app_state_sync_key_id().await.unwrap().as_deref(), Some(&b"k2"[..]));
    assert_eq!(k.get_all_app_state_sync_keys().await.unwrap().len(), 2);
    assert_eq!(k.get_app_state_sync_key(b"nope").await.unwrap(), None);

    let a = &d.app_state;
    assert_eq!(a.get_app_state_version("regular").await.unwrap(), (0, [0; 128]));
    a.put_app_state_version("regular", 3, [9; 128]).await.unwrap();
    assert_eq!(a.get_app_state_version("regular").await.unwrap(), (3, [9; 128]));

    let macs: Vec<AppStateMutationMac> = (0..450u32)
        .map(|i| AppStateMutationMac { index_mac: [&i.to_be_bytes()[..], &[0; 28]].concat(), value_mac: vec![1; 32] })
        .collect();
    a.put_app_state_mutation_macs("regular", 3, &macs).await.unwrap();
    a.put_app_state_mutation_macs(
        "regular",
        4,
        &[AppStateMutationMac { index_mac: macs[0].index_mac.clone(), value_mac: vec![2; 32] }],
    )
    .await
    .unwrap();
    assert_eq!(
        a.get_app_state_mutation_mac("regular", &macs[0].index_mac).await.unwrap(),
        Some(vec![2; 32]),
        "latest version wins"
    );
    a.delete_app_state_mutation_macs("regular", &[macs[0].index_mac.clone(), macs[449].index_mac.clone()])
        .await
        .unwrap();
    assert_eq!(a.get_app_state_mutation_mac("regular", &macs[0].index_mac).await.unwrap(), None);
    assert_eq!(a.get_app_state_mutation_mac("regular", &macs[1].index_mac).await.unwrap(), Some(vec![1; 32]));

    // Deleting the version cascades to its MACs.
    a.delete_app_state_version("regular").await.unwrap();
    assert_eq!(a.get_app_state_mutation_mac("regular", &macs[1].index_mac).await.unwrap(), None);
}

#[tokio::test]
async fn contacts_and_cache() {
    let c = Container::open_memory().await.unwrap();
    let d = saved_device(&c, "1", 1).await;
    let s = &d.contacts;
    let bob = pn("5");
    assert_eq!(s.get_contact(&bob).await.unwrap(), ContactInfo::default());
    assert_eq!(s.put_push_name(&bob, "Bob").await.unwrap(), (true, String::new()));
    assert_eq!(s.put_push_name(&bob, "Bob").await.unwrap(), (false, String::new()));
    assert_eq!(s.put_push_name(&bob, "Bobby").await.unwrap(), (true, "Bob".into()));
    assert_eq!(s.put_business_name(&bob, "Bob Ltd").await.unwrap(), (true, String::new()));
    s.put_contact_name(&bob, "Robert", "Robert Smith").await.unwrap();
    let info = s.get_contact(&bob).await.unwrap();
    assert!(info.found);
    assert_eq!(
        (info.push_name.as_str(), info.first_name.as_str(), info.business_name.as_str()),
        ("Bobby", "Robert", "Bob Ltd")
    );

    s.put_all_contact_names(vec![
        ContactEntry { jid: pn("6"), first_name: "A".into(), full_name: "Old".into() },
        ContactEntry { jid: pn("7"), first_name: "C".into(), full_name: "Carol".into() },
        ContactEntry { jid: pn("6"), first_name: "A".into(), full_name: "Ann".into() },
    ])
    .await
    .unwrap();
    s.put_many_redacted_phones(vec![RedactedPhoneEntry { jid: pn("7"), redacted_phone: "+48 *** 12".into() }])
        .await
        .unwrap();
    let all = s.get_all_contacts().await.unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[&pn("6")].full_name, "Ann", "the last duplicate wins");
    assert_eq!(all[&pn("7")].redacted_phone, "+48 *** 12");
    assert_eq!(all[&bob].push_name, "Bobby", "bulk name insert keeps push names");
}

#[tokio::test]
async fn chat_settings() {
    let c = Container::open_memory().await.unwrap();
    let d = saved_device(&c, "1", 1).await;
    let s = &d.chat_settings;
    let chat = Jid::new("123", "g.us");
    assert_eq!(s.get_chat_settings(&chat).await.unwrap(), LocalChatSettings::default());
    s.put_pinned(&chat, true).await.unwrap();
    s.put_archived(&chat, true).await.unwrap();
    let until = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    s.put_muted_until(&chat, Some(until)).await.unwrap();
    assert_eq!(
        s.get_chat_settings(&chat).await.unwrap(),
        LocalChatSettings { found: true, muted_until: Some(until), pinned: true, archived: true }
    );
    s.put_muted_until(&chat, Some(muted_forever())).await.unwrap();
    assert_eq!(s.get_chat_settings(&chat).await.unwrap().muted_until, Some(muted_forever()));
    s.put_muted_until(&chat, None).await.unwrap();
    assert_eq!(s.get_chat_settings(&chat).await.unwrap().muted_until, None);

    assert_eq!(s.get_wasa_root_secret_id(&chat).await.unwrap().as_deref(), Some(""));
    s.put_wasa_root_secret_id(&chat, "MSGID").await.unwrap();
    assert_eq!(s.get_wasa_root_secret_id(&chat).await.unwrap().as_deref(), Some("MSGID"));
    assert_eq!(s.get_wasa_root_secret_id(&Jid::new("x", "g.us")).await.unwrap(), None);
}

#[tokio::test]
async fn message_secrets_resolve_lid_and_pn() {
    let c = Container::open_memory().await.unwrap();
    let d = saved_device(&c, "1", 1).await;
    c.lid_map().put_lid_mapping(&lid("900"), &pn("48")).await.unwrap();
    let chat = Jid::new("g1", "g.us");
    let sender_dev = Jid { device: 4, ..pn("48") };
    d.msg_secrets.put_message_secret(&chat, &sender_dev, "M1", b"secret").await.unwrap();
    d.msg_secrets
        .put_message_secrets(&[
            MessageSecretInsert { chat: chat.clone(), sender: pn("49"), id: "M2".into(), secret: b"s2".to_vec() },
            MessageSecretInsert {
                chat: chat.clone(),
                sender: pn("49"),
                id: String::new(),
                secret: b"skipped".to_vec(),
            },
        ])
        .await
        .unwrap();

    // Looking up by the LID of the sender finds the PN-stored secret.
    let (secret, real) = d.msg_secrets.get_message_secret(&chat, &lid("900"), "M1").await.unwrap().unwrap();
    assert_eq!((secret.as_slice(), real), (&b"secret"[..], pn("48")));
    assert!(d.msg_secrets.get_message_secret(&chat, &pn("49"), "M2").await.unwrap().is_some());
    assert!(d.msg_secrets.get_message_secret(&chat, &pn("49"), "nope").await.unwrap().is_none());
}

#[tokio::test]
async fn privacy_tokens_and_nct_salt() {
    let c = Container::open_memory().await.unwrap();
    let d = saved_device(&c, "1", 1).await;
    let t = &d.privacy_tokens;
    let at = |s| UNIX_EPOCH + Duration::from_secs(s);
    t.put_privacy_tokens(&[
        PrivacyToken {
            user: Jid { device: 3, ..pn("48") },
            token: b"t1".to_vec(),
            timestamp: at(100),
            sender_timestamp: Some(at(90)),
        },
        PrivacyToken { user: pn("49"), token: b"t2".to_vec(), timestamp: at(50), sender_timestamp: None },
    ])
    .await
    .unwrap();
    // Older token never overwrites; a NULL sender timestamp keeps the old one.
    t.put_privacy_tokens(&[PrivacyToken {
        user: pn("48"),
        token: b"old".to_vec(),
        timestamp: at(10),
        sender_timestamp: None,
    }])
    .await
    .unwrap();
    t.put_privacy_tokens(&[PrivacyToken {
        user: pn("48"),
        token: b"new".to_vec(),
        timestamp: at(200),
        sender_timestamp: None,
    }])
    .await
    .unwrap();
    let got = t.get_privacy_token(&pn("48")).await.unwrap().unwrap();
    assert_eq!((got.token.as_slice(), got.timestamp, got.sender_timestamp), (&b"new"[..], at(200), Some(at(90))));

    c.lid_map().put_lid_mapping(&lid("777"), &pn("49")).await.unwrap();
    assert_eq!(t.get_privacy_token(&lid("777")).await.unwrap().unwrap().token, b"t2");
    assert_eq!(t.delete_expired_privacy_tokens(at(60)).await.unwrap(), 1);
    assert!(t.get_privacy_token(&pn("49")).await.unwrap().is_none());

    let n = &d.nct_salt;
    assert_eq!(n.get_nct_salt().await.unwrap(), None);
    n.put_nct_salt(b"salt").await.unwrap();
    n.put_nct_salt(b"salt2").await.unwrap();
    assert_eq!(n.get_nct_salt().await.unwrap().as_deref(), Some(&b"salt2"[..]));
    n.delete_nct_salt().await.unwrap();
    assert_eq!(n.get_nct_salt().await.unwrap(), None);
}

#[tokio::test]
async fn event_buffer_and_retry_buffer() {
    let c = Container::open_memory().await.unwrap();
    let d = saved_device(&c, "1", 1).await;
    let e = &d.event_buffer;
    let server = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    assert_eq!(e.get_buffered_event([1; 32]).await.unwrap(), None);
    e.put_buffered_event([1; 32], b"plain", server).await.unwrap();
    let got = e.get_buffered_event([1; 32]).await.unwrap().unwrap();
    assert_eq!((got.plaintext.as_deref(), got.server_time), (Some(&b"plain"[..]), server));
    assert!(SystemTime::now().duration_since(got.insert_time).unwrap() < Duration::from_secs(60));
    e.clear_buffered_event_plaintext([1; 32]).await.unwrap();
    assert_eq!(e.get_buffered_event([1; 32]).await.unwrap().unwrap().plaintext, None);
    e.delete_old_buffered_hashes().await.unwrap();
    assert!(e.get_buffered_event([1; 32]).await.unwrap().is_some(), "fresh hashes survive");

    let ran = std::sync::atomic::AtomicBool::new(false);
    e.do_decryption_txn(Box::pin(async {
        ran.store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }))
    .await
    .unwrap();
    assert!(ran.into_inner());
    assert!(e.do_decryption_txn(Box::pin(async { anyhow::bail!("nope") })).await.is_err());

    let chat = pn("48");
    assert_eq!(e.get_outgoing_event(&chat, &lid("9"), "M").await.unwrap(), None);
    e.add_outgoing_event(&lid("9"), "M", "proto", b"payload").await.unwrap();
    assert_eq!(e.get_outgoing_event(&chat, &lid("9"), "M").await.unwrap(), Some(("proto".into(), b"payload".to_vec())));
    e.delete_old_outgoing_events().await.unwrap();
    assert!(e.get_outgoing_event(&chat, &lid("9"), "M").await.unwrap().is_some());
}

#[tokio::test]
async fn stores_are_scoped_per_device() {
    let c = Container::open_memory().await.unwrap();
    let a = saved_device(&c, "1", 1).await;
    let b = saved_device(&c, "2", 1).await;
    a.sessions.put_session("x:0", b"a").await.unwrap();
    assert_eq!(b.sessions.get_session("x:0").await.unwrap(), None);
    a.contacts.put_push_name(&pn("5"), "A's Bob").await.unwrap();
    assert!(!b.contacts.get_contact(&pn("5")).await.unwrap().found);
}

// Keeps the `device` module path used above referenced for docs.
#[allow(dead_code)]
type _DeviceAlias = device::Device;

trait AltNone {
    async fn get_alt_jid_server_none(&self) -> Option<Jid>;
}

impl AltNone for Arc<CachedLidMap> {
    /// Groups have no LID alternative (`GetAltJID` → empty).
    async fn get_alt_jid_server_none(&self) -> Option<Jid> {
        let c = Container::with_pool(SqlitePoolOptions::new().connect("sqlite::memory:").await.ok()?);
        c.new_device().get_alt_jid(&Jid::new("1", "g.us")).await.ok().flatten()
    }
}

use std::str::FromStr;
