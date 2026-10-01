//! Generic cryptographic building blocks. Protocol envelope integration is deliberately external.
use libsodium_rs::{
    crypto_aead::xchacha20poly1305 as aead, crypto_pwhash::argon2id,
    crypto_secretstream::xchacha20poly1305 as stream,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
};
use thiserror::Error;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;
use zeroize::{Zeroize, Zeroizing};

pub const CRYPTO_SUITE_1: u16 = 1;
pub const SALT_BYTES: usize = 16;
pub const ROOT_KEY_BYTES: usize = 32;
pub const KDF_OPSLIMIT: u64 = 3;
pub const KDF_MEMLIMIT: usize = 256 * 1024 * 1024;
pub const STREAM_CHUNK_BYTES: usize = 64 * 1024;
const VAULT_CHECK: &[u8] = b"openpush-vault-check-v1";
static SODIUM_READY: OnceLock<Result<(), ()>> = OnceLock::new();
fn ensure_sodium() -> Result<(), CryptoError> {
    SODIUM_READY
        .get_or_init(|| libsodium_rs::ensure_init().map_err(|_| ()))
        .as_ref()
        .map_err(|_| CryptoError::OperationFailed)
        .copied()
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CryptoError {
    #[error("unsupported cryptographic suite")]
    UnsupportedSuite,
    #[error("invalid public key profile")]
    InvalidProfile,
    #[error("passphrase has surrounding whitespace")]
    SurroundingWhitespace,
    #[error("cryptographic operation failed")]
    OperationFailed,
    #[error("authentication failed")]
    AuthenticationFailed,
    #[error("invalid encrypted data")]
    InvalidCiphertext,
    #[error("stream is truncated or has trailing data")]
    InvalidStream,
    #[error("I/O failed")]
    Io,
}
impl From<io::Error> for CryptoError {
    fn from(_: io::Error) -> Self {
        Self::Io
    }
}

/// Non-secret metadata which must be pinned during enrollment. Its fingerprint never uses a passphrase.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyProfile {
    pub crypto_suite: u16,
    pub salt: [u8; SALT_BYTES],
    pub vault_id: Uuid,
    pub key_epoch: u32,
}
impl KeyProfile {
    pub fn new(vault_id: Uuid, key_epoch: u32) -> Result<Self, CryptoError> {
        ensure_sodium()?;
        let mut salt = [0; SALT_BYTES];
        libsodium_rs::random::fill_bytes(&mut salt);
        Ok(Self {
            crypto_suite: CRYPTO_SUITE_1,
            salt,
            vault_id,
            key_epoch,
        })
    }
    pub fn validate(&self) -> Result<(), CryptoError> {
        if self.crypto_suite != CRYPTO_SUITE_1 {
            return Err(CryptoError::UnsupportedSuite);
        }
        Ok(())
    }
    /// Lowercase SHA-256 hex of canonical public suite/salt/vault/epoch bytes.
    pub fn fingerprint(&self) -> Result<String, CryptoError> {
        let mut output = String::with_capacity(64);
        for byte in profile_fingerprint_bytes(self)? {
            use std::fmt::Write as _;
            write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
        }
        Ok(output)
    }
}

pub struct RootKey([u8; ROOT_KEY_BYTES]);
impl Zeroize for RootKey {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}
impl Drop for RootKey {
    fn drop(&mut self) {
        self.zeroize();
    }
}
impl RootKey {
    fn from_bytes(bytes: &[u8]) -> Result<Self, CryptoError> {
        let mut output = [0; ROOT_KEY_BYTES];
        if bytes.len() != output.len() {
            return Err(CryptoError::OperationFailed);
        }
        output.copy_from_slice(bytes);
        Ok(Self(output))
    }
}

