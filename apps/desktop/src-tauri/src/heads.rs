//! Private, binding-scoped persistence for explicitly pinned conversation heads.
//!
//! This file deliberately stores only opaque conversation IDs and monitor-relative
//! geometry. Display names, previews and message text remain in the encrypted
//! local database and are resolved only while that account's session is open.
use crate::{credentials::Binding, windows::HeadPosition};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
};

pub const MAX_HEADS: usize = 8;
pub const MAX_LAYOUTS: usize = 64;
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

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PanelLayout {
    pub monitor: Option<String>,
    pub width: f64,
    pub height: f64,
    pub offset_x: f64,
    pub offset_y: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeadLayout {
    pub conversation_id: String,
    pub head: Option<HeadPosition>,
    pub panel: Option<PanelLayout>,
    pub updated: u64,
}

#[derive(Default, Serialize, Deserialize)]
struct LayoutFile {
    #[serde(default)]
    bindings: BTreeMap<String, Vec<HeadLayout>>,
}

/// Geometry intentionally outlives a pin, but remains private and binding scoped.
pub struct HeadLayoutStore {
    path: PathBuf,
    transaction: Mutex<()>,
}

pub fn binding_key(binding: &Binding) -> String {
    format!(
        "{}\u{1f}{}\u{1f}{}",
        binding.origin, binding.vault_id, binding.device_id
    )
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
                .remove(&binding_key(binding))
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
        let key = binding_key(binding);
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
        let key = binding_key(binding);
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
        if let Some(pins) = file.bindings.get_mut(&binding_key(binding)) {
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

impl HeadLayoutStore {
    pub fn new(root: &Path) -> Self {
        Self {
            path: root.join("head-layouts.json"),
            transaction: Mutex::new(()),
        }
    }

    fn read(&self) -> Result<LayoutFile, String> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LayoutFile::default())
            }
            Err(_) => return Err("Could not read saved floating conversation layouts.".into()),
        };
        if metadata.len() > MAX_PIN_FILE_BYTES {
            return Err("Could not read saved floating conversation layouts.".into());
        }
        parse_layout_file(
            &fs::read(&self.path)
                .map_err(|_| "Could not read saved floating conversation layouts.".to_string())?,
        )
    }

    /// Read for write operations: treats a corrupt file as empty (geometry only).
    /// This prevents corruption from blocking future writes.
    fn read_for_write(&self) -> Result<LayoutFile, String> {
        let metadata = match fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(LayoutFile::default())
            }
            Err(_) => return Err("Could not read saved floating conversation layouts.".into()),
        };
        if metadata.len() > MAX_PIN_FILE_BYTES {
            return Err("Could not read saved floating conversation layouts.".into());
        }
        let bytes = fs::read(&self.path)
            .map_err(|_| "Could not read saved floating conversation layouts.".to_string())?;
        match parse_layout_file(&bytes) {
            Ok(file) => Ok(file),
            Err(_) => {
                // On write path, treat corrupt file as empty so writes can proceed
                Ok(LayoutFile::default())
            }
        }
    }

    fn write(&self, value: &LayoutFile) -> Result<(), String> {
        let bytes = serde_json::to_vec(value)
            .map_err(|_| "Could not save floating conversation layouts.".to_string())?;
        crate::fsutil::write_private_atomic(&self.path, &bytes)
            .map_err(|_| "Could not save floating conversation layouts.".to_string())
    }

    pub fn layout(
        &self,
        binding: &Binding,
        conversation_id: &str,
    ) -> Result<Option<HeadLayout>, String> {
        let _guard = self
            .transaction
            .lock()
            .map_err(|_| "Could not read saved floating conversation layouts.")?;
        Ok(self
            .read()?
            .bindings
            .remove(&binding_key(binding))
            .unwrap_or_default()
            .into_iter()
            .find(|layout| layout.conversation_id == conversation_id))
    }

    /// Read-modify-write a layout under a single transaction lock.
    /// The closure receives the prior layout (if it exists) and returns the updated layout
    /// (or None to delete). Validation, cap/eviction, and updated timestamp bump are handled
    /// automatically. Multiple interleaved calls to this method preserve each other's fields.
    pub fn update<F>(
        &self,
        binding: &Binding,
        conversation_id: &str,
        f: F,
    ) -> Result<Option<HeadLayout>, String>
    where
        F: FnOnce(Option<HeadLayout>) -> Option<HeadLayout>,
    {
        if uuid::Uuid::parse_str(conversation_id).is_err() {
            return Err("The floating conversation layout is invalid.".into());
        }
        let _guard = self
            .transaction
            .lock()
            .map_err(|_| "Could not save floating conversation layouts.")?;
        let mut file = self.read_for_write()?;
        let layouts = file.bindings.entry(binding_key(binding)).or_default();
        *layouts = sanitize_layouts(std::mem::take(layouts));
        let prior = layouts
            .iter()
            .find(|layout| layout.conversation_id == conversation_id)
            .cloned();
        let updated = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
            .max(
                layouts
                    .iter()
                    .map(|layout| layout.updated)
                    .max()
                    .unwrap_or(0)
                    .saturating_add(1),
            );
        let new_layout = f(prior).map(|mut layout| {
            layout.updated = updated;
            layout
        });
        if let Some(new) = &new_layout {
            if new.conversation_id != conversation_id || !valid_layout(new) {
                return Err("The floating conversation layout is invalid.".into());
            }
        }
        if let Some(new) = new_layout.clone() {
            if let Some(old) = layouts
                .iter_mut()
                .find(|old| old.conversation_id == conversation_id)
            {
                *old = new.clone();
            } else {
                layouts.push(new.clone());
            }
        } else {
            layouts.retain(|layout| layout.conversation_id != conversation_id);
        }
        layouts.sort_by_key(|layout| std::cmp::Reverse(layout.updated));
        layouts.truncate(MAX_LAYOUTS);
        self.write(&file)?;
        Ok(new_layout)
    }

    pub fn save_head(
        &self,
        binding: &Binding,
        id: &str,
        position: HeadPosition,
    ) -> Result<HeadLayout, String> {
        self.update(binding, id, |prior| {
            let mut layout = prior.unwrap_or_else(|| HeadLayout {
                conversation_id: id.to_owned(),
                head: None,
                panel: None,
                updated: 0,
            });
            layout.head = Some(position);
            Some(layout)
        })
        .map(|layout| layout.expect("head update always returns a layout"))
    }

    pub fn save_panel(
        &self,
        binding: &Binding,
        id: &str,
        position: HeadPosition,
        panel: PanelLayout,
    ) -> Result<HeadLayout, String> {
        self.update(binding, id, |prior| {
            let mut layout = prior.unwrap_or_else(|| HeadLayout {
                conversation_id: id.to_owned(),
                head: Some(position),
                panel: None,
                updated: 0,
            });
            layout.panel = Some(panel);
            Some(layout)
        })
        .map(|layout| layout.expect("panel update always returns a layout"))
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

fn parse_layout_file(bytes: &[u8]) -> Result<LayoutFile, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| "Could not read saved floating conversation layouts.".to_string())?;
    let bindings = value
        .get("bindings")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "Could not read saved floating conversation layouts.".to_string())?;
    Ok(LayoutFile {
        bindings: bindings
            .iter()
            .filter_map(|(key, records)| {
                records.as_array().map(|records| {
                    (
                        key.clone(),
                        sanitize_layouts(
                            records
                                .iter()
                                .filter_map(|record| serde_json::from_value(record.clone()).ok())
                                .collect(),
                        ),
                    )
                })
            })
            .collect(),
    })
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

