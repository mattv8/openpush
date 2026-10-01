use openpush_crypto::*;
use std::io::Cursor;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use uuid::Uuid;
use zeroize::Zeroize;

fn profile() -> KeyProfile {
    KeyProfile {
        crypto_suite: CRYPTO_SUITE_1,
        salt: [7; 16],
        vault_id: Uuid::nil(),
        key_epoch: 9,
    }
}
fn root() -> RootKey {
    derive_root_key("correct horse battery staple", &profile()).unwrap()
}

#[test]
fn public_profile_fingerprint_is_frozen() {
    assert_eq!(
        profile().fingerprint().unwrap(),
        "f48b1abff547aeb987f436cf39b28be32c49bb014c3a227b1b8202a64b0b7157"
    );
}
#[test]
fn kdf_unicode_and_suite_rules() {
    let p = profile();
    let composed = derive_root_key("café café café café café", &p).unwrap();
    let decomposed = derive_root_key(
        "cafe\u{301} cafe\u{301} cafe\u{301} cafe\u{301} cafe\u{301}",
        &p,
    )
    .unwrap();
    let sealed = encrypt(
        &derive_purpose_key(&composed, &p, KeyPurpose::Event).unwrap(),
        b"normalization",
        b"same key",
    )
    .unwrap();
    assert_eq!(
        decrypt(
            &derive_purpose_key(&decomposed, &p, KeyPurpose::Event).unwrap(),
            b"normalization",
            &sealed,
        )
        .unwrap(),
        b"same key"
    );
    let vector_root = root();
    let vector_key = derive_purpose_key(&vector_root, &p, KeyPurpose::Command).unwrap();
    let mut exported = String::new();
    vector_key.with_native_cache_bytes(|bytes| {
        for byte in bytes {
            use std::fmt::Write as _;
            write!(&mut exported, "{byte:02x}").unwrap();
        }
    });
    assert_eq!(
        exported,
        "14fae1f9ef4307531ad18b8c0f644863baf09e3d31cbb38e37575d808379713f"
    );
    let mut invalid = p.clone();
    invalid.crypto_suite = 2;
    assert!(matches!(
        derive_root_key("anything", &invalid),
        Err(CryptoError::UnsupportedSuite)
    ));
    assert!(matches!(
        derive_root_key(" passphrase", &p),
        Err(CryptoError::SurroundingWhitespace)
    ));
}
#[test]
fn key_handle_binds_purpose_and_profile() {
    let p = profile();
    let root = root();
    let command = derive_purpose_key(&root, &p, KeyPurpose::Command).unwrap();
    let sealed = encrypt(&command, b"canonical aad", b"hello").unwrap();
    assert_eq!(
        decrypt(&command, b"canonical aad", &sealed).unwrap(),
        b"hello"
    );
    assert!(decrypt(&command, b"changed aad", &sealed).is_err());
    let event = derive_purpose_key(&root, &p, KeyPurpose::Event).unwrap();
    assert!(decrypt(&event, b"canonical aad", &sealed).is_err());
    let mut changed = p.clone();
    changed.key_epoch += 1;
    let changed_key = derive_purpose_key(&root, &changed, KeyPurpose::Command).unwrap();
    assert!(decrypt(&changed_key, b"canonical aad", &sealed).is_err());
}
#[test]
fn vault_check_rejects_wrong_key_profile_and_epoch() {
    let p = profile();
    let root = root();
    let header = create_vault_check_header(&root, p.clone()).unwrap();
    assert!(verify_vault_check_header(&root, &p, &header).is_ok());
    assert!(
        verify_vault_check_header(
            &derive_root_key("another passphrase", &p).unwrap(),
            &p,
            &header
        )
        .is_err()
    );
    let mut epoch = p.clone();
    epoch.key_epoch += 1;
    assert_eq!(
        verify_vault_check_header(&root, &epoch, &header),
        Err(CryptoError::InvalidProfile)
    );
}
#[test]
fn stream_rejects_tampering_without_promotion() {
    let key = FileKey::generate().unwrap();
    let mut wire = Vec::new();
    encrypt_stream(
        Cursor::new(vec![42; STREAM_CHUNK_BYTES + 5]),
        &mut wire,
        &key,
        b"object",
    )
    .unwrap();
    let dest = std::env::temp_dir().join(format!("openpush-{}", Uuid::new_v4()));
    decrypt_stream_to_path(Cursor::new(&wire), &dest, &key, b"object").unwrap();
    assert_eq!(std::fs::read(&dest).unwrap().len(), STREAM_CHUNK_BYTES + 5);
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::remove_file(&dest).unwrap();
    for mut bad in [
        wire[..wire.len() - 1].to_vec(),
        {
            let mut x = wire.clone();
            x.push(1);
            x
        },
        {
            let mut x = wire.clone();
            x[5] ^= 1;
            x
        },
    ] {
        assert!(decrypt_stream_to_path(Cursor::new(&bad), &dest, &key, b"object").is_err());
        assert!(!dest.exists());
        bad.zeroize();
    }
}
