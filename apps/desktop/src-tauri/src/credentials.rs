//! Device credential import, origin/vault/device binding, and host configuration.
//!
//! Secure-store accounts are keyed by vault AND device, so importing another device for the
//! same vault never reuses (or overwrites) the first device's database key or local database.
//! The stored credential carries its origin; one vault/device can only ever be bound to that
//! origin. A database key is generated exactly once per binding and is never replaced.
use crate::{
    error::{BridgeError, BridgeResult},
    origin::validate_origin,
    secure_store::SecretStore,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    path::{Path, PathBuf},
};
use zeroize::{Zeroize, Zeroizing};

pub const MAX_CREDENTIAL_BYTES: usize = 16 * 1024;
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const DEVICE_TOKEN_HEX: usize = 96;

/// Import format v1 (also written by the gateway simulator's `pair ... device`).
/// Deliberately implements neither `Debug` nor `Clone`; the token is zeroized on drop.
#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImportedCredential {
    pub version: u8,
    pub origin: String,
    pub vault_id: String,
    pub device_id: String,
    pub device_token: String,
}

impl Drop for ImportedCredential {
    fn drop(&mut self) {
        self.device_token.zeroize();
    }
}

impl ImportedCredential {
    pub fn binding(&self) -> Binding {
        Binding {
            origin: self.origin.clone(),
            vault_id: self.vault_id.clone(),
            device_id: self.device_id.clone(),
        }
    }
}

fn import_error(message: &'static str) -> BridgeError {
    BridgeError::new("credential-import", message)
}

/// Strict, bounded parser. Errors are fixed strings and never echo the input.
pub fn parse_credential(bytes: &[u8]) -> BridgeResult<ImportedCredential> {
    if bytes.len() > MAX_CREDENTIAL_BYTES {
        return Err(import_error(
            "The credential file exceeds the 16 KiB safety limit.",
        ));
    }
    let mut credential: ImportedCredential = serde_json::from_slice(bytes)
        .map_err(|_| import_error("The credential file is not valid Peppy credential JSON."))?;
    if credential.version != 1 {
        return Err(import_error(
            "Only Peppy v1 device credentials are supported.",
        ));
    }
    if credential.device_token.len() != DEVICE_TOKEN_HEX
        || !credential
            .device_token
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(import_error(
            "The credential does not contain a valid device token.",
        ));
    }
    let origin = validate_origin(&credential.origin)?;
    let vault = uuid::Uuid::parse_str(&credential.vault_id)
        .map_err(|_| import_error("The credential vault ID is invalid."))?;
    let device = uuid::Uuid::parse_str(&credential.device_id)
        .map_err(|_| import_error("The credential device ID is invalid."))?;
    credential.origin = origin;
    credential.vault_id = vault.to_string();
    credential.device_id = device.to_string();
    Ok(credential)
}

/// Reads at most `MAX_CREDENTIAL_BYTES + 1` bytes so oversized files are rejected without
/// loading them.
pub fn read_credential_file(path: &Path) -> BridgeResult<Zeroizing<Vec<u8>>> {
    let file = fs::File::open(path)
        .map_err(|_| import_error("Could not read the selected credential file."))?;
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(MAX_CREDENTIAL_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| import_error("Could not read the selected credential file."))?;
    Ok(bytes)
}

/// The identity a credential, database key, key cache and local database are bound to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Binding {
    pub origin: String,
    pub vault_id: String,
    pub device_id: String,
}

