//! Bounded encrypted attachment files owned by the core.
//!
//! Layout under `<database file>.media/` (directories 0700, files 0600):
//! * `cipher/<attachment_id>.opss` — verified secretstream ciphertext (safe to upload/hand to native).
//! * `tmp/` — partial writes and downloads under verification; removed on failure.
//! * `plain/` — native-only plaintext for preview/PDU encoding. A `NativePlaintextFile` deletes its
//!   file on drop and the directory is purged on every open. Deletion is not secure erasure, and
//!   decrypted local files are endpoint data, not end-to-end protected.
//!
//! File keys live only in SQLCipher rows and inside AEAD-encrypted payloads.
use crate::{AttachmentId, Error, VaultId};
use openpush_crypto::{FileKey, decrypt_stream_to_path, encrypt_stream};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    any::Any,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};
use zeroize::Zeroize;

pub(crate) const STREAM_VERSION: u8 = 1;
pub(crate) const MAX_ATTACHMENT_PLAINTEXT_BYTES: u64 = 32 * 1024 * 1024;
/// Plaintext bound plus secretstream framing for 64 KiB chunks.
pub(crate) const MAX_ATTACHMENT_CIPHERTEXT_BYTES: u64 = 33 * 1024 * 1024;
const MAX_MEDIA_TYPE_BYTES: usize = 127;
const MAX_DISPLAY_NAME_BYTES: usize = 255;

/// Private per-object metadata carried only inside encrypted payloads. Fields are crate-private;
/// `Debug` redacts the file key.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MediaDescriptor {
    pub(crate) attachment_id: AttachmentId,
    pub(crate) remote_object_id: String,
    pub(crate) media_type: String,
    pub(crate) display_name: String,
    pub(crate) plaintext_bytes: u64,
    pub(crate) ciphertext_bytes: u64,
    pub(crate) ciphertext_sha256: String,
    pub(crate) stream_version: u8,
    pub(crate) file_key: [u8; 32],
}
impl Drop for MediaDescriptor {
    fn drop(&mut self) {
        self.file_key.zeroize();
    }
}
impl fmt::Debug for MediaDescriptor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MediaDescriptor")
            .field("attachment_id", &self.attachment_id)
            .field("media_type", &self.media_type)
            .field("ciphertext_bytes", &self.ciphertext_bytes)
            .field("file_key", &"<redacted>")
            .finish()
    }
}
impl MediaDescriptor {
    pub(crate) fn is_valid(&self) -> bool {
        self.stream_version == STREAM_VERSION
            && uuid::Uuid::parse_str(&self.remote_object_id).is_ok()
            && valid_media_type(&self.media_type)
            && sanitize_display_name(&self.display_name) == self.display_name
            && self.plaintext_bytes <= MAX_ATTACHMENT_PLAINTEXT_BYTES
            && self.ciphertext_bytes > self.plaintext_bytes
            && self.ciphertext_bytes <= MAX_ATTACHMENT_CIPHERTEXT_BYTES
            && is_sha256_hex(&self.ciphertext_sha256)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachmentState {
    /// Encrypted on this device; waiting for the native host to upload the ciphertext.
    PendingUpload,
    /// Encrypted on this device and uploaded.
    Uploaded,
    /// Metadata received; ciphertext not yet downloaded and verified.
    PendingDownload,
    /// Downloaded and fully verified.
    Available,
}
impl AttachmentState {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::PendingUpload => "pending_upload",
            Self::Uploaded => "uploaded",
            Self::PendingDownload => "pending_download",
            Self::Available => "available",
        }
    }
    pub(crate) fn from_code(code: &str) -> Result<Self, Error> {
        Ok(match code {
            "pending_upload" => Self::PendingUpload,
            "uploaded" => Self::Uploaded,
            "pending_download" => Self::PendingDownload,
            "available" => Self::Available,
            _ => return Err(Error::Database),
        })
    }
    /// Verified ciphertext exists in the local store.
    pub fn is_local(self) -> bool {
        !matches!(self, Self::PendingDownload)
    }
}

/// Sanitized, key-free attachment metadata suitable for UI view models.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentInfo {
    pub attachment_id: AttachmentId,
    pub media_type: String,
    pub display_name: String,
    pub plaintext_bytes: u64,
    pub ciphertext_bytes: u64,
    pub ciphertext_sha256: String,
    pub state: AttachmentState,
}

/// A ciphertext object the native host must upload (`remote_object_id: None`) or download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CipherObject {
    pub attachment_id: AttachmentId,
    pub ciphertext_bytes: u64,
    pub ciphertext_sha256: String,
    pub remote_object_id: Option<String>,
}

/// Native-only plaintext file for preview or MMS PDU encoding. Deleted on drop; never pass the
/// path to a web view. Implements neither `Clone` nor `Debug`.
pub struct NativePlaintextFile {
    path: PathBuf,
    /// Keeps the database owner registered, so a later `Client::open` of the same path reuses it
    /// instead of starting a fresh owner that purges `plain/` under a live handle.
    _owner: Arc<dyn Any + Send + Sync>,
}
impl NativePlaintextFile {
    pub fn path(&self) -> &Path {
        &self.path
    }
}
impl Drop for NativePlaintextFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub(crate) struct Encrypted {
    pub plaintext_bytes: u64,
    pub ciphertext_bytes: u64,
    pub ciphertext_sha256: String,
}