/// Normalizes NFKC centrally, rejects (rather than trims) surrounding Unicode whitespace, then runs fixed Argon2id13.
pub fn derive_root_key(passphrase: &str, profile: &KeyProfile) -> Result<RootKey, CryptoError> {
    ensure_sodium()?;
    profile.validate()?;
    let normalized = Zeroizing::new(passphrase.nfkc().collect::<String>());
    if normalized.chars().next().is_some_and(char::is_whitespace)
        || normalized
            .chars()
            .next_back()
            .is_some_and(char::is_whitespace)
    {
        return Err(CryptoError::SurroundingWhitespace);
    }
    let password = Zeroizing::new(normalized.as_bytes().to_vec());
    let mut key = argon2id::pwhash(
        ROOT_KEY_BYTES,
        &password,
        &profile.salt,
        KDF_OPSLIMIT,
        KDF_MEMLIMIT,
    )
    .map_err(|_| CryptoError::OperationFailed)?;
    let result = RootKey::from_bytes(&key);
    key.zeroize();
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyPurpose {
    Command,
    Event,
    Header,
}
impl KeyPurpose {
    fn id(self) -> u64 {
        match self {
            Self::Command => 1,
            Self::Event => 2,
            Self::Header => 3,
        }
    }
}
pub struct PurposeKey {
    bytes: [u8; ROOT_KEY_BYTES],
    purpose: KeyPurpose,
    profile_fingerprint: [u8; 32],
}
impl Zeroize for PurposeKey {
    fn zeroize(&mut self) {
        self.bytes.zeroize();
    }
}
impl Drop for PurposeKey {
    fn drop(&mut self) {
        self.zeroize();
    }
}
impl PurposeKey {
    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn purpose(&self) -> KeyPurpose {
        self.purpose
    }
    /// Passes bytes to the immediate native secure-store operation. Callers must not retain or copy them.
    pub fn with_native_cache_bytes<T>(
        &self,
        operation: impl FnOnce(&[u8; ROOT_KEY_BYTES]) -> T,
    ) -> T {
        operation(&self.bytes)
    }
    pub fn import_native_cache(
        bytes: [u8; ROOT_KEY_BYTES],
        profile: &KeyProfile,
        purpose: KeyPurpose,
    ) -> Result<Self, CryptoError> {
        ensure_sodium()?;
        Ok(Self {
            bytes,
            purpose,
            profile_fingerprint: profile_fingerprint_bytes(profile)?,
        })
    }
}
pub fn derive_purpose_key(
    root: &RootKey,
    profile: &KeyProfile,
    purpose: KeyPurpose,
) -> Result<PurposeKey, CryptoError> {
    ensure_sodium()?;
    profile.validate()?;
    let master = libsodium_rs::crypto_kdf::Key::from_slice(&root.0)
        .map_err(|_| CryptoError::OperationFailed)?;
    let mut bytes = libsodium_rs::crypto_kdf::derive_from_key(
        ROOT_KEY_BYTES,
        purpose.id(),
        b"OPushK01",
        &master,
    )
    .map_err(|_| CryptoError::OperationFailed)?;
    let mut out = [0; ROOT_KEY_BYTES];
    out.copy_from_slice(&bytes);
    bytes.zeroize();
    Ok(PurposeKey {
        bytes: out,
        purpose,
        profile_fingerprint: profile_fingerprint_bytes(profile)?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncryptedEnvelope {
    pub nonce: [u8; 24],
    pub ciphertext: Vec<u8>,
}
/// Encrypts generic bytes; callers supply the exact canonical authenticated context bytes.
pub fn encrypt(
    key: &PurposeKey,
    aad: &[u8],
    plaintext: &[u8],
) -> Result<EncryptedEnvelope, CryptoError> {
    ensure_sodium()?;
    let sodium_key =
        aead::Key::from_bytes(key.as_bytes()).map_err(|_| CryptoError::OperationFailed)?;
    let nonce = aead::Nonce::generate();
    let context = purpose_aad(key, aad);
    let ciphertext = aead::encrypt(plaintext, Some(&context), &nonce, &sodium_key)
        .map_err(|_| CryptoError::OperationFailed)?;
    let mut nonce_bytes = [0; 24];
    nonce_bytes.copy_from_slice(nonce.as_bytes());
    Ok(EncryptedEnvelope {
        nonce: nonce_bytes,
        ciphertext,
    })
}
pub fn decrypt(
    key: &PurposeKey,
    aad: &[u8],
    encrypted: &EncryptedEnvelope,
) -> Result<Vec<u8>, CryptoError> {
    ensure_sodium()?;
    let sodium_key =
        aead::Key::from_bytes(key.as_bytes()).map_err(|_| CryptoError::OperationFailed)?;
    let nonce = aead::Nonce::from_bytes(encrypted.nonce);
    aead::decrypt(
        &encrypted.ciphertext,
        Some(&purpose_aad(key, aad)),
        &nonce,
        &sodium_key,
    )
    .map_err(|_| CryptoError::AuthenticationFailed)
}
fn purpose_aad(key: &PurposeKey, aad: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(56 + aad.len());
    result.extend_from_slice(b"openpush-aead-v1\0");
    result.push(key.purpose.id() as u8);
    result.extend_from_slice(&key.profile_fingerprint);
    result.extend_from_slice(aad);
    result
}

/// Fresh independent file key for a single private-media object.
pub struct FileKey([u8; ROOT_KEY_BYTES]);
impl FileKey {
    pub fn generate() -> Result<Self, CryptoError> {
        ensure_sodium()?;
        let key = stream::Key::generate();
        let mut out = [0; ROOT_KEY_BYTES];
        out.copy_from_slice(key.as_bytes());
        Ok(Self(out))
    }
    pub fn with_encrypted_reference_bytes<T>(
        &self,
        operation: impl FnOnce(&[u8; ROOT_KEY_BYTES]) -> T,
    ) -> T {
        operation(&self.0)
    }
    pub fn import_encrypted_reference(bytes: [u8; ROOT_KEY_BYTES]) -> Result<Self, CryptoError> {
        ensure_sodium()?;
        Ok(Self(bytes))
    }
}
impl Zeroize for FileKey {
    fn zeroize(&mut self) {
        self.0.zeroize();
    }
}
impl Drop for FileKey {
    fn drop(&mut self) {
        self.zeroize();
    }
}
fn stream_key(key: &FileKey) -> Result<stream::Key, CryptoError> {
    stream::Key::from_bytes(&key.0).map_err(|_| CryptoError::OperationFailed)
}

/// Encrypts bounded chunks from a reader. `object_aad` authenticates the object identity on every chunk.
pub fn encrypt_stream(
    mut input: impl Read,
    mut output: impl Write,
    key: &FileKey,
    object_aad: &[u8],
) -> Result<(), CryptoError> {
    ensure_sodium()?;
    let (mut state, header) = stream::PushState::init_push(&stream_key(key)?)
        .map_err(|_| CryptoError::OperationFailed)?;
    output.write_all(b"OPSS\x01")?;
    output.write_all(&header)?;
    let mut buf = vec![0; STREAM_CHUNK_BYTES];
    loop {
        let count = input.read(&mut buf)?;
        if count == 0 {
            break;
        }
        let frame = state
            .push(&buf[..count], Some(object_aad), stream::TAG_MESSAGE)
            .map_err(|_| CryptoError::OperationFailed)?;
        write_frame(&mut output, &frame)?;
    }
    let final_frame = state
        .push(&[], Some(object_aad), stream::TAG_FINAL)
        .map_err(|_| CryptoError::OperationFailed)?;
    write_frame(&mut output, &final_frame)?;
    Ok(())
}
/// Decrypts a fully verified stream to a temporary sibling and atomically promotes it only after its final tag.
pub fn decrypt_stream_to_path(
    mut input: impl Read,
    destination: &Path,
    key: &FileKey,
    object_aad: &[u8],
) -> Result<(), CryptoError> {
    ensure_sodium()?;
    let mut prefix = [0; 5];
    input
        .read_exact(&mut prefix)
        .map_err(|_| CryptoError::InvalidStream)?;
    if &prefix != b"OPSS\x01" {
        return Err(CryptoError::InvalidStream);
    }
    let mut header = [0; stream::HEADERBYTES];
    input
        .read_exact(&mut header)
        .map_err(|_| CryptoError::InvalidStream)?;
    let mut state = stream::PullState::init_pull(&header, &stream_key(key)?)
        .map_err(|_| CryptoError::AuthenticationFailed)?;
    let temporary = temp_sibling(destination);
    let result = (|| -> Result<(), CryptoError> {
        let mut out = create_private_temp(&temporary)?;
        let mut final_seen = false;
        while !final_seen {
            let frame = read_frame(&mut input)?;
            let (plain, tag) = state
                .pull(&frame, Some(object_aad))
                .map_err(|_| CryptoError::AuthenticationFailed)?;
            out.write_all(&plain)?;
            final_seen = tag == stream::TAG_FINAL;
            if tag != stream::TAG_MESSAGE && tag != stream::TAG_FINAL {
                return Err(CryptoError::InvalidStream);
            }
        }
        let mut trailing = [0; 1];
        if input.read(&mut trailing)? != 0 {
            return Err(CryptoError::InvalidStream);
        }
        out.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        return result;
    }
    if let Err(error) = promote_noclobber(&temporary, destination) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    let _ = sync_parent_directory(destination);
    Ok(())
}
fn temp_sibling(destination: &Path) -> PathBuf {
    let name = destination
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    destination.with_file_name(format!("{name}.openpush-{}.tmp", Uuid::new_v4()))
}
fn create_private_temp(path: &Path) -> Result<File, CryptoError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        Ok(OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)?)
    }
    #[cfg(not(unix))]
    {
        Ok(OpenOptions::new().create_new(true).write(true).open(path)?)
    }
}
fn promote_noclobber(temporary: &Path, destination: &Path) -> io::Result<()> {
    fs::hard_link(temporary, destination)?;
    fs::remove_file(temporary)
}
#[cfg(unix)]
fn sync_parent_directory(destination: &Path) -> io::Result<()> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}
#[cfg(not(unix))]
fn sync_parent_directory(_: &Path) -> io::Result<()> {
    Ok(())
}
fn profile_fingerprint_bytes(profile: &KeyProfile) -> Result<[u8; 32], CryptoError> {
    profile.validate()?;
    let mut h = Sha256::new();
    h.update(b"openpush-key-profile-v1\0");
    h.update(profile.crypto_suite.to_be_bytes());
    h.update(profile.salt);
    h.update(profile.vault_id.as_bytes());
    h.update(profile.key_epoch.to_be_bytes());
    Ok(h.finalize().into())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VaultCheckHeader {
    pub profile: KeyProfile,
    pub check: EncryptedEnvelope,
}
pub fn create_vault_check_header(
    root: &RootKey,
    profile: KeyProfile,
) -> Result<VaultCheckHeader, CryptoError> {
    let key = derive_purpose_key(root, &profile, KeyPurpose::Header)?;
    let aad = vault_header_aad(&profile)?;
    Ok(VaultCheckHeader {
        check: encrypt(&key, &aad, VAULT_CHECK)?,
        profile,
    })
}
pub fn verify_vault_check_header(
    root: &RootKey,
    expected: &KeyProfile,
    header: &VaultCheckHeader,
) -> Result<(), CryptoError> {
    if expected != &header.profile {
        return Err(CryptoError::InvalidProfile);
    }
    let key = derive_purpose_key(root, expected, KeyPurpose::Header)?;
    let plain = decrypt(&key, &vault_header_aad(expected)?, &header.check)?;
    if plain == VAULT_CHECK {
        Ok(())
    } else {
        Err(CryptoError::AuthenticationFailed)
    }
}
fn vault_header_aad(profile: &KeyProfile) -> Result<Vec<u8>, CryptoError> {
    let mut aad = Vec::from(&b"openpush-vault-header-v1\0"[..]);
    aad.extend_from_slice(&profile.crypto_suite.to_be_bytes());
    aad.extend_from_slice(&profile.salt);
    aad.extend_from_slice(profile.vault_id.as_bytes());
    aad.extend_from_slice(&profile.key_epoch.to_be_bytes());
    aad.extend_from_slice(profile.fingerprint()?.as_bytes());
    Ok(aad)
}
fn write_frame(output: &mut impl Write, frame: &[u8]) -> Result<(), CryptoError> {
    let len = u32::try_from(frame.len()).map_err(|_| CryptoError::InvalidStream)?;
    output.write_all(&len.to_be_bytes())?;
    output.write_all(frame)?;
    Ok(())
}
fn read_frame(input: &mut impl Read) -> Result<Vec<u8>, CryptoError> {
    let mut len = [0; 4];
    input
        .read_exact(&mut len)
        .map_err(|_| CryptoError::InvalidStream)?;
    let len = u32::from_be_bytes(len) as usize;
    if !(stream::ABYTES..=STREAM_CHUNK_BYTES + stream::ABYTES).contains(&len) {
        return Err(CryptoError::InvalidStream);
    }
    let mut frame = vec![0; len];
    input
        .read_exact(&mut frame)
        .map_err(|_| CryptoError::InvalidStream)?;
    Ok(frame)
}