impl Binding {
    fn scope(&self) -> String {
        format!("{}:{}", self.vault_id, self.device_id)
    }
    pub fn credential_account(&self) -> String {
        format!("credential:v2:{}", self.scope())
    }
    pub fn db_key_account(&self) -> String {
        format!("db-key:v2:{}", self.scope())
    }
    pub fn key_cache_account(&self, epoch: u32) -> String {
        format!("key-cache:v2:{}:{epoch}", self.scope())
    }
    pub fn data_dir(&self, root: &Path) -> PathBuf {
        root.join("vaults")
            .join(&self.vault_id)
            .join(&self.device_id)
    }
    pub fn database_path(&self, root: &Path) -> PathBuf {
        self.data_dir(root).join("client.sqlcipher")
    }
    pub fn same_identity(&self, other: &Binding) -> bool {
        self.vault_id == other.vault_id && self.device_id == other.device_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownBinding {
    #[serde(flatten)]
    pub binding: Binding,
    #[serde(default)]
    pub cached_epochs: Vec<u32>,
}

/// Non-secret host configuration (`server.json`, mode 0600).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HostConfig {
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub active: Option<Binding>,
    #[serde(default)]
    pub bindings: Vec<KnownBinding>,
}

impl HostConfig {
    /// The active binding, only while it matches the configured origin.
    pub fn active_binding(&self) -> Option<&Binding> {
        self.active
            .as_ref()
            .filter(|binding| self.origin.as_deref() == Some(binding.origin.as_str()))
    }
    pub fn known(&self, binding: &Binding) -> Option<&KnownBinding> {
        self.bindings
            .iter()
            .find(|known| known.binding.same_identity(binding))
    }
    pub fn remember(&mut self, binding: &Binding) {
        match self
            .bindings
            .iter_mut()
            .find(|known| known.binding.same_identity(binding))
        {
            Some(known) => known.binding = binding.clone(),
            None => self.bindings.push(KnownBinding {
                binding: binding.clone(),
                cached_epochs: vec![],
            }),
        }
    }
    pub fn add_cached_epoch(&mut self, binding: &Binding, epoch: u32) {
        self.remember(binding);
        if let Some(known) = self
            .bindings
            .iter_mut()
            .find(|known| known.binding.same_identity(binding))
        {
            if !known.cached_epochs.contains(&epoch) {
                known.cached_epochs.push(epoch);
                known.cached_epochs.sort_unstable();
            }
        }
    }
    /// Switching servers deactivates credentials bound elsewhere (they stay in secure storage and
    /// are never sent to the new origin) and reactivates the most recent binding for `origin`.
    pub fn select_origin(&mut self, origin: &str) {
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.origin != origin)
        {
            self.active = None;
        }
        if self.active.is_none() {
            self.active = self
                .bindings
                .iter()
                .rev()
                .find(|known| known.binding.origin == origin)
                .map(|known| known.binding.clone());
        }
        self.origin = Some(origin.to_owned());
    }
}

pub fn load_config(path: &Path) -> BridgeResult<HostConfig> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(HostConfig::default())
        }
        Err(_) => {
            return Err(BridgeError::new(
                "host-config",
                "Could not read the native host configuration.",
            ))
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| {
            BridgeError::new(
                "host-config",
                "Could not read the native host configuration.",
            )
        })?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(BridgeError::new(
            "host-config",
            "The native host configuration is too large.",
        ));
    }
    serde_json::from_slice(&bytes).map_err(|_| {
        BridgeError::new(
            "host-config",
            "The native host configuration is invalid; it was not modified.",
        )
    })
}

/// Atomic replace (create-new temp file, fsync, rename) with owner-only permissions.
pub fn save_config(path: &Path, config: &HostConfig) -> BridgeResult<()> {
    let failed = || {
        BridgeError::new(
            "host-config",
            "Could not save the native host configuration.",
        )
    };
    let bytes = serde_json::to_vec_pretty(config).map_err(|_| failed())?;
    crate::fsutil::write_private_atomic(path, &bytes).map_err(|_| failed())
}

