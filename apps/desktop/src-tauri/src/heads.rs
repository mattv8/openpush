//! Private, binding-scoped persistence for explicitly pinned conversation heads.
//!
//! This file deliberately stores only opaque conversation IDs and monitor-relative
//! geometry. Display names, previews and message text remain in the encrypted
//! session and are resolved only while that session is unlocked.
use crate::{credentials::Binding, windows::HeadPosition};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub const MAX_HEADS: usize = 8;
const MAX_PIN_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Pin {
    pub conversation_id: String,
    pub position: HeadPosition,
}

#[derive(Default, Serialize, Deserialize)]
struct PinFile {
    #[serde(default)]
    bindings: BTreeMap<String, Vec<Pin>>,
}

pub struct PinStore {
    path: PathBuf,
    transaction: Mutex<()>,
}

/// Generation fence for asynchronous backend callbacks.
#[derive(Default)]
pub struct GenerationFence {
    generation: u64,
}

impl GenerationFence {
    pub fn current(&self) -> u64 {
        self.generation
    }
    pub fn invalidate(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }
    pub fn accepts(&self, generation: u64) -> bool {
        generation == self.generation
    }
}

impl PinStore {
    pub fn new(root: &Path) -> Self {
        Self {
            path: root.join("head-pins.json"),
            transaction: Mutex::new(()),
        }
    }

    fn binding_key(binding: &Binding) -> String {
        format!(
            "{}\u{1f}{}\u{1f}{}",
            binding.origin, binding.vault_id, binding.device_id
        )
    }

    fn read(&self) -> Result<PinFile, String> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(PinFile::default())
            }
            Err(_) => return Err("Could not read saved floating conversations.".into()),
        };
        if metadata.len() > MAX_PIN_FILE_BYTES {
            return Err("Could not read saved floating conversations.".into());
        }
        let bytes = fs::read(&self.path)
            .map_err(|_| "Could not read saved floating conversations.".to_string())?;
        parse_pin_file(&bytes)
    }

    fn write(&self, value: &PinFile) -> Result<(), String> {
        let bytes = serde_json::to_vec(value)
            .map_err(|_| "Could not save floating conversations.".to_string())?;
        crate::fsutil::write_private_atomic(&self.path, &bytes)
            .map_err(|_| "Could not save floating conversations.".to_string())
    }

    pub fn pins(&self, binding: &Binding) -> Result<Vec<Pin>, String> {
        let _transaction = self
            .transaction
            .lock()
            .map_err(|_| "Could not read saved floating conversations.")?;
        let mut file = self.read()?;
        Ok(sanitize(
            file.bindings
                .remove(&Self::binding_key(binding))
                .unwrap_or_default(),
        ))
    }

    pub fn upsert(&self, binding: &Binding, pin: Pin) -> Result<bool, String> {
        if !valid_pin(&pin) {
            return Err("The floating conversation position is invalid.".into());
        }
        let _transaction = self
            .transaction
            .lock()
            .map_err(|_| "Could not save floating conversations.")?;
        let mut file = self.read()?;
        let key = Self::binding_key(binding);
        let pins = file.bindings.entry(key).or_default();
        *pins = sanitize(std::mem::take(pins));
        if let Some(existing) = pins
            .iter_mut()
            .find(|item| item.conversation_id == pin.conversation_id)
        {
            *existing = pin;
            self.write(&file)?;
            return Ok(false);
        }
        if pins.len() >= MAX_HEADS {
            return Err("You can pin at most eight floating conversations.".into());
        }
        pins.push(pin);
        self.write(&file)?;
        Ok(true)
    }

    /// Updates geometry only when the pin still exists. A delayed native move
    /// must never recreate a pin that was dismissed while the callback waited.
    pub fn update_position(
        &self,
        binding: &Binding,
        conversation_id: &str,
        position: HeadPosition,
    ) -> Result<bool, String> {
        if uuid::Uuid::parse_str(conversation_id).is_err() || !valid_position(&position) {
            return Err("The floating conversation position is invalid.".into());
        }
        let _transaction = self
            .transaction
            .lock()
            .map_err(|_| "Could not save floating conversations.")?;
        let mut file = self.read()?;
        let key = Self::binding_key(binding);
        let Some(pins) = file.bindings.get_mut(&key) else {
            return Ok(false);
        };
        *pins = sanitize(std::mem::take(pins));
        let Some(existing) = pins
            .iter_mut()
            .find(|pin| pin.conversation_id == conversation_id)
        else {
            return Ok(false);
        };
        existing.position = position;
        self.write(&file)?;
        Ok(true)
    }

    pub fn remove(&self, binding: &Binding, conversation_id: &str) -> Result<Option<Pin>, String> {
        let _transaction = self
            .transaction
            .lock()
            .map_err(|_| "Could not save floating conversations.")?;
        let mut file = self.read()?;
        let mut removed = None;
        if let Some(pins) = file.bindings.get_mut(&Self::binding_key(binding)) {
            *pins = sanitize(std::mem::take(pins));
            if let Some(index) = pins
                .iter()
                .position(|pin| pin.conversation_id == conversation_id)
            {
                removed = Some(pins.remove(index));
            }
        }
        self.write(&file)?;
        Ok(removed)
    }
}