fn valid_layout(layout: &HeadLayout) -> bool {
    uuid::Uuid::parse_str(&layout.conversation_id).is_ok()
        && layout.head.as_ref().is_none_or(valid_position)
        && layout.panel.as_ref().is_none_or(|panel| {
            panel.width.is_finite()
                && panel.height.is_finite()
                && panel.width >= 320.0
                && panel.height >= 360.0
                && panel.width <= 10_000.0
                && panel.height <= 10_000.0
                && panel.offset_x.is_finite()
                && panel.offset_y.is_finite()
                && (-10_000.0..=10_000.0).contains(&panel.offset_x)
                && (-10_000.0..=10_000.0).contains(&panel.offset_y)
                && panel
                    .monitor
                    .as_ref()
                    .is_none_or(|name| !name.is_empty() && name.len() <= 512)
        })
}

fn sanitize_layouts(layouts: Vec<HeadLayout>) -> Vec<HeadLayout> {
    let mut seen = HashSet::new();
    let mut layouts = layouts
        .into_iter()
        .filter(|layout| valid_layout(layout) && seen.insert(layout.conversation_id.clone()))
        .collect::<Vec<_>>();
    layouts.sort_by_key(|layout| std::cmp::Reverse(layout.updated));
    layouts.truncate(MAX_LAYOUTS);
    layouts
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn first_panel_and_head_writes_survive_store_reopen() {
        let root = tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let id2 = uuid::Uuid::new_v4().to_string();
        let initial = pin(&id).position;
        let panel = layout(&id, 0).panel.unwrap();
        let store = HeadLayoutStore::new(root.path());
        store
            .save_panel(&binding("one"), &id, initial.clone(), panel)
            .unwrap();
        let mut moved = initial.clone();
        moved.x = 125.0;
        store
            .save_head(&binding("one"), &id, moved.clone())
            .unwrap();
        store.save_head(&binding("one"), &id2, moved).unwrap();
        drop(store);
        let reopened = HeadLayoutStore::new(root.path());
        let saved = reopened.layout(&binding("one"), &id).unwrap().unwrap();
        assert_eq!(saved.head.unwrap().x, 125.0);
        assert_eq!(saved.panel.unwrap().offset_x, -8.0);
        assert_eq!(
            reopened
                .layout(&binding("one"), &id2)
                .unwrap()
                .unwrap()
                .head
                .unwrap()
                .x,
            125.0
        );
    }

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
        let key = binding_key(&binding("good"));
        let bad_key = binding_key(&binding("bad"));
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
        let key = binding_key(&binding("one"));
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

    fn layout(id: &str, updated: u64) -> HeadLayout {
        HeadLayout {
            conversation_id: id.into(),
            head: None,
            updated,
            panel: Some(PanelLayout {
                monitor: None,
                width: 340.0,
                height: 440.0,
                offset_x: -8.0,
                offset_y: 64.0,
            }),
        }
    }

    #[test]
    fn layouts_validate_isolate_and_survive_unpin() {
        let root = tempdir().unwrap();
        let layouts = HeadLayoutStore::new(root.path());
        let id = uuid::Uuid::new_v4().to_string();
        layouts
            .update(&binding("one"), &id, |_| Some(layout(&id, 1)))
            .unwrap();
        assert!(layouts.layout(&binding("two"), &id).unwrap().is_none());
        PinStore::new(root.path())
            .remove(&binding("one"), &id)
            .unwrap();
        assert!(layouts.layout(&binding("one"), &id).unwrap().is_some());
        let mut invalid = layout(&uuid::Uuid::new_v4().to_string(), 2);
        invalid.panel.as_mut().unwrap().offset_x = f64::NAN;
        let invalid_id = invalid.conversation_id.clone();
        assert!(layouts
            .update(&binding("one"), &invalid_id, |_| Some(invalid))
            .is_err());
    }

    #[test]
    fn layouts_evict_oldest_and_drop_malformed_records() {
        let root = tempdir().unwrap();
        let store = HeadLayoutStore::new(root.path());
        let ids: Vec<_> = (0..=MAX_LAYOUTS)
            .map(|_| uuid::Uuid::new_v4().to_string())
            .collect();
        for id in &ids {
            store
                .update(&binding("one"), id, |_| Some(layout(id, 0)))
                .unwrap();
        }
        let file = store.read().unwrap();
        assert_eq!(
            file.bindings[&binding_key(&binding("one"))].len(),
            MAX_LAYOUTS
        );
        assert_eq!(
            file.bindings[&binding_key(&binding("one"))]
                .last()
                .unwrap()
                .conversation_id,
            ids[1]
        );
        assert!(store.layout(&binding("one"), &ids[0]).unwrap().is_none());
        let valid = uuid::Uuid::new_v4().to_string();
        fs::write(&store.path, serde_json::to_vec(&serde_json::json!({"bindings": {(binding_key(&binding("one"))): [
            {"conversationId": valid, "head": null, "panel": null, "updated": 1}, {"conversationId": 3}
        ]}})).unwrap()).unwrap();
        assert!(store.layout(&binding("one"), &valid).unwrap().is_some());
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

    #[test]
    fn update_preserves_other_field_across_interleaved_head_and_panel_updates() {
        let root = tempdir().unwrap();
        let store = HeadLayoutStore::new(root.path());
        let id = uuid::Uuid::new_v4().to_string();

        // Start with a layout with panel only (no head)
        store
            .update(&binding("one"), &id, |_| {
                Some(HeadLayout {
                    conversation_id: id.clone(),
                    head: None,
                    panel: Some(PanelLayout {
                        monitor: Some("primary".into()),
                        width: 340.0,
                        height: 440.0,
                        offset_x: 10.0,
                        offset_y: 20.0,
                    }),
                    updated: 100,
                })
            })
            .unwrap();

        // Update only the head field using the update method
        let result = store
            .update(&binding("one"), &id, |prior| {
                prior.map(|mut layout| {
                    layout.head = Some(HeadPosition {
                        monitor: None,
                        x: 100.0,
                        y: 200.0,
                    });
                    layout
                })
            })
            .unwrap();

        // Verify head was added
        assert!(result.as_ref().unwrap().head.is_some());
        // Verify panel was preserved
        assert!(result.as_ref().unwrap().panel.is_some());
        assert_eq!(
            result.unwrap().panel.unwrap().offset_x,
            10.0,
            "panel offset_x should be preserved"
        );

        // Now update panel only, simulating interleaved debounce
        let result = store
            .update(&binding("one"), &id, |prior| {
                prior.map(|mut layout| {
                    layout.panel = Some(PanelLayout {
                        monitor: Some("secondary".into()),
                        width: 400.0,
                        height: 500.0,
                        offset_x: 30.0,
                        offset_y: 40.0,
                    });
                    layout
                })
            })
            .unwrap();

        // Verify panel was updated
        assert_eq!(
            result.as_ref().unwrap().panel.as_ref().unwrap().width,
            400.0
        );
        // Verify head was preserved
        assert!(result.as_ref().unwrap().head.is_some());
        assert_eq!(
            result.unwrap().head.unwrap().x,
            100.0,
            "head x should be preserved"
        );
    }

    #[test]
    fn corrupt_file_then_update_succeeds_and_file_becomes_valid() {
        let root = tempdir().unwrap();
        let store = HeadLayoutStore::new(root.path());
        let id = uuid::Uuid::new_v4().to_string();

        // Write corrupt JSON to the file
        fs::write(&store.path, b"{broken json").unwrap();

        // Reading should still fail
        assert!(store.layout(&binding("one"), &id).is_err());

        // But updating should succeed by treating the file as empty
        let result = store
            .update(&binding("one"), &id, |_prior| {
                Some(HeadLayout {
                    conversation_id: id.clone(),
                    head: Some(HeadPosition {
                        monitor: None,
                        x: 50.0,
                        y: 60.0,
                    }),
                    panel: None,
                    updated: 0,
                })
            })
            .unwrap();

        assert!(result.is_some());
        assert_eq!(result.unwrap().conversation_id, id);

        // Now the file should be readable and valid
        let retrieved = store.layout(&binding("one"), &id).unwrap();
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().head.unwrap().x, 50.0);
    }
}