/// Creates the per-binding directory with owner-only permissions.
pub fn prepare_data_dir(root: &Path, binding: &Binding) -> BridgeResult<PathBuf> {
    let dir = binding.data_dir(root);
    fs::create_dir_all(&dir).map_err(|_| {
        BridgeError::new("host-state", "Could not create the local data directory.")
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in [
            root.join("vaults"),
            root.join("vaults").join(&binding.vault_id),
            dir.clone(),
        ] {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).map_err(|_| {
                BridgeError::new("host-state", "Could not protect the local data directory.")
            })?;
        }
    }
    Ok(dir)
}

fn invalid_stored() -> BridgeError {
    BridgeError::new(
        "secure-store-unavailable",
        "A protected credential in secure storage is invalid; it was not replaced.",
    )
}

/// Returns the existing key for this binding, or creates one ONLY when no local database exists.
/// An existing key is never replaced: replacing it would strand the SQLCipher database.
///
/// Key initialization is serialized process-wide: two concurrent first imports (or an import
/// racing a session open) can never both observe "no key" and each store a different one. The
/// database's existence is checked under the same lock.
pub fn ensure_database_key(
    store: &dyn SecretStore,
    binding: &Binding,
    database: &Path,
) -> BridgeResult<Zeroizing<Vec<u8>>> {
    static KEY_INIT: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serialized = KEY_INIT.lock().unwrap_or_else(|poison| poison.into_inner());
    let database_exists = database.exists();
    if let Some(existing) = store.get(&binding.db_key_account())? {
        if existing.len() != 32 {
            return Err(invalid_stored());
        }
        return Ok(existing);
    }
    if database_exists {
        return Err(BridgeError::new(
            "database-key-missing",
            "A local database exists for this device but its protected key is missing; Peppy will not replace or reset it.",
        ));
    }
    let mut key = Zeroizing::new(vec![0u8; 32]);
    getrandom::fill(&mut key).map_err(|_| {
        BridgeError::new(
            "secure-store-unavailable",
            "Could not generate the protected database key.",
        )
    })?;
    store.set(&binding.db_key_account(), &key)?;
    Ok(key)
}

/// The stored credential for this vault/device, if any. It must still match the binding origin.
pub fn stored_credential(
    store: &dyn SecretStore,
    binding: &Binding,
) -> BridgeResult<Option<ImportedCredential>> {
    let Some(bytes) = store.get(&binding.credential_account())? else {
        return Ok(None);
    };
    let credential = parse_credential(&bytes).map_err(|_| invalid_stored())?;
    if !credential.binding().same_identity(binding) {
        return Err(invalid_stored());
    }
    if credential.origin != binding.origin {
        return Err(BridgeError::new(
            "origin-binding",
            "This device credential is bound to a different server origin.",
        ));
    }
    Ok(Some(credential))
}

/// Refuses to rebind an already imported vault/device to a different origin.
pub fn check_origin_binding(
    store: &dyn SecretStore,
    credential: &ImportedCredential,
) -> BridgeResult<()> {
    let binding = credential.binding();
    if let Some(bytes) = store.get(&binding.credential_account())? {
        let existing = parse_credential(&bytes).map_err(|_| invalid_stored())?;
        if existing.origin != credential.origin {
            return Err(BridgeError::new(
                "origin-binding",
                "This vault device was already imported for a different server origin; credentials are never moved between origins.",
            ));
        }
    }
    Ok(())
}