fn parse_pin_file(bytes: &[u8]) -> Result<PinFile, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| "Could not read saved floating conversations.".to_string())?;
    let bindings = value
        .get("bindings")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "Could not read saved floating conversations.".to_string())?;
    let mut parsed = BTreeMap::new();
    for (binding, records) in bindings {
        let Some(records) = records.as_array() else {
            continue;
        };
        let pins = records
            .iter()
            .filter_map(|record| serde_json::from_value::<Pin>(record.clone()).ok())
            .collect::<Vec<_>>();
        parsed.insert(binding.clone(), sanitize(pins));
    }
    Ok(PinFile { bindings: parsed })
}

fn sanitize(pins: Vec<Pin>) -> Vec<Pin> {
    let mut seen = HashSet::new();
    pins.into_iter()
        .filter(|pin| valid_pin(pin) && seen.insert(pin.conversation_id.clone()))
        .take(MAX_HEADS)
        .collect()
}

pub fn valid_position(position: &HeadPosition) -> bool {
    position.x.is_finite()
        && position.y.is_finite()
        && position.x >= 0.0
        && position.y >= 0.0
        && position.x <= 100_000.0
        && position.y <= 100_000.0
        && position
            .monitor
            .as_ref()
            .is_none_or(|name| !name.is_empty() && name.len() <= 512)
}