pub(crate) fn media_aad(vault_id: VaultId, attachment_id: AttachmentId) -> Vec<u8> {
    let mut aad = b"openpush-attachment-v1\0".to_vec();
    aad.extend_from_slice(vault_id.0.as_bytes());
    aad.extend_from_slice(attachment_id.0.as_bytes());
    aad
}

pub(crate) fn valid_media_type(value: &str) -> bool {
    let token = |part: &str| {
        !part.is_empty()
            && part.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-')
            })
    };
    value.len() <= MAX_MEDIA_TYPE_BYTES
        && value
            .split_once('/')
            .is_some_and(|(kind, subtype)| token(kind) && token(subtype))
}

/// Keeps only the final path component, replaces separators/controls, bounds length.
pub(crate) fn sanitize_display_name(value: &str) -> String {
    let last = value.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut name: String = last
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();
    while name.len() > MAX_DISPLAY_NAME_BYTES {
        name.pop();
    }
    if name.is_empty() || name == "." || name == ".." {
        "attachment".into()
    } else {
        name
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 15)]));
    }
    out
}
fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `<parent>/<db file>.media`; created 0700 and stale scratch purged on open.
pub(crate) fn prepare_media_root(database: &Path) -> Result<PathBuf, Error> {
    let parent = database.parent().ok_or(Error::Storage)?;
    let name = database
        .file_name()
        .ok_or(Error::Storage)?
        .to_string_lossy();
    let root = parent.join(format!("{name}.media"));
    for dir in [
        root.clone(),
        root.join("cipher"),
        root.join("tmp"),
        root.join("plain"),
    ] {
        private_dir(&dir)?;
    }
    // Fresh owner only: remove leftover scratch files and symlinks (never their targets).
    // Unexpected subdirectories are left alone rather than deleted or treated as fatal.
    for scratch in ["tmp", "plain"] {
        for entry in fs::read_dir(root.join(scratch)).map_err(|_| Error::Storage)? {
            let entry = entry.map_err(|_| Error::Storage)?;
            let kind = entry.file_type().map_err(|_| Error::Storage)?;
            if !kind.is_dir() {
                fs::remove_file(entry.path()).map_err(|_| Error::Storage)?;
            }
        }
    }
    Ok(root)
}
fn private_dir(path: &Path) -> Result<(), Error> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        Ok(_) => return Err(Error::Storage),
        Err(_) => {
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder.create(path).map_err(|_| Error::Storage)?;
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|_| Error::Storage)?;
    }
    Ok(())
}
fn private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}
pub(crate) fn cipher_path(root: &Path, attachment_id: AttachmentId) -> PathBuf {
    // A UUID's canonical text form cannot contain separators.
    root.join("cipher").join(format!("{attachment_id}.opss"))
}
fn scratch_path(root: &Path, dir: &str, extension: &str) -> PathBuf {
    root.join(dir)
        .join(format!("{}.{extension}", uuid::Uuid::new_v4()))
}
fn sync_dir(path: &Path) {
    if let Ok(dir) = File::open(path) {
        let _ = dir.sync_all();
    }
}

struct Hashing<W> {
    inner: W,
    digest: Sha256,
    bytes: u64,
}
impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.digest.update(&buf[..written]);
        self.bytes += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}
struct Counting<R> {
    inner: R,
    bytes: u64,
}
impl<R: Read> Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buf)?;
        self.bytes += read as u64;
        Ok(read)
    }
}

/// Removes a scratch name when dropped (after a successful hard link only the name goes).
struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Encrypts an app-owned regular file into the cipher store under a fresh object identity.
pub(crate) fn encrypt_into_store(
    root: &Path,
    attachment_id: AttachmentId,
    source: &Path,
    key: &FileKey,
    aad: &[u8],
) -> Result<Encrypted, Error> {
    let meta = fs::metadata(source).map_err(|_| Error::InvalidRequest("attachment source"))?;
    if !meta.is_file() || meta.len() > MAX_ATTACHMENT_PLAINTEXT_BYTES {
        return Err(Error::InvalidRequest(
            "attachment must be a regular file within the size limit",
        ));
    }
    let input = File::open(source).map_err(|_| Error::InvalidRequest("attachment source"))?;
    let temporary = scratch_path(root, "tmp", "part");
    let cleanup = Cleanup(temporary.clone());
    let mut reader = Counting {
        inner: input.take(MAX_ATTACHMENT_PLAINTEXT_BYTES + 1),
        bytes: 0,
    };
    let mut writer = Hashing {
        inner: private_file(&temporary).map_err(|_| Error::Storage)?,
        digest: Sha256::new(),
        bytes: 0,
    };
    encrypt_stream(&mut reader, &mut writer, key, aad).map_err(|_| Error::Storage)?;
    if reader.bytes > MAX_ATTACHMENT_PLAINTEXT_BYTES {
        return Err(Error::InvalidRequest(
            "attachment must be a regular file within the size limit",
        ));
    }
    // Short reads add frame overhead; enforce the receiver's ciphertext bound before promotion.
    if writer.bytes > MAX_ATTACHMENT_CIPHERTEXT_BYTES {
        return Err(Error::InvalidRequest(
            "attachment must be a regular file within the size limit",
        ));
    }
    writer.inner.sync_all().map_err(|_| Error::Storage)?;
    let destination = cipher_path(root, attachment_id);
    fs::hard_link(&temporary, &destination).map_err(|_| Error::Storage)?;
    drop(cleanup); // removes the temporary name; the hard link remains
    sync_dir(&root.join("cipher"));
    Ok(Encrypted {
        plaintext_bytes: reader.bytes,
        ciphertext_bytes: writer.bytes,
        ciphertext_sha256: hex(&writer.digest.finalize()),
    })
}