pub fn store_credential(
    store: &dyn SecretStore,
    credential: &ImportedCredential,
) -> BridgeResult<()> {
    let bytes = Zeroizing::new(
        serde_json::to_vec(credential)
            .map_err(|_| import_error("The credential could not be stored."))?,
    );
    store.set(&credential.binding().credential_account(), &bytes)
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::secure_store::MemoryStore;

    pub const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    pub fn credential_json(origin: &str, vault: &str, device: &str, token: &str) -> Vec<u8> {
        serde_json::json!({"version":1,"origin":origin,"vaultId":vault,"deviceId":device,"deviceToken":token}).to_string().into_bytes()
    }

    #[test]
    fn parser_is_strict_and_never_echoes_input() {
        let vault = uuid::Uuid::new_v4().to_string();
        let device = uuid::Uuid::new_v4().to_string();
        let parsed = parse_credential(&credential_json(
            "HTTPS://Example.test/",
            &vault.to_uppercase(),
            &device,
            TOKEN,
        ))
        .unwrap();
        assert_eq!(parsed.origin, "https://example.test");
        assert_eq!(parsed.vault_id, vault);
        let secret_marker = "zz-secret-marker-zz";
        for bad in [
            format!("{{\"version\":1,\"origin\":\"https://x.test\",\"vaultId\":\"{vault}\",\"deviceId\":\"{device}\",\"deviceToken\":\"{secret_marker}\"}}"),
            format!("{{\"version\":1,\"origin\":\"https://x.test\",\"vaultId\":\"{vault}\",\"deviceId\":\"{device}\",\"deviceToken\":\"{TOKEN}\",\"extra\":\"{secret_marker}\"}}"),
            format!("{{\"version\":2,\"origin\":\"https://x.test\",\"vaultId\":\"{vault}\",\"deviceId\":\"{device}\",\"deviceToken\":\"{TOKEN}\"}}"),
            format!("{{\"version\":1,\"origin\":\"http://{secret_marker}.test\",\"vaultId\":\"{vault}\",\"deviceId\":\"{device}\",\"deviceToken\":\"{TOKEN}\"}}"),
            format!("{{\"version\":1,\"origin\":\"https://x.test\",\"vaultId\":\"{secret_marker}\",\"deviceId\":\"{device}\",\"deviceToken\":\"{TOKEN}\"}}"),
            format!("not json {secret_marker}"),
        ] {
            let error = parse_credential(bad.as_bytes()).err().expect("malformed credential must be rejected");
            assert!(!error.message.contains(secret_marker), "error echoed input: {}", error.message);
            assert!(!error.message.contains(TOKEN));
        }
        assert!(parse_credential(&vec![b' '; MAX_CREDENTIAL_BYTES + 1]).is_err());
    }

    fn binding(origin: &str, vault: &str, device: &str) -> Binding {
        Binding {
            origin: origin.into(),
            vault_id: vault.into(),
            device_id: device.into(),
        }
    }

    #[test]
    fn reimport_preserves_database_key_and_never_replaces_it() {
        let store = MemoryStore::default();
        let dir = tempfile::tempdir().unwrap();
        let (absent, present) = (dir.path().join("absent.db"), dir.path().join("present.db"));
        fs::write(&present, b"db").unwrap();
        let b = binding("https://a.test", "v", "d");
        let first = ensure_database_key(&store, &b, &absent).unwrap();
        // Reimport (database now exists): same key returned, nothing overwritten.
        let second = ensure_database_key(&store, &b, &present).unwrap();
        assert_eq!(first.as_slice(), second.as_slice());
        // A token rotation for the same device keeps the key too.
        let third = ensure_database_key(&store, &b, &present).unwrap();
        assert_eq!(first.as_slice(), third.as_slice());
        // Corrupt stored key: rejected, never regenerated over the database.
        store.set(&b.db_key_account(), &[1, 2, 3]).unwrap();
        assert_eq!(
            ensure_database_key(&store, &b, &present).unwrap_err().code,
            "secure-store-unavailable"
        );
        assert_eq!(
            store.get(&b.db_key_account()).unwrap().unwrap().as_slice(),
            &[1, 2, 3]
        );
        // Missing key while a database exists: refuse instead of stranding it.
        let other = binding("https://a.test", "v", "d2");
        assert_eq!(
            ensure_database_key(&store, &other, &present)
                .unwrap_err()
                .code,
            "database-key-missing"
        );
        assert!(store.get(&other.db_key_account()).unwrap().is_none());
    }

    /// Widens the race window: every lookup of a missing key pauses before answering.
    struct SlowStore {
        inner: MemoryStore,
        key_sets: std::sync::atomic::AtomicUsize,
    }
    impl SecretStore for SlowStore {
        fn get(&self, account: &str) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
            let value = self.inner.get(account)?;
            if value.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            Ok(value)
        }
        fn set(&self, account: &str, secret: &[u8]) -> BridgeResult<()> {
            if account.starts_with("db-key:") {
                self.key_sets
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            self.inner.set(account, secret)
        }
    }

    #[test]
    fn concurrent_first_imports_create_exactly_one_database_key() {
        let store = SlowStore {
            inner: MemoryStore::default(),
            key_sets: Default::default(),
        };
        let dir = tempfile::tempdir().unwrap();
        let b = binding("https://a.test", "v", "d");
        let database = b.database_path(dir.path());
        let barrier = std::sync::Barrier::new(4);
        let keys: Vec<Vec<u8>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        ensure_database_key(&store, &b, &database).unwrap().to_vec()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(
            store.key_sets.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "one key generated"
        );
        assert!(
            keys.iter().all(|key| key == &keys[0]),
            "every caller got the stored key"
        );
        assert_eq!(
            store
                .inner
                .get(&b.db_key_account())
                .unwrap()
                .unwrap()
                .to_vec(),
            keys[0]
        );
    }

    #[test]
    fn identity_binding_separates_devices_and_origins() {
        let a = binding("https://a.test", "v", "d1");
        let b = binding("https://a.test", "v", "d2");
        assert_ne!(a.db_key_account(), b.db_key_account());
        assert_ne!(a.credential_account(), b.credential_account());
        assert_ne!(
            a.database_path(Path::new("/r")),
            b.database_path(Path::new("/r"))
        );
        assert_ne!(a.key_cache_account(1), b.key_cache_account(1));

        let store = MemoryStore::default();
        let vault = uuid::Uuid::new_v4().to_string();
        let device = uuid::Uuid::new_v4().to_string();
        let original =
            parse_credential(&credential_json("https://a.test", &vault, &device, TOKEN)).unwrap();
        store_credential(&store, &original).unwrap();
        let moved = parse_credential(&credential_json(
            "https://evil.test",
            &vault,
            &device,
            TOKEN,
        ))
        .unwrap();
        assert_eq!(
            check_origin_binding(&store, &moved).unwrap_err().code,
            "origin-binding"
        );
        assert!(check_origin_binding(&store, &original).is_ok());
        let loaded = stored_credential(&store, &original.binding())
            .unwrap()
            .unwrap();
        assert_eq!(loaded.device_token, TOKEN);
        let mut wrong_origin = original.binding();
        wrong_origin.origin = "https://evil.test".into();
        assert_eq!(
            stored_credential(&store, &wrong_origin).err().unwrap().code,
            "origin-binding"
        );
    }

    #[test]
    fn origin_switch_deactivates_and_restores_bindings() {
        let mut config = HostConfig::default();
        let a = binding("https://a.test", "v", "d1");
        config.select_origin("https://a.test");
        config.remember(&a);
        config.active = Some(a.clone());
        assert_eq!(config.active_binding(), Some(&a));
        config.select_origin("https://b.test");
        assert_eq!(config.active_binding(), None);
        config.select_origin("https://a.test");
        assert_eq!(config.active_binding(), Some(&a));
        // Legacy `{ "origin": ... }` files still parse.
        let legacy: HostConfig = serde_json::from_str("{\"origin\":\"https://a.test\"}").unwrap();
        assert_eq!(legacy.origin.as_deref(), Some("https://a.test"));
    }

    #[test]
    fn config_is_saved_atomically_with_owner_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.json");
        let mut config = HostConfig::default();
        config.select_origin("https://a.test");
        save_config(&path, &config).unwrap();
        assert_eq!(load_config(&path).unwrap(), config);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