fn valid_pin(pin: &Pin) -> bool {
    uuid::Uuid::parse_str(&pin.conversation_id).is_ok() && valid_position(&pin.position)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn binding(device: &str) -> Binding {
        Binding {
            origin: "https://example.test".into(),
            vault_id: "vault".into(),
            device_id: device.into(),
        }
    }
    fn pin(id: &str) -> Pin {
        Pin {
            conversation_id: id.into(),
            position: HeadPosition {
                monitor: None,
                x: 4.0,
                y: 8.0,
            },
        }
    }

    #[test]
    fn pins_are_deduped_and_isolated_per_binding() {
        let root = tempdir().unwrap();
        let store = PinStore::new(root.path());
        let one = uuid::Uuid::new_v4().to_string();
        let two = uuid::Uuid::new_v4().to_string();
        assert!(store.upsert(&binding("one"), pin(&one)).unwrap());
        assert!(!store.upsert(&binding("one"), pin(&one)).unwrap());
        store.upsert(&binding("two"), pin(&two)).unwrap();
        assert_eq!(store.pins(&binding("one")).unwrap().len(), 1);
        assert_eq!(store.pins(&binding("two")).unwrap()[0].conversation_id, two);
    }

    #[test]
    fn generation_rejects_stale_account_callbacks() {
        let mut fence = GenerationFence::default();
        let prior = fence.current();
        fence.invalidate();
        assert!(!fence.accepts(prior));
        assert!(fence.accepts(fence.current()));
    }

    #[test]
    fn rejects_untrusted_geometry_and_caps_sanitized_pins() {
        let root = tempdir().unwrap();
        let store = PinStore::new(root.path());
        let invalid = Pin {
            conversation_id: uuid::Uuid::new_v4().to_string(),
            position: HeadPosition {
                monitor: Some("x".repeat(513)),
                x: f64::NAN,
                y: 0.0,
            },
        };
        assert!(store.upsert(&binding("one"), invalid).is_err());
        for _ in 0..MAX_HEADS {
            assert!(store
                .upsert(&binding("one"), pin(&uuid::Uuid::new_v4().to_string()))
                .unwrap());
        }
        assert!(store
            .upsert(&binding("one"), pin(&uuid::Uuid::new_v4().to_string()))
            .is_err());
    }

    #[test]
    fn malformed_records_do_not_hide_valid_profiles() {
        let root = tempdir().unwrap();
        let store = PinStore::new(root.path());
        let valid = uuid::Uuid::new_v4().to_string();
        let key = PinStore::binding_key(&binding("good"));
        let bad_key = PinStore::binding_key(&binding("bad"));
        fs::write(
            &store.path,
            serde_json::to_vec(&serde_json::json!({"bindings": {
                (key): [{"conversationId": valid, "position": {"monitor": null, "x": 1.0, "y": 2.0}}],
                (bad_key): [{"conversationId": 7, "position": null}]
            }}))
            .unwrap(),
        )
        .unwrap();
        assert_eq!(store.pins(&binding("good")).unwrap().len(), 1);
        assert!(store.pins(&binding("bad")).unwrap().is_empty());
    }

    #[test]
    fn oversized_file_is_rejected_before_deserialization() {
        let root = tempdir().unwrap();
        let store = PinStore::new(root.path());
        fs::write(&store.path, vec![b' '; MAX_PIN_FILE_BYTES as usize + 1]).unwrap();
        assert!(store.pins(&binding("one")).is_err());
    }

    #[test]
    fn settled_move_does_not_resurrect_a_removed_pin() {
        let root = tempdir().unwrap();
        let store = PinStore::new(root.path());
        let id = uuid::Uuid::new_v4().to_string();
        store.upsert(&binding("one"), pin(&id)).unwrap();
        assert!(store.remove(&binding("one"), &id).unwrap().is_some());
        assert!(!store
            .update_position(
                &binding("one"),
                &id,
                HeadPosition {
                    monitor: None,
                    x: 20.0,
                    y: 30.0,
                },
            )
            .unwrap());
        assert!(store.pins(&binding("one")).unwrap().is_empty());
    }

    #[test]
    fn invalid_persisted_records_do_not_consume_the_limit() {
        let root = tempdir().unwrap();
        let store = PinStore::new(root.path());
        let key = PinStore::binding_key(&binding("one"));
        let invalid = (0..MAX_HEADS + 2)
            .map(|index| {
                serde_json::json!({
                    "conversationId": format!("invalid-{index}"),
                    "position": {"monitor": null, "x": 1.0, "y": 2.0}
                })
            })
            .collect::<Vec<_>>();
        fs::write(
            &store.path,
            serde_json::to_vec(&serde_json::json!({"bindings": {(key): invalid}})).unwrap(),
        )
        .unwrap();
        assert!(store
            .upsert(&binding("one"), pin(&uuid::Uuid::new_v4().to_string()),)
            .unwrap());
    }

    #[test]
    fn atomic_write_failure_is_reported() {
        let root = tempdir().unwrap();
        let missing = root.path().join("missing");
        let store = PinStore::new(&missing);
        assert!(store
            .upsert(&binding("one"), pin(&uuid::Uuid::new_v4().to_string()),)
            .is_err());
        assert!(!store.path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_pin_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir().unwrap();
        let store = PinStore::new(root.path());
        store
            .upsert(&binding("one"), pin(&uuid::Uuid::new_v4().to_string()))
            .unwrap();
        assert_eq!(
            fs::metadata(&store.path).unwrap().permissions().mode() & 0o077,
            0
        );
    }
}
