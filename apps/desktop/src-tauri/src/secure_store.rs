//! OS secure storage through the maintained `keyring` crate: macOS Keychain
//! (Security.framework generic passwords), Windows Credential Manager, and the Linux Secret
//! Service. Secrets are passed in-process only; nothing is placed on a process argv. Other
//! platforms fail closed instead of falling back to keyring's in-memory mock store.
use crate::error::{BridgeError, BridgeResult};
#[cfg(not(any(target_os = "macos", test)))]
use zeroize::Zeroizing;
#[cfg(any(target_os = "macos", test))]
use zeroize::{Zeroize, Zeroizing};

pub const SERVICE: &str = "org.openpush.desktop";

pub trait SecretStore: Send + Sync {
    fn get(&self, account: &str) -> BridgeResult<Option<Zeroizing<Vec<u8>>>>;
    fn set(&self, account: &str, secret: &[u8]) -> BridgeResult<()>;
}

fn unavailable() -> BridgeError {
    BridgeError::new(
        "secure-store-unavailable",
        "OS secure storage is unavailable or denied access; OpenPush cannot safely continue.",
    )
}

#[cfg(any(target_os = "macos", test))]
mod bundled {
    use super::{unavailable, BridgeError, BridgeResult, SecretStore, Zeroize, Zeroizing};
    use base64::{engine::general_purpose::STANDARD, Engine};
    use serde::{Deserialize, Serialize};
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::{
        collections::{BTreeMap, HashMap},
        fs::{File, OpenOptions},
        path::PathBuf,
        sync::Mutex,
    };

    const MAX_RECORD_BYTES: usize = 64 * 1024;
    static BUNDLED_WRITE: Mutex<()> = Mutex::new(());

    fn invalid_record() -> BridgeError {
        BridgeError::new(
            "secure-store-unavailable",
            "A protected credential in secure storage is invalid; it was not replaced.",
        )
    }

    fn database_key_conflict() -> BridgeError {
        BridgeError::new(
            "database-key-conflict",
            "The protected database key already exists and was not replaced.",
        )
    }

    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct WireRecord {
        version: u8,
        entries: BTreeMap<String, String>,
    }

    #[derive(Clone, Default)]
    struct Record(BTreeMap<String, Zeroizing<Vec<u8>>>);

    enum CachedRecord {
        Record(Record),
        Poisoned,
    }

    pub(crate) struct Account {
        scope: String,
        field: String,
        record_account: String,
        is_db_key: bool,
    }

    fn parse_uuid(value: &str) -> Option<String> {
        let uuid = uuid::Uuid::parse_str(value).ok()?;
        (uuid.to_string() == value).then(|| uuid.to_string())
    }

    pub(crate) fn parse_account(account: &str) -> Option<Account> {
        let mut parts = account.split(':');
        let kind = parts.next()?;
        if !matches!(kind, "credential" | "db-key" | "key-cache") || parts.next()? != "v2" {
            return None;
        }
        let vault = parse_uuid(parts.next()?)?;
        let device = parse_uuid(parts.next()?)?;
        let field = match kind {
            "credential" | "db-key" if parts.next().is_none() => kind.to_owned(),
            "key-cache" => {
                let epoch = parts.next()?;
                if epoch.parse::<u32>().ok()?.to_string() != epoch || parts.next().is_some() {
                    return None;
                }
                format!("key-cache:{epoch}")
            }
            _ => return None,
        };
        let scope = format!("{vault}:{device}");
        Some(Account {
            record_account: format!("secrets:v3:{scope}"),
            scope,
            is_db_key: kind == "db-key",
            field,
        })
    }

