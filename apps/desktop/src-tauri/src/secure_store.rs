//! OS secure storage through the maintained `keyring` crate: macOS Keychain
//! (Security.framework generic passwords), Windows Credential Manager, and the Linux Secret
//! Service. Secrets are passed in-process only; nothing is placed on a process argv. Other
//! platforms fail closed instead of falling back to keyring's in-memory mock store.
use crate::error::{BridgeError, BridgeResult};
use zeroize::Zeroizing;

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