/// Copies a native download into private scratch, checks exact length and SHA-256, fully
/// decrypts it (final tag, no truncation/trailing data) into a discarded scratch file, then
/// installs the ciphertext. Nothing is promoted on failure.
/// Authenticated sizes/digest an installed object must match.
pub(crate) struct Expected<'a> {
    pub ciphertext_bytes: u64,
    pub ciphertext_sha256: &'a str,
    pub plaintext_bytes: u64,
}

pub(crate) fn install_into_store(
    root: &Path,
    attachment_id: AttachmentId,
    downloaded: &Path,
    expected: &Expected<'_>,
    key: &FileKey,
    aad: &[u8],
) -> Result<(), Error> {
    let (expected_bytes, expected_sha256) = (expected.ciphertext_bytes, expected.ciphertext_sha256);
    let mut source = File::open(downloaded).map_err(|_| Error::InvalidMedia)?;
    let temporary = scratch_path(root, "tmp", "part");
    let cleanup = Cleanup(temporary.clone());
    let mut writer = Hashing {
        inner: private_file(&temporary).map_err(|_| Error::Storage)?,
        digest: Sha256::new(),
        bytes: 0,
    };
    io::copy(&mut (&mut source).take(expected_bytes + 1), &mut writer)
        .map_err(|_| Error::InvalidMedia)?;
    if writer.bytes != expected_bytes || hex(&writer.digest.finalize()) != expected_sha256 {
        return Err(Error::InvalidMedia);
    }
    writer.inner.sync_all().map_err(|_| Error::Storage)?;
    let verify = scratch_path(root, "plain", "verify");
    let verified = decrypt_stream_to_path(
        File::open(&temporary).map_err(|_| Error::Storage)?,
        &verify,
        key,
        aad,
    );
    let decrypted_bytes = fs::metadata(&verify).map(|m| m.len());
    let _ = fs::remove_file(&verify);
    verified.map_err(|_| Error::InvalidMedia)?;
    if decrypted_bytes.ok() != Some(expected.plaintext_bytes) {
        return Err(Error::InvalidMedia);
    }
    let destination = cipher_path(root, attachment_id);
    // No-clobber promotion: a concurrent installer may already have linked a verified object.
    match fs::hard_link(&temporary, &destination) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if !matches_digest(&destination, expected_bytes, expected_sha256) {
                // Not a verified object (e.g. a damaged leftover): atomically replace it.
                fs::rename(&temporary, &destination).map_err(|_| Error::Storage)?;
            }
        }
        Err(_) => return Err(Error::Storage),
    }
    drop(cleanup);
    sync_dir(&root.join("cipher"));
    Ok(())
}
fn matches_digest(path: &Path, bytes: u64, sha256: &str) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    let mut hashing = Hashing {
        inner: io::sink(),
        digest: Sha256::new(),
        bytes: 0,
    };
    io::copy(&mut file.take(bytes + 1), &mut hashing).is_ok()
        && hashing.bytes == bytes
        && hex(&hashing.digest.finalize()) == sha256
}

/// Decrypts verified local ciphertext to a fresh native-only plaintext file.
pub(crate) fn open_plaintext(
    root: &Path,
    attachment_id: AttachmentId,
    key: &FileKey,
    aad: &[u8],
    plaintext_bytes: u64,
    owner: Arc<dyn Any + Send + Sync>,
) -> Result<NativePlaintextFile, Error> {
    let input = File::open(cipher_path(root, attachment_id)).map_err(|_| Error::InvalidMedia)?;
    let path = scratch_path(root, "plain", "bin");
    decrypt_stream_to_path(input, &path, key, aad).map_err(|_| Error::InvalidMedia)?;
    let file = NativePlaintextFile {
        path,
        _owner: owner,
    };
    if fs::metadata(file.path()).map(|m| m.len()).ok() != Some(plaintext_bytes) {
        return Err(Error::InvalidMedia); // dropping `file` removes the plaintext
    }
    Ok(file)
}