    fn decode_record(bytes: &[u8]) -> BridgeResult<Record> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(invalid_record());
        }
        let wire: WireRecord = serde_json::from_slice(bytes).map_err(|_| invalid_record())?;
        if wire.version != 3 {
            return Err(invalid_record());
        }
        let mut record = Record::default();
        let mut entries = wire.entries;
        while let Some((field, mut encoded)) = entries.pop_first() {
            let decoded = STANDARD.decode(&encoded);
            encoded.zeroize();
            let decoded = match decoded {
                Ok(decoded) => decoded,
                Err(_) => {
                    entries.values_mut().for_each(Zeroize::zeroize);
                    return Err(invalid_record());
                }
            };
            record.0.insert(field, Zeroizing::new(decoded));
        }
        Ok(record)
    }

    fn encode_record(record: &Record) -> BridgeResult<Zeroizing<Vec<u8>>> {
        let mut entries = BTreeMap::new();
        for (field, value) in &record.0 {
            entries.insert(field.clone(), STANDARD.encode(value.as_slice()));
        }
        let mut wire = WireRecord {
            version: 3,
            entries,
        };
        let bytes = match serde_json::to_vec(&wire) {
            Ok(bytes) => bytes,
            Err(_) => {
                wire.entries.values_mut().for_each(Zeroize::zeroize);
                return Err(unavailable());
            }
        };
        wire.entries.values_mut().for_each(Zeroize::zeroize);
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(unavailable());
        }
        Ok(Zeroizing::new(bytes))
    }

    /// Consolidates the secrets for one vault/device into one secure-storage item.
    pub struct BundledStore<S: SecretStore> {
        inner: S,
        lock_path: PathBuf,
        cache: Mutex<HashMap<String, CachedRecord>>,
    }

    impl<S: SecretStore> BundledStore<S> {
        pub fn new(inner: S, lock_path: PathBuf) -> Self {
            Self {
                inner,
                lock_path,
                cache: Mutex::new(HashMap::new()),
            }
        }

        fn lock_file(&self) -> BridgeResult<File> {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true);
            #[cfg(unix)]
            options.mode(0o600);
            let file = options.open(&self.lock_path).map_err(|_| unavailable())?;
            #[cfg(unix)]
            file.set_permissions(std::fs::Permissions::from_mode(0o600))
                .map_err(|_| unavailable())?;
            file.lock().map_err(|_| unavailable())?;
            Ok(file)
        }

        fn read_fresh(&self, account: &Account) -> BridgeResult<CachedRecord> {
            match self.inner.get(&account.record_account)? {
                Some(bytes) => decode_record(&bytes)
                    .map(CachedRecord::Record)
                    .or(Ok(CachedRecord::Poisoned)),
                None => Ok(CachedRecord::Record(Record::default())),
            }
        }

        /// `from_legacy` means `value` was just read from the legacy item, so that item is not
        /// read again (each extra read can prompt on macOS).
        pub(crate) fn write(
            &self,
            account: &Account,
            value: &[u8],
            from_legacy: bool,
        ) -> BridgeResult<()> {
            let _serialized = BUNDLED_WRITE
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let _lock = self.lock_file()?;
            let fresh = self.read_fresh(account)?;
            let mut record = match fresh {
                CachedRecord::Record(record) => record,
                CachedRecord::Poisoned => return Err(invalid_record()),
            };
            if from_legacy && record.0.contains_key(&account.field) {
                self.cache
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .insert(account.scope.clone(), CachedRecord::Record(record));
                return Ok(());
            }
            if account.is_db_key {
                if let Some(existing) = record.0.get("db-key") {
                    if existing.as_slice() != value {
                        return Err(database_key_conflict());
                    }
                    self.cache
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .insert(account.scope.clone(), CachedRecord::Record(record));
                    return Ok(());
                }
                if !from_legacy {
                    match self.inner.get(&format!("db-key:v2:{}", account.scope))? {
                        Some(existing) if existing.as_slice() != value => {
                            return Err(database_key_conflict())
                        }
                        Some(_) => {}
                        None => self
                            .inner
                            .set(&format!("db-key:v2:{}", account.scope), value)?,
                    }
                }
            }
            record
                .0
                .insert(account.field.clone(), Zeroizing::new(value.to_vec()));
            let bytes = encode_record(&record)?;
            self.inner.set(&account.record_account, &bytes)?;
            self.cache
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .insert(account.scope.clone(), CachedRecord::Record(record));
            Ok(())
        }
    }

    impl<S: SecretStore> SecretStore for BundledStore<S> {
        fn get(&self, account: &str) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
            if account.starts_with("secrets:") {
                return Err(unavailable());
            }
            let Some(parsed) = parse_account(account) else {
                return self.inner.get(account);
            };
            let (found, poisoned) = {
                let mut cache = self
                    .cache
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if parsed.field == "credential" {
                    let fresh = self.read_fresh(&parsed)?;
                    cache.insert(parsed.scope.clone(), fresh);
                } else {
                    let needs_reload = !matches!(cache.get(&parsed.scope), Some(CachedRecord::Record(record)) if record.0.contains_key(&parsed.field));
                    if needs_reload
                        && !matches!(cache.get(&parsed.scope), Some(CachedRecord::Poisoned))
                    {
                        let fresh = self.read_fresh(&parsed)?;
                        cache.insert(parsed.scope.clone(), fresh);
                    }
                }
                match cache.get(&parsed.scope) {
                    Some(CachedRecord::Record(record)) => {
                        (record.0.get(&parsed.field).cloned(), false)
                    }
                    Some(CachedRecord::Poisoned) => (None, true),
                    None => (None, false),
                }
            };
            if found.is_some() {
                return Ok(found);
            }
            let legacy = self.inner.get(account)?;
            if let Some(value) = &legacy {
                if !poisoned {
                    let _ = self.write(&parsed, value, true);
                }
            }
            if poisoned && legacy.is_none() {
                return Err(invalid_record());
            }
            Ok(legacy)
        }

        fn set(&self, account: &str, secret: &[u8]) -> BridgeResult<()> {
            if account.starts_with("secrets:") {
                return Err(unavailable());
            }
            match parse_account(account) {
                Some(parsed) => self.write(&parsed, secret, false),
                None => self.inner.set(account, secret),
            }
        }
    }
}

#[cfg(any(target_os = "macos", test))]
pub use bundled::BundledStore;

/// Production store. Unit tests use [`MemoryStore`] instead of touching the real keychain.
pub struct KeyringStore;

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
impl SecretStore for KeyringStore {
    fn get(&self, account: &str) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
        let entry = keyring::Entry::new(SERVICE, account).map_err(|_| unavailable())?;
        match entry.get_secret() {
            Ok(secret) => Ok(Some(Zeroizing::new(secret))),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => Err(unavailable()),
        }
    }
    fn set(&self, account: &str, secret: &[u8]) -> BridgeResult<()> {
        let entry = keyring::Entry::new(SERVICE, account).map_err(|_| unavailable())?;
        entry.set_secret(secret).map_err(|_| unavailable())?;
        // Read back so a silently non-persistent store can never be mistaken for success.
        let stored = Zeroizing::new(entry.get_secret().map_err(|_| unavailable())?);
        if stored.as_slice() == secret {
            Ok(())
        } else {
            Err(unavailable())
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
impl SecretStore for KeyringStore {
    fn get(&self, _: &str) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
        Err(unavailable())
    }
    fn set(&self, _: &str, _: &[u8]) -> BridgeResult<()> {
        Err(unavailable())
    }
}

/// In-process store for unit tests only.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore(pub std::sync::Mutex<std::collections::HashMap<String, Vec<u8>>>);

#[cfg(test)]
impl SecretStore for MemoryStore {
    fn get(&self, account: &str) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(account)
            .cloned()
            .map(Zeroizing::new))
    }
    fn set(&self, account: &str, secret: &[u8]) -> BridgeResult<()> {
        self.0
            .lock()
            .unwrap()
            .insert(account.to_owned(), secret.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod bundled_tests {
    use super::*;
    use crate::secure_store::bundled::parse_account;
    use std::{
        fs::OpenOptions,
        path::PathBuf,
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc, Arc,
        },
        thread,
        time::Duration,
    };

    const VAULT: &str = "11111111-1111-1111-1111-111111111111";
    const DEVICE: &str = "22222222-2222-2222-2222-222222222222";

    #[derive(Clone, Default)]
    struct SharedStore {
        values: Arc<MemoryStore>,
        gets: Arc<AtomicUsize>,
    }

    impl SecretStore for SharedStore {
        fn get(&self, account: &str) -> BridgeResult<Option<Zeroizing<Vec<u8>>>> {
            self.gets.fetch_add(1, Ordering::SeqCst);
            self.values.get(account)
        }
        fn set(&self, account: &str, secret: &[u8]) -> BridgeResult<()> {
            self.values.set(account, secret)
        }
    }

    fn account(field: &str) -> String {
        if let Some(epoch) = field.strip_prefix("key-cache:") {
            format!("key-cache:v2:{VAULT}:{DEVICE}:{epoch}")
        } else {
            format!("{field}:v2:{VAULT}:{DEVICE}")
        }
    }
    fn bundled(inner: SharedStore, path: PathBuf) -> BundledStore<SharedStore> {
        BundledStore::new(inner, path)
    }

    #[test]
    fn fresh_flow_dual_writes_only_database_key() {
        let inner = SharedStore::default();
        let dir = tempfile::tempdir().unwrap();
        let store = bundled(inner.clone(), dir.path().join("lock"));
        store.set(&account("db-key"), b"key").unwrap();
        store.set(&account("credential"), b"credential").unwrap();
        assert!(inner.values.get(&account("db-key")).unwrap().is_some());
        assert!(inner.values.get(&account("credential")).unwrap().is_none());
        assert!(inner
            .values
            .get(&format!("secrets:v3:{VAULT}:{DEVICE}"))
            .unwrap()
            .is_some());
    }

    #[test]
    fn migration_preserves_legacy_and_restart_reads_one_record() {
        let inner = SharedStore::default();
        inner
            .values
            .set(&account("credential"), b"credential")
            .unwrap();
        inner.values.set(&account("db-key"), b"key").unwrap();
        inner.values.set(&account("key-cache:1"), b"cache").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = bundled(inner.clone(), dir.path().join("lock"));
        for field in ["credential", "db-key", "key-cache:1"] {
            assert!(store.get(&account(field)).unwrap().is_some());
        }
        assert_eq!(
            inner
                .values
                .get(&account("credential"))
                .unwrap()
                .unwrap()
                .as_slice(),
            b"credential"
        );
        inner.gets.store(0, Ordering::SeqCst);
        let restart = bundled(inner.clone(), dir.path().join("lock"));
        for field in ["credential", "db-key", "key-cache:1"] {
            assert!(restart.get(&account(field)).unwrap().is_some());
        }
        assert_eq!(inner.gets.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn migration_reads_each_legacy_item_once() {
        let inner = SharedStore::default();
        inner
            .values
            .set(&account("credential"), b"credential")
            .unwrap();
        inner.values.set(&account("db-key"), b"key").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = bundled(inner.clone(), dir.path().join("lock"));
        inner.gets.store(0, Ordering::SeqCst);
        assert!(store.get(&account("credential")).unwrap().is_some());
        assert!(store.get(&account("db-key")).unwrap().is_some());
        // Record reads plus exactly one read of each legacy item.
        let record_reads = 4;
        assert_eq!(inner.gets.load(Ordering::SeqCst), record_reads + 2);
    }

    #[test]
    fn database_key_never_replaces_or_conflicts_with_legacy() {
        let inner = SharedStore::default();
        let dir = tempfile::tempdir().unwrap();
        let store = bundled(inner.clone(), dir.path().join("lock"));
        store.set(&account("db-key"), b"one").unwrap();
        assert_eq!(
            store.set(&account("db-key"), b"two").unwrap_err().code,
            "database-key-conflict"
        );
        assert!(store.set(&account("db-key"), b"one").is_ok());
        let other = SharedStore::default();
        other.values.set(&account("db-key"), b"old").unwrap();
        let conflict = bundled(other.clone(), dir.path().join("other"));
        assert_eq!(
            conflict.set(&account("db-key"), b"new").unwrap_err().code,
            "database-key-conflict"
        );
        assert!(other
            .values
            .get(&format!("secrets:v3:{VAULT}:{DEVICE}"))
            .unwrap()
            .is_none());
        assert_eq!(
            other
                .values
                .get(&account("db-key"))
                .unwrap()
                .unwrap()
                .as_slice(),
            b"old"
        );
    }

    #[test]
    fn corrupt_record_falls_back_to_legacy_and_refuses_set() {
        let inner = SharedStore::default();
        inner
            .values
            .set(&format!("secrets:v3:{VAULT}:{DEVICE}"), b"bad")
            .unwrap();
        inner.values.set(&account("credential"), b"legacy").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = bundled(inner.clone(), dir.path().join("lock"));
        assert_eq!(
            store
                .get(&account("credential"))
                .unwrap()
                .unwrap()
                .as_slice(),
            b"legacy"
        );
        assert_eq!(
            store.set(&account("credential"), b"new").unwrap_err().code,
            "secure-store-unavailable"
        );
        assert_eq!(
            inner
                .values
                .get(&format!("secrets:v3:{VAULT}:{DEVICE}"))
                .unwrap()
                .unwrap()
                .as_slice(),
            b"bad"
        );
    }

    #[test]
    fn corrupt_record_without_legacy_returns_an_error() {
        let inner = SharedStore::default();
        inner
            .values
            .set(&format!("secrets:v3:{VAULT}:{DEVICE}"), b"bad")
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = bundled(inner, dir.path().join("lock"));
        assert_eq!(
            store.get(&account("credential")).unwrap_err().code,
            "secure-store-unavailable"
        );
    }

    #[test]
    fn migration_does_not_replace_a_newer_record_value() {
        let inner = SharedStore::default();
        inner.values.set(&account("credential"), b"old").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = bundled(inner.clone(), dir.path().join("lock"));
        store.set(&account("credential"), b"new").unwrap();
        store
            .write(
                &parse_account(&account("credential")).unwrap(),
                b"old",
                true,
            )
            .unwrap();
        assert_eq!(
            store
                .get(&account("credential"))
                .unwrap()
                .unwrap()
                .as_slice(),
            b"new"
        );
        assert_eq!(
            inner
                .values
                .get(&account("credential"))
                .unwrap()
                .unwrap()
                .as_slice(),
            b"old"
        );
    }

    #[test]
    fn oversized_record_is_rejected_without_writing() {
        let inner = SharedStore::default();
        let dir = tempfile::tempdir().unwrap();
        let store = bundled(inner.clone(), dir.path().join("lock"));
        let oversized = vec![0; 64 * 1024];
        assert_eq!(
            store
                .set(&account("credential"), &oversized)
                .unwrap_err()
                .code,
            "secure-store-unavailable"
        );
        assert!(inner
            .values
            .get(&format!("secrets:v3:{VAULT}:{DEVICE}"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn file_lock_blocks_writes_from_another_file_handle() {
        let inner = SharedStore::default();
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        lock.lock().unwrap();
        let store = Arc::new(bundled(inner.clone(), lock_path));
        let (done, received) = mpsc::channel();
        let writer = {
            let store = store.clone();
            thread::spawn(move || {
                done.send(store.set(&account("credential"), b"value"))
                    .unwrap()
            })
        };
        assert!(received.recv_timeout(Duration::from_millis(300)).is_err());
        drop(lock);
        received
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        writer.join().unwrap();
        assert_eq!(
            store
                .get(&account("credential"))
                .unwrap()
                .unwrap()
                .as_slice(),
            b"value"
        );
    }

    #[test]
    fn unusable_lock_path_refuses_writes() {
        let inner = SharedStore::default();
        let dir = tempfile::tempdir().unwrap();
        let store = bundled(inner.clone(), dir.path().to_path_buf());
        assert_eq!(
            store
                .set(&account("credential"), b"value")
                .unwrap_err()
                .code,
            "secure-store-unavailable"
        );
        assert!(inner
            .values
            .get(&format!("secrets:v3:{VAULT}:{DEVICE}"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn noncanonical_bundled_accounts_pass_through() {
        let inner = SharedStore::default();
        let dir = tempfile::tempdir().unwrap();
        let store = bundled(inner.clone(), dir.path().join("lock"));
        for account in [
            format!(
                "credential:v2:{}:{DEVICE}",
                "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_uppercase()
            ),
            format!("key-cache:v2:{VAULT}:{DEVICE}:01"),
            format!("key-cache:v2:{VAULT}:{DEVICE}:+1"),
            format!("credential:v2:{VAULT}:{DEVICE}:extra"),
        ] {
            store.set(&account, b"value").unwrap();
            assert_eq!(store.get(&account).unwrap().unwrap().as_slice(), b"value");
            assert_eq!(
                inner.values.get(&account).unwrap().unwrap().as_slice(),
                b"value"
            );
        }
    }

    #[test]
    fn credential_reloads_and_key_cache_miss_reloads() {
        let inner = SharedStore::default();
        let dir = tempfile::tempdir().unwrap();
        let first = bundled(inner.clone(), dir.path().join("lock"));
        let second = bundled(inner.clone(), dir.path().join("lock"));
        first.set(&account("credential"), b"one").unwrap();
        assert_eq!(
            second
                .get(&account("credential"))
                .unwrap()
                .unwrap()
                .as_slice(),
            b"one"
        );
        first.set(&account("credential"), b"two").unwrap();
        assert_eq!(
            second
                .get(&account("credential"))
                .unwrap()
                .unwrap()
                .as_slice(),
            b"two"
        );
        assert!(second.get(&account("key-cache:7")).unwrap().is_none());
        first.set(&account("key-cache:7"), b"cache").unwrap();
        assert_eq!(
            second
                .get(&account("key-cache:7"))
                .unwrap()
                .unwrap()
                .as_slice(),
            b"cache"
        );
    }

    #[test]
    fn unknown_and_secrets_accounts_are_handled_safely() {
        let inner = SharedStore::default();
        let dir = tempfile::tempdir().unwrap();
        let store = bundled(inner.clone(), dir.path().join("lock"));
        store.set("other", b"value").unwrap();
        assert_eq!(store.get("other").unwrap().unwrap().as_slice(), b"value");
        assert_eq!(
            store.get("secrets:v3:x").unwrap_err().code,
            "secure-store-unavailable"
        );
        assert_eq!(
            store.set("secrets:v3:x", b"x").unwrap_err().code,
            "secure-store-unavailable"
        );
    }

    #[test]
    fn epochs_coexist_and_file_lock_preserves_concurrent_updates() {
        let inner = SharedStore::default();
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");
        let a = Arc::new(bundled(inner.clone(), lock.clone()));
        let b = Arc::new(bundled(inner.clone(), lock));
        let left = {
            let a = a.clone();
            thread::spawn(move || {
                for _ in 0..20 {
                    a.set(&account("db-key"), b"key").unwrap();
                    a.set(&account("key-cache:1"), b"one").unwrap();
                }
            })
        };
        let right = {
            let b = b.clone();
            thread::spawn(move || {
                for _ in 0..20 {
                    b.set(&account("credential"), b"credential").unwrap();
                    b.set(&account("key-cache:2"), b"two").unwrap();
                }
            })
        };
        left.join().unwrap();
        right.join().unwrap();
        let verify = bundled(inner, dir.path().join("lock"));
        for field in ["db-key", "credential", "key-cache:1", "key-cache:2"] {
            assert!(verify.get(&account(field)).unwrap().is_some());
        }
    }
}
