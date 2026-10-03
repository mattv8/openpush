//! Contact IPC: a whitelist adapter between the webview's camelCase contact DTOs and the
//! client-core contact JSON API.
//!
//! * Views copy only named, displayable fields. Provider identity, photo descriptors, file keys,
//!   attachment IDs and filesystem paths never reach the webview; photos arrive as re-encoded
//!   data URLs.
//! * Edits become core `ContactEditRequest`s addressed to the book's owner device. The desktop
//!   never owns a book, so an accepted edit is only `pending` (with its request ID) until the
//!   owner's result reaches the local ledger, which `list_edits` reads. Nothing here reports an
//!   apply early, and an issued request cannot be withdrawn.
//! * View field IDs are preserved so list patches address existing items, and the original
//!   values (`expectedOld`) become `expected_old` so the owner can merge a stale base safely.
use crate::{
    error::{core_error, BridgeError, BridgeResult},
    fsutil, media,
    session::Session,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use peppy_client_core::{AttachmentId, Client, Error as CoreError};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::Read,
    path::{Path, PathBuf},
    str::FromStr,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_ID: usize = 1024;
const MAX_TEXT: usize = 1024;
/// Core limits notes by UTF-8 bytes on capture and by characters on patches; bytes is stricter.
const MAX_NOTES_BYTES: usize = 8192;
const MAX_VALUES: usize = 20;
const MAX_PATCHES: usize = 100;
/// Core's page cap for `contact_book_view`.
const PAGE_LIMIT: u32 = 200;
/// Core's normalized contact photo limit.
const MAX_AVATAR_BYTES: u64 = 64 * 1024;
const AVATAR_EDGE: u32 = 128;
/// Display derivative cap; a 128px JPEG is typically 4–10 KiB.
const AVATAR_MAX_BYTES: usize = 16 * 1024;
/// Avatars decrypted per list call; the rest render initials until a later (cached) call.
const AVATAR_BUDGET: usize = 64;
/// Core's `prepare_contact_photo` input limit.
const MAX_PHOTO_SOURCE_BYTES: usize = 8 * 1024 * 1024;
/// Edge of the re-encoded image handed to the webview cropper.
const CROP_SOURCE_EDGE: u32 = 1024;
const PHOTO_STAGING_DIR: &str = "contact-photo-staging";
const STAGING_MAX_AGE: Duration = Duration::from_secs(10 * 60);
const MAX_PREVIEW_CACHE: usize = 256;

/// Postal address components core accepts (`contacts::ADDRESS_KEYS`): (core key, view key).
const ADDRESS_KEYS: [(&str, &str); 11] = [
    ("value", "formatted"),
    ("street", "street"),
    ("po_box", "poBox"),
    ("neighborhood", "neighborhood"),
    ("sub_locality", "subLocality"),
    ("city", "city"),
    ("sub_administrative_area", "subAdministrativeArea"),
    ("state", "state"),
    ("postal_code", "postalCode"),
    ("country", "country"),
    ("iso_country_code", "isoCountryCode"),
];

fn invalid(message: &'static str) -> BridgeError {
    BridgeError::new("invalid-contact-edit", message)
}

fn parse_core(result: Result<String, CoreError>) -> BridgeResult<Value> {
    serde_json::from_str(&result.map_err(core_error)?).map_err(|_| BridgeError::host_state())
}

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Reads an optional UI text value: absent and `null` are empty; anything else must be a string
/// within `max` (characters, or bytes for notes).
fn ui_text(value: Option<&Value>, max: usize, bytes: bool) -> BridgeResult<String> {
    match value {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(text)) => {
            let size = if bytes {
                text.len()
            } else {
                text.chars().count()
            };
            if size <= max {
                Ok(text.clone())
            } else {
                Err(invalid("A contact value is too long."))
            }
        }
        Some(_) => Err(invalid("A contact value is invalid.")),
    }
}

fn required_id(input: &Value, key: &str) -> BridgeResult<String> {
    str_field(input, key)
        .filter(|id| valid_id(id))
        .map(str::to_owned)
        .ok_or_else(|| invalid("A required contact identifier is missing."))
}

/// Core revisions are canonical non-negative decimal strings.
fn required_revision(input: &Value) -> BridgeResult<String> {
    let revision = required_id(input, "baseRevision")?;
    let canonical = revision.bytes().all(|b| b.is_ascii_digit())
        && (revision == "0" || !revision.starts_with('0'))
        && revision.len() <= 18;
    canonical
        .then_some(revision)
        .ok_or_else(|| invalid("The contact revision is invalid."))
}

// ---------------------------------------------------------------------------------------------
// Books
// ---------------------------------------------------------------------------------------------

/// What this device may ask of a book. The owner still enforces policy and capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub can_write: bool,
    pub supports_notes: bool,
    pub supports_photo: bool,
    pub supports_birthday: bool,
    /// Any device holding a retained tombstone may ask the owner to recreate it.
    pub can_restore: bool,
}

pub fn capabilities(book: &Value, own_device: &str) -> Capabilities {
    let caps = book.get("capabilities");
    let flag = |key: &str| caps.and_then(|c| c.get(key)).and_then(Value::as_bool);
    let owned = str_field(book, "owner_device_id") == Some(own_device);
    let live = matches!(str_field(book, "state"), Some("active" | "limited"));
    let remote_edits = book.pointer("/policy/remote_edits").and_then(Value::as_str);
    // A book whose every account is read-only cannot take edits even if `write` is set.
    let accounts = book.get("accounts").and_then(Value::as_array);
    let all_read_only = accounts.is_some_and(|accounts| {
        !accounts.is_empty()
            && accounts
                .iter()
                .all(|a| a.get("writable").and_then(Value::as_bool) == Some(false))
    });
    let can_write = !owned
        && live
        && flag("write") == Some(true)
        && remote_edits != Some("off")
        && !all_read_only;
    Capabilities {
        can_write,
        // iOS advertises `notes:false` (entitlement); books without the flag accept notes.
        supports_notes: can_write && flag("notes") != Some(false),
        supports_photo: can_write && flag("photo") == Some(true),
        supports_birthday: can_write && flag("birthday") != Some(false),
        can_restore: can_write,
    }
}

fn book_state(book: &Value) -> &'static str {
    match str_field(book, "state") {
        Some("active") => "active",
        Some("limited") => "limited",
        Some("retired") => "retired",
        _ => "unavailable",
    }
}

pub fn book_view(
    book: &Value,
    own_device: &str,
    device_names: &HashMap<String, String>,
    pending: &HashMap<String, usize>,
) -> Option<Value> {
    let id = str_field(book, "id").filter(|id| valid_id(id))?;
    let caps = capabilities(book, own_device);
    let device_name = str_field(book, "owner_device_id")
        .and_then(|owner| device_names.get(owner))
        .map_or("Phone", String::as_str);
    let mut view = json!({
        "id": id,
        "deviceName": device_name,
        "state": book_state(book),
        "capabilities": {
            "canWrite": caps.can_write,
            "canDelete": caps.can_write,
            "canRestore": caps.can_restore,
            "supportsNotes": caps.supports_notes,
            "supportsPhoto": caps.supports_photo,
            "supportsBirthday": caps.supports_birthday,
        },
        "contactCount": book.get("contact_count").and_then(Value::as_u64).unwrap_or(0),
        "pendingEditCount": pending.get(id).copied().unwrap_or(0),
    });
    // Synced books carry account names but not which is the owner's default; a single
    // account is unambiguous.
    if let Some([account]) = book
        .get("accounts")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
    {
        if let Some(name) = str_field(account, "name").filter(|n| !n.is_empty()) {
            view["defaultAccountLabel"] = json!(name.chars().take(MAX_TEXT).collect::<String>());
        }
    }
    Some(view)
}

fn core_books(client: &Client) -> BridgeResult<Vec<Value>> {
    let raw = parse_core(client.list_contact_books_json())?;
    match raw.get("books") {
        Some(Value::Array(books)) => Ok(books.clone()),
        _ => Err(BridgeError::host_state()),
    }
}

fn find_book(client: &Client, book_id: &str) -> BridgeResult<Value> {
    core_books(client)?
        .into_iter()
        .find(|book| str_field(book, "id") == Some(book_id))
        .ok_or_else(|| BridgeError::new("not-found", "The contact book was not found."))
}

pub fn list_books(session: &Session) -> BridgeResult<Vec<Value>> {
    let own = session.binding.device_id.as_str();
    let pending = pending_counts(&list_edits_for(&session.client, own, None)?);
    let device_names = session
        .gateways()
        .0
        .into_iter()
        .map(|gateway| (gateway.id, gateway.name))
        .collect::<HashMap<_, _>>();
    Ok(core_books(&session.client)?
        .iter()
        .filter_map(|book| book_view(book, own, &device_names, &pending))
        .collect())
}

pub fn forget_book(session: &Session, book_id: &str) -> BridgeResult<()> {
    if !valid_id(book_id) {
        return Err(invalid("The contact book is invalid."));
    }
    parse_core(
        session
            .client
            .forget_contact_book(&json!({"book_id": book_id}).to_string()),
    )?;
    session.notify();
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Contact views
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum List {
    Phones,
    Emails,
    Addresses,
}

impl List {
    fn from_field(field: &str) -> Option<Self> {
        match field {
            "phones" => Some(Self::Phones),
            "emails" => Some(Self::Emails),
            "addresses" => Some(Self::Addresses),
            _ => None,
        }
    }
    fn core_key(self) -> &'static str {
        match self {
            Self::Phones => "phones",
            Self::Emails => "emails",
            Self::Addresses => "addresses",
        }
    }
    /// View key of the single value of a phone or email.
    fn value_key(self) -> &'static str {
        match self {
            Self::Phones => "number",
            Self::Emails => "address",
            Self::Addresses => "",
        }
    }
    fn known_core_key(self, key: &str) -> bool {
        matches!(key, "id" | "label" | "read_only" | "writable")
            || match self {
                Self::Phones | Self::Emails => key == "value",
                Self::Addresses => ADDRESS_KEYS.iter().any(|(core, _)| *core == key),
            }
    }
}

/// One core list item as a view. Items carrying keys this view cannot show are read-only, because
/// a replacement built from the view would drop those keys on the owner's device.
fn item_view(list: List, item: &Value) -> Option<Value> {
    let obj = item.as_object()?;
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| valid_id(id))?;
    let read_only = obj.get("read_only").and_then(Value::as_bool) == Some(true)
        || obj.get("writable").and_then(Value::as_bool) == Some(false)
        || obj.keys().any(|key| !list.known_core_key(key));
    let label = obj.get("label").and_then(Value::as_str).unwrap_or("");
    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    // `label` is the wire value and must round-trip unchanged; `displayLabel` is for people.
    out.insert("label".into(), json!(label));
    out.insert("displayLabel".into(), json!(display_label(label)));
    match list {
        List::Phones | List::Emails => {
            out.insert(
                list.value_key().into(),
                json!(obj.get("value").and_then(Value::as_str).unwrap_or("")),
            );
        }
        List::Addresses => {
            for (core, view) in ADDRESS_KEYS {
                if let Some(text) = obj.get(core).and_then(Value::as_str) {
                    out.insert(view.into(), json!(text));
                }
            }
        }
    }
    out.insert("readOnly".into(), json!(read_only));
    Some(Value::Object(out))
}

/// Human label for a wire label. Apple's built-in labels arrive raw as `_$!<Mobile>!$_`.
pub fn display_label(label: &str) -> String {
    label
        .strip_prefix("_$!<")
        .and_then(|rest| rest.strip_suffix(">!$_"))
        .map_or_else(|| label.to_owned(), str::to_lowercase)
}

/// Core birthday `{year?, month, day}` as a view value (numbers only).
fn birthday_view(value: Option<&Value>) -> Option<Value> {
    let value = value?.as_object()?;
    let part = |key: &str| value.get(key).and_then(Value::as_i64);
    let (month, day) = (part("month")?, part("day")?);
    let mut out = json!({"month": month, "day": day});
    if let Some(year) = part("year") {
        out["year"] = json!(year);
    }
    Some(out)
}

fn list_view(contact: &Value, list: List) -> Value {
    Value::Array(
        contact
            .get(list.core_key())
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item_view(list, item))
            .collect(),
    )
}

fn copy_text(out: &mut Map<String, Value>, view_key: &str, value: Option<&Value>) {
    if let Some(text) = value.and_then(Value::as_str) {
        out.insert(view_key.into(), json!(text));
    }
}

/// Core view photo `{attachment_id, available}`; the ID is resolved natively and never returned.
fn photo_reference(contact: &Value) -> Option<(AttachmentId, bool)> {
    let photo = contact.get("photo")?;
    let id = AttachmentId::from_str(str_field(photo, "attachment_id")?).ok()?;
    Some((
        id,
        photo.get("available").and_then(Value::as_bool) == Some(true),
    ))
}

/// Whitelisted contact view. `avatar` turns an available local photo into a data URL.
pub fn contact_view(
    contact: &Value,
    book_id: &str,
    avatar: &mut dyn FnMut(AttachmentId) -> Option<String>,
) -> Option<Value> {
    let id = str_field(contact, "id").filter(|id| valid_id(id))?;
    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    out.insert("bookId".into(), json!(book_id));
    copy_text(&mut out, "revision", contact.get("revision"));
    out.insert(
        "displayName".into(),
        json!(str_field(contact, "display_name")
            .filter(|name| !name.is_empty())
            .unwrap_or("Contact")),
    );
    let name = contact.get("name");
    copy_text(&mut out, "givenName", name.and_then(|n| n.get("given")));
    copy_text(&mut out, "familyName", name.and_then(|n| n.get("family")));
    for key in ["nickname", "organization", "title", "notes"] {
        copy_text(&mut out, key, contact.get(key));
    }
    if let Some(birthday) = birthday_view(contact.get("birthday")) {
        out.insert("birthday".into(), birthday);
    }
    out.insert("phones".into(), list_view(contact, List::Phones));
    out.insert("emails".into(), list_view(contact, List::Emails));
    out.insert("addresses".into(), list_view(contact, List::Addresses));
    if let Some((photo, available)) = photo_reference(contact) {
        match available.then(|| avatar(photo)).flatten() {
            Some(url) => {
                out.insert("photoDataUrl".into(), json!(url));
            }
            None if !available => {
                out.insert("photoPending".into(), json!(true));
            }
            None => {}
        }
    }
    Some(Value::Object(out))
}

/// Decrypts a verified local contact photo into a small re-encoded JPEG data URL (at most
/// `AVATAR_MAX_BYTES`), cached per attachment in the session preview cache, which the media
/// worker invalidates after downloads. Used for contact lists and display-only name resolution.
pub(crate) fn session_avatar(
    session: &Session,
    budget: &mut usize,
    id: AttachmentId,
) -> Option<String> {
    let cached = session
        .previews
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .get(&id)
        .cloned();
    if let Some(value) = cached {
        return value;
    }
    if *budget == 0 {
        return None;
    }
    *budget -= 1;
    let value = session
        .client
        .open_native_plaintext(id)
        .ok()
        .and_then(|file| {
            let mut bytes = Vec::new();
            fs::File::open(file.path())
                .ok()?
                .take(MAX_AVATAR_BYTES + 1)
                .read_to_end(&mut bytes)
                .ok()?;
            (bytes.len() as u64 <= MAX_AVATAR_BYTES)
                .then(|| media::avatar_data_url(&bytes, AVATAR_EDGE, AVATAR_MAX_BYTES))
                .flatten()
        });
    let mut cache = session
        .previews
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    if cache.len() >= MAX_PREVIEW_CACHE {
        cache.clear();
    }
    cache.insert(id, value.clone());
    value
}

/// One page (at most 200, core's cap) of a book's live contacts, starting at `offset`.
pub fn list_contacts(
    session: &Session,
    book_id: &str,
    query: Option<&str>,
    offset: Option<u32>,
) -> BridgeResult<Vec<Value>> {
    if !valid_id(book_id) {
        return Err(invalid("The contact book is invalid."));
    }
    let query = query.unwrap_or("");
    if query.chars().count() > MAX_TEXT {
        return Err(invalid("The contact search is too long."));
    }
    let raw = parse_core(
        session.client.contact_book_view(
            &json!({
                "schema_version": 1,
                "book_id": book_id,
                "search": query,
                "limit": PAGE_LIMIT,
                "offset": offset.unwrap_or(0),
            })
            .to_string(),
        ),
    )?;
    let contacts = raw
        .get("contacts")
        .and_then(Value::as_array)
        .ok_or_else(BridgeError::host_state)?;
    let badges = contact_badges(
        &list_edits_for(&session.client, &session.binding.device_id, Some(book_id))?,
        now_seconds(),
    );
    let mut budget = AVATAR_BUDGET;
    let mut avatar = |id: AttachmentId| session_avatar(session, &mut budget, id);
    Ok(contacts
        .iter()
        .filter_map(|contact| {
            let mut view = contact_view(contact, book_id, &mut avatar)?;
            if let Some(edit) = str_field(&view, "id").and_then(|id| badges.get(id)) {
                apply_badge(&mut view, edit);
            }
            Some(view)
        })
        .collect())
}

/// `YYYY-MM-DD` (UTC) for a Unix timestamp (Howard Hinnant's civil-from-days).
fn utc_date(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400) + 719_468;
    let era = days.div_euclid(146_097);
    let doe = days - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

pub fn restorable(session: &Session, book_id: &str) -> BridgeResult<Vec<Value>> {
    if !valid_id(book_id) {
        return Err(invalid("The contact book is invalid."));
    }
    let raw = parse_core(
        session
            .client
            .list_restorable_contacts_json(&json!({"book_id": book_id}).to_string()),
    )?;
    let contacts = raw
        .get("contacts")
        .and_then(Value::as_array)
        .ok_or_else(BridgeError::host_state)?;
    let mut budget = AVATAR_BUDGET;
    Ok(contacts
        .iter()
        .filter_map(|contact| {
            let id = str_field(contact, "id").filter(|id| valid_id(id))?;
            let mut out = json!({
                "id": id,
                "bookId": book_id,
                "displayName": str_field(contact, "display_name").filter(|n| !n.is_empty()).unwrap_or("Contact"),
                "deletedAt": utc_date(contact.get("deleted_at").and_then(Value::as_i64)?),
            });
            if let Some((photo, true)) = photo_reference(contact) {
                if let Some(url) = session_avatar(session, &mut budget, photo) {
                    out["photoDataUrl"] = json!(url);
                }
            }
            Some(out)
        })
        .collect())
}

/// Asks the book owner to recreate a retained tombstone (core `restore_contact_json`). The result
/// is a pending request like any other edit; only the owner's result makes it applied.
pub fn restore(session: &Session, book_id: &str, contact_id: &str) -> BridgeResult<Value> {
    if !valid_id(book_id) || !valid_id(contact_id) {
        return Err(invalid("The contact is invalid."));
    }
    let book = find_book(&session.client, book_id)?;
    if !capabilities(&book, &session.binding.device_id).can_restore {
        return Err(BridgeError::new(
            "contact-book-read-only",
            "This contact book does not accept changes from this computer.",
        ));
    }
    let result =
        parse_core(session.client.restore_contact_json(
            &json!({"book_id": book_id, "contact_id": contact_id}).to_string(),
        ))?;
    let request_id = str_field(&result, "request_id").ok_or_else(BridgeError::host_state)?;
    session.request_work();
    session.notify();
    Ok(json!({"state": "pending", "requestId": request_id}))
}

// ---------------------------------------------------------------------------------------------
// Edit status (requester ledger)
// ---------------------------------------------------------------------------------------------

/// Requester-side ledger status as a UI state. A request still unanswered past its expiry can no
/// longer receive a permit, so it reads as expired.
fn edit_state(status: &str, expires_at: i64, now: i64) -> Option<&'static str> {
    Some(match status {
        "requested" | "approved" | "awaiting_approval" if expires_at <= now => "expired",
        "requested" | "approved" | "applying" => "pending",
        "awaiting_approval" => "awaiting-approval",
        "outcome_unknown" => "outcome-unknown",
        "applied" => "applied",
        "conflict" => "conflict",
        "rejected" => "rejected",
        "expired" => "expired",
        "failed" => "failed",
        _ => return None,
    })
}

fn safe_reason(result: &Value) -> Option<&str> {
    str_field(result, "reason").filter(|reason| {
        !reason.is_empty()
            && reason.len() <= 64
            && reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
    })
}

fn list_edits_for(
    client: &Client,
    own_device: &str,
    book_id: Option<&str>,
) -> BridgeResult<Vec<Value>> {
    let raw = parse_core(client.list_contact_requests_json(
        &json!({"schema_version": 1, "requester": own_device}).to_string(),
    ))?;
    let now = now_seconds();
    Ok(raw
        .get("requests")
        .and_then(Value::as_array)
        .ok_or_else(BridgeError::host_state)?
        .iter()
        .filter(|request| book_id.is_none_or(|book| str_field(request, "book_id") == Some(book)))
        .filter_map(|request| {
            let expires_at = request.get("expires_at").and_then(Value::as_i64)?;
            let state = edit_state(str_field(request, "state")?, expires_at, now)?;
            let mut out = json!({
                "requestId": str_field(request, "request_id")?,
                "bookId": str_field(request, "book_id")?,
                "state": state,
                "expiresAt": expires_at,
                "summary": edit_summary(request),
            });
            if let Some(reason) = request.get("observed").and_then(safe_reason) {
                out["reason"] = json!(reason);
            }
            if let Some(kind) =
                str_field(request, "kind").filter(|k| matches!(*k, "create" | "update" | "delete"))
            {
                out["kind"] = json!(kind);
            }
            if let Some(contact) = str_field(request, "contact_id").filter(|id| valid_id(id)) {
                out["contactId"] = json!(contact);
            }
            if let Some(name) = str_field(request, "display_name").filter(|n| !n.is_empty()) {
                out["displayName"] = json!(name.chars().take(MAX_TEXT).collect::<String>());
            }
            Some(out)
        })
        .collect())
}

/// Short human summary from core's sanitized `kind` + `field_paths` (no values).
fn edit_summary(request: &Value) -> String {
    let mut fields: Vec<&str> = Vec::new();
    for path in request
        .get("field_paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        let root = path.split(['.', '[']).next().unwrap_or(path);
        let label = match root {
            "name" | "display_name" => "name",
            "phones" => "phone numbers",
            "emails" => "email addresses",
            "addresses" => "addresses",
            "notes" => "notes",
            "photo" => "photo",
            "birthday" => "birthday",
            "nickname" => "nickname",
            "organization" | "title" => "work details",
            _ => continue,
        };
        if !fields.contains(&label) {
            fields.push(label);
        }
    }
    match str_field(request, "kind") {
        Some("create") => "New contact".to_owned(),
        Some("delete") => "Delete contact".to_owned(),
        _ if fields.is_empty() => "Contact change".to_owned(),
        _ => {
            let joined = fields.join(", ");
            let mut chars = joined.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        }
    }
}

/// The edit to badge on each contact: the newest unresolved request, or a recent failed one
/// (conflict, rejected, failed, expired) until a day after its expiry. Applied edits clear.
fn contact_badges(edits: &[Value], now: i64) -> HashMap<String, Value> {
    let mut badges = HashMap::new();
    for edit in edits {
        let (Some(contact), Some(state)) = (str_field(edit, "contactId"), str_field(edit, "state"))
        else {
            continue;
        };
        if badges.contains_key(contact) {
            continue;
        }
        let unresolved = matches!(state, "pending" | "awaiting-approval" | "outcome-unknown");
        let recent = edit
            .get("expiresAt")
            .and_then(Value::as_i64)
            .is_some_and(|expires| now < expires.saturating_add(24 * 60 * 60));
        if state == "applied" {
            badges.insert(contact.to_owned(), Value::Null);
        } else if unresolved || recent {
            badges.insert(contact.to_owned(), edit.clone());
        }
    }
    badges.retain(|_, edit| !edit.is_null());
    badges
}

fn apply_badge(view: &mut Value, edit: &Value) {
    view["pendingEditId"] = edit["requestId"].clone();
    view["pendingEditState"] = edit["state"].clone();
    view["pendingEditSummary"] = edit["summary"].clone();
}

pub fn list_edits(session: &Session, book_id: Option<&str>) -> BridgeResult<Vec<Value>> {
    list_edits_for(&session.client, &session.binding.device_id, book_id)
}

fn pending_counts(edits: &[Value]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for edit in edits {
        if matches!(
            str_field(edit, "state"),
            Some("pending" | "awaiting-approval" | "outcome-unknown")
        ) {
            if let Some(book) = str_field(edit, "bookId") {
                *counts.entry(book.to_owned()).or_default() += 1;
            }
        }
    }
    counts
}

// ---------------------------------------------------------------------------------------------
// Edit requests
// ---------------------------------------------------------------------------------------------

/// Core list item from a view item. `None` when the item carries no value (a blank row).
fn wire_item(list: List, item: &Value, id: &str) -> BridgeResult<Option<Value>> {
    let obj = item
        .as_object()
        .ok_or_else(|| invalid("A contact list entry is invalid."))?;
    let mut out = Map::new();
    out.insert("id".into(), json!(id));
    let label = ui_text(obj.get("label"), MAX_TEXT, false)?;
    if !label.is_empty() {
        out.insert("label".into(), json!(label));
    }
    let mut has_value = false;
    let mut put = |core: &str, view: &str| -> BridgeResult<()> {
        let text = ui_text(obj.get(view), MAX_TEXT, false)?;
        if !text.trim().is_empty() {
            out.insert(core.into(), json!(text));
            has_value = true;
        }
        Ok(())
    };
    match list {
        List::Phones | List::Emails => put("value", list.value_key())?,
        List::Addresses => {
            for (core, view) in ADDRESS_KEYS {
                put(core, view)?;
            }
        }
    }
    Ok(has_value.then_some(Value::Object(out)))
}

/// A field ID from the view; core item paths are `list[<id>]`, so brackets are refused.
fn item_id(item: &Value) -> BridgeResult<Option<String>> {
    match item.get("id") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(id)) if valid_id(id) && !id.contains(['[', ']']) => Ok(Some(id.clone())),
        Some(_) => Err(invalid("A contact field ID is invalid.")),
    }
}

fn ui_items(value: Option<&Value>) -> BridgeResult<&[Value]> {
    match value {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) if items.len() <= MAX_VALUES * 2 => Ok(items),
        Some(_) => Err(invalid("Contact list changes are invalid.")),
    }
}

/// Items for a new contact: blank rows are dropped and every item gets a fresh field ID.
fn new_items(list: List, value: Option<&Value>) -> BridgeResult<Vec<Value>> {
    let mut items = Vec::new();
    for item in ui_items(value)? {
        if let Some(wire) = wire_item(list, item, &uuid::Uuid::new_v4().to_string())? {
            items.push(wire);
        }
    }
    if items.len() > MAX_VALUES {
        return Err(invalid("A contact can have at most 20 entries per list."));
    }
    Ok(items)
}

/// Patches turning `old` (the view's original items) into `next`. Existing items keep their IDs;
/// blank or missing items are removed, new ones are added with fresh IDs. Returns the patches and
/// the original list in core form for `expected_old`.
fn list_patches(
    list: List,
    next: Option<&Value>,
    old: Option<&Value>,
) -> BridgeResult<(Vec<Value>, Value)> {
    let key = list.core_key();
    let old = match old {
        Some(Value::Array(items)) => items,
        _ => {
            return Err(invalid(
                "Contact list changes require their original values.",
            ))
        }
    };
    let mut old_wire = Vec::with_capacity(old.len());
    let mut read_only = HashSet::new();
    for item in old {
        let id = item_id(item)?.ok_or_else(|| invalid("An original contact field has no ID."))?;
        if item.get("readOnly").and_then(Value::as_bool) == Some(true) {
            read_only.insert(id.clone());
        }
        let wire = wire_item(list, item, &id)?.unwrap_or_else(|| json!({"id": id}));
        old_wire.push((id, wire));
    }
    let mut kept = HashMap::new();
    let mut added = Vec::new();
    for item in ui_items(next)? {
        match item_id(item)? {
            Some(id) => {
                if !old_wire.iter().any(|(old_id, _)| *old_id == id) {
                    return Err(invalid(
                        "A contact field ID does not belong to this contact.",
                    ));
                }
                if kept
                    .insert(id.clone(), wire_item(list, item, &id)?)
                    .is_some()
                {
                    return Err(invalid("A contact field appears twice."));
                }
            }
            None => added.extend(wire_item(list, item, &uuid::Uuid::new_v4().to_string())?),
        }
    }
    let mut patches = Vec::new();
    let mut remaining = 0;
    for (id, previous) in &old_wire {
        let path = format!("{key}[{id}]");
        match kept.remove(id).flatten() {
            Some(current) if current == *previous => remaining += 1,
            current => {
                if read_only.contains(id) {
                    return Err(BridgeError::new(
                        "contact-field-read-only",
                        "A read-only contact field cannot be changed from this computer.",
                    ));
                }
                match current {
                    Some(current) => {
                        remaining += 1;
                        patches.push(json!({"op": "replace", "path": path, "value": current}));
                    }
                    None => patches.push(json!({"op": "remove", "path": path})),
                }
            }
        }
    }
    if remaining + added.len() > MAX_VALUES {
        return Err(invalid("A contact can have at most 20 entries per list."));
    }
    patches.extend(
        added
            .into_iter()
            .map(|item| json!({"op": "add", "path": key, "value": item})),
    );
    let old_core = Value::Array(old_wire.into_iter().map(|(_, wire)| wire).collect());
    Ok((patches, old_core))
}

/// Scalar field → (core path, limit, limit counts bytes, is notes).
fn scalar(field: &str) -> Option<(&'static str, usize, bool)> {
    Some(match field {
        "givenName" => ("name.given", MAX_TEXT, false),
        "familyName" => ("name.family", MAX_TEXT, false),
        "nickname" => ("nickname", MAX_TEXT, false),
        "organization" => ("organization", MAX_TEXT, false),
        "title" => ("title", MAX_TEXT, false),
        "notes" => ("notes", MAX_NOTES_BYTES, true),
        _ => return None,
    })
}

/// A view birthday as core `{year?, month, day}`: absent, `null` or all-blank parts mean no
/// birthday. Parts may be numbers or numeric strings (form inputs); core validates the date.
fn ui_birthday(value: Option<&Value>) -> BridgeResult<Option<Value>> {
    let bad = || invalid("The birthday is invalid.");
    let parts = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Object(parts)) => parts,
        Some(_) => return Err(bad()),
    };
    let mut out = Map::new();
    for (key, part) in parts {
        if !matches!(key.as_str(), "year" | "month" | "day") {
            return Err(bad());
        }
        let number = match part {
            Value::Null => continue,
            Value::String(text) if text.trim().is_empty() => continue,
            Value::String(text) => text.trim().parse::<i64>().map_err(|_| bad())?,
            Value::Number(n) => n.as_i64().ok_or_else(bad)?,
            _ => return Err(bad()),
        };
        out.insert(key.clone(), json!(number));
    }
    if out.is_empty() {
        return Ok(None);
    }
    let month = out.get("month").and_then(Value::as_i64).ok_or_else(bad)?;
    let day = out.get("day").and_then(Value::as_i64).ok_or_else(bad)?;
    let year_ok = out
        .get("year")
        .and_then(Value::as_i64)
        .is_none_or(|year| (1..=9999).contains(&year));
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || !year_ok {
        return Err(bad());
    }
    Ok(Some(Value::Object(out)))
}

fn require_birthday_support(caps: &Capabilities) -> BridgeResult<()> {
    if caps.supports_birthday {
        Ok(())
    } else {
        Err(BridgeError::new(
            "contact-field-unsupported",
            "This phone does not accept contact birthdays.",
        ))
    }
}

fn reject_notes(caps: &Capabilities, notes: &str) -> BridgeResult<()> {
    if notes.is_empty() || caps.supports_notes {
        Ok(())
    } else {
        Err(BridgeError::new(
            "contact-field-unsupported",
            "This phone does not accept contact notes.",
        ))
    }
}

/// UI patches as (field, entry), refusing duplicates and oversized inputs.
fn ui_patches(input: &Value) -> BridgeResult<Vec<(&str, &Value)>> {
    let patches = match input.get("patches") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(patches)) if patches.len() <= MAX_PATCHES => patches,
        Some(_) => return Err(invalid("Contact changes are invalid.")),
    };
    let mut seen = HashSet::new();
    patches
        .iter()
        .map(|patch| {
            let field =
                str_field(patch, "field").ok_or_else(|| invalid("A contact field is invalid."))?;
            if !seen.insert(field) {
                return Err(invalid("A contact field was changed twice."));
            }
            Ok((field, patch))
        })
        .collect()
}

fn create_fields(input: &Value, caps: &Capabilities) -> BridgeResult<Map<String, Value>> {
    let mut fields = Map::new();
    let mut name = Map::new();
    for (field, patch) in ui_patches(input)? {
        let value = patch.get("value");
        if let Some(list) = List::from_field(field) {
            let items = new_items(list, value)?;
            if !items.is_empty() {
                fields.insert(list.core_key().into(), Value::Array(items));
            }
        } else if let Some((path, max, bytes)) = scalar(field) {
            let text = ui_text(value, max, bytes)?;
            if path == "notes" {
                reject_notes(caps, &text)?;
            }
            if text.trim().is_empty() {
                continue;
            }
            match path.strip_prefix("name.") {
                Some(part) => name.insert(part.into(), json!(text)),
                None => fields.insert(path.into(), json!(text)),
            };
        } else if field == "birthday" {
            if let Some(birthday) = ui_birthday(value)? {
                require_birthday_support(caps)?;
                fields.insert("birthday".into(), birthday);
            }
        } else {
            return Err(invalid(
                "This contact field cannot be created from this computer.",
            ));
        }
    }
    let first_value = |list: &str| {
        fields
            .get(list)
            .and_then(|items| items.get(0))
            .and_then(|item| str_field(item, "value"))
            .map(str::to_owned)
    };
    let person = ["given", "family"]
        .iter()
        .filter_map(|part| name.get(*part).and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(" ");
    let display = [Some(person)]
        .into_iter()
        .chain(
            ["nickname", "organization"]
                .map(|key| fields.get(key).and_then(Value::as_str).map(str::to_owned)),
        )
        .chain([first_value("phones"), first_value("emails")])
        .flatten()
        .find(|candidate| !candidate.trim().is_empty())
        .ok_or_else(|| invalid("A new contact needs a name, phone number or email address."))?;
    fields.insert(
        "display_name".into(),
        json!(display.chars().take(MAX_TEXT).collect::<String>()),
    );
    if !name.is_empty() {
        fields.insert("name".into(), Value::Object(name));
    }
    Ok(fields)
}

/// Update patches and the matching `expected_old` subset of the original contact.
fn update_patches(
    input: &Value,
    caps: &Capabilities,
) -> BridgeResult<(Vec<Value>, Map<String, Value>)> {
    let mut patches = Vec::new();
    let mut expected = Map::new();
    for (field, patch) in ui_patches(input)? {
        let value = patch.get("value");
        if let Some(list) = List::from_field(field) {
            let (changes, previous) = list_patches(list, value, patch.get("expectedOld"))?;
            if !changes.is_empty() {
                patches.extend(changes);
                expected.insert(list.core_key().into(), previous);
            }
        } else if let Some((path, max, bytes)) = scalar(field) {
            let text = ui_text(value, max, bytes)?;
            if path == "notes" {
                reject_notes(caps, &text)?;
            }
            let old = ui_text(patch.get("expectedOld"), max, bytes)?;
            if text == old {
                continue;
            }
            patches.push(if text.is_empty() {
                json!({"op": "remove", "path": path})
            } else {
                json!({"op": "replace", "path": path, "value": text})
            });
            let old = if old.is_empty() {
                Value::Null
            } else {
                json!(old)
            };
            match path.split_once('.') {
                Some((root, part)) => {
                    let parent = expected.entry(root).or_insert_with(|| json!({}));
                    parent[part] = old;
                }
                None => {
                    expected.insert(path.into(), old);
                }
            }
        } else if field == "birthday" {
            let next = ui_birthday(value)?;
            let old = ui_birthday(patch.get("expectedOld"))?;
            if next == old {
                continue;
            }
            require_birthday_support(caps)?;
            patches.push(match &next {
                Some(birthday) => json!({"op": "replace", "path": "birthday", "value": birthday}),
                None => json!({"op": "remove", "path": "birthday"}),
            });
            expected.insert("birthday".into(), old.unwrap_or(Value::Null));
        } else {
            return Err(invalid(
                "This contact field cannot be changed from this computer.",
            ));
        }
    }
    if patches.len() > MAX_PATCHES {
        return Err(invalid("Too many contact changes."));
    }
    Ok((patches, expected))
}

/// Builds a core `ContactEditRequest` (without `photo_op`) for `book`. `photo_change` says whether
/// the caller will add a photo operation, which alone makes an update non-empty.
pub fn build_request(
    input: &Value,
    book: &Value,
    own_device: &str,
    photo_change: bool,
) -> BridgeResult<Value> {
    let kind = str_field(input, "kind").unwrap_or("");
    if !matches!(kind, "create" | "update" | "delete") {
        return Err(invalid("The contact operation is invalid."));
    }
    let book_id = required_id(input, "targetBookId")?;
    if str_field(book, "id") != Some(book_id.as_str()) {
        return Err(invalid("The contact book is invalid."));
    }
    let owner = str_field(book, "owner_device_id")
        .filter(|owner| valid_id(owner))
        .ok_or_else(BridgeError::host_state)?;
    let caps = capabilities(book, own_device);
    if !caps.can_write {
        return Err(BridgeError::new(
            "contact-book-read-only",
            "This contact book does not accept changes from this computer.",
        ));
    }
    if photo_change && (kind == "delete" || !caps.supports_photo) {
        return Err(BridgeError::new(
            "contact-field-unsupported",
            "This phone does not accept contact photo changes.",
        ));
    }
    let mut request = json!({
        "schema_version": 1,
        "request_id": uuid::Uuid::new_v4().to_string(),
        "target_owner": owner,
        "book_id": book_id,
        "kind": kind,
    });
    match kind {
        "create" => {
            for (key, value) in create_fields(input, &caps)? {
                request[key] = value;
            }
        }
        "update" => {
            request["contact_id"] = json!(required_id(input, "contactId")?);
            request["base_revision"] = json!(required_revision(input)?);
            let (patches, expected) = update_patches(input, &caps)?;
            if patches.is_empty() && !photo_change {
                return Err(invalid("No contact changes were made."));
            }
            request["patches"] = Value::Array(patches);
            request["expected_old"] = Value::Object(expected);
        }
        _ => {
            request["contact_id"] = json!(required_id(input, "contactId")?);
            request["base_revision"] = json!(required_revision(input)?);
        }
    }
    Ok(request)
}

// ---------------------------------------------------------------------------------------------
// Photos
// ---------------------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum PhotoInput {
    Keep,
    Remove,
    Set(Vec<u8>),
}

fn photo_error() -> BridgeError {
    BridgeError::new(
        "contact-photo-invalid",
        "The photo could not be used. Choose a JPEG, PNG or WebP image up to 8 MB.",
    )
}

fn is_photo_format(bytes: &[u8]) -> bool {
    matches!(
        media::sniff_media_type(bytes),
        "image/png" | "image/jpeg" | "image/webp"
    )
}

/// The cropper's `data:image/{png,jpeg,webp};base64,…` output, size-bounded and sniffed.
fn decode_photo_data_url(url: &str) -> BridgeResult<Vec<u8>> {
    let (header, data) = url
        .strip_prefix("data:")
        .and_then(|rest| rest.split_once(','))
        .ok_or_else(photo_error)?;
    if !matches!(
        header,
        "image/png;base64" | "image/jpeg;base64" | "image/webp;base64"
    ) || data.len() > MAX_PHOTO_SOURCE_BYTES / 3 * 4 + 4
    {
        return Err(photo_error());
    }
    let bytes = STANDARD.decode(data).map_err(|_| photo_error())?;
    if bytes.len() > MAX_PHOTO_SOURCE_BYTES || !is_photo_format(&bytes) {
        return Err(photo_error());
    }
    Ok(bytes)
}

pub fn photo_input(input: &Value) -> BridgeResult<PhotoInput> {
    let Some(photo) = input.get("photo").filter(|photo| !photo.is_null()) else {
        return Ok(PhotoInput::Keep);
    };
    match str_field(photo, "kind") {
        Some("keep") => Ok(PhotoInput::Keep),
        Some("remove") => Ok(PhotoInput::Remove),
        Some("set") => str_field(photo, "croppedDataUrl")
            .ok_or_else(photo_error)
            .and_then(decode_photo_data_url)
            .map(PhotoInput::Set),
        _ => Err(invalid("The contact photo change is invalid.")),
    }
}

/// Removes the staged plaintext when dropped, including on early returns and panics.
struct Staged(PathBuf);
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Private staging directory inside the binding's data directory. Leftovers from a crash are
/// purged once they are older than a few minutes.
fn staging_dir(data_dir: &Path) -> BridgeResult<PathBuf> {
    let dir = data_dir.join(PHOTO_STAGING_DIR);
    fs::create_dir_all(&dir).map_err(|_| {
        BridgeError::new(
            "attachment-local",
            "Local attachment storage is unavailable.",
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| modified.elapsed().ok())
                .is_some_and(|age| age > STAGING_MAX_AGE);
            if stale {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    Ok(dir)
}

/// Normalizes the user's cropped image through core (`prepare_contact_photo`: re-encoded 256×256
/// JPEG, metadata stripped, encrypted into the media store) and returns the local attachment ID.
/// The plaintext exists only as a 0600 file in the private staging directory for the call.
pub fn prepare_photo(client: &Client, data_dir: &Path, bytes: &[u8]) -> BridgeResult<AttachmentId> {
    let staged =
        Staged(staging_dir(data_dir)?.join(format!("{}.img", uuid::Uuid::new_v4().simple())));
    fsutil::write_private_atomic(&staged.0, bytes).map_err(|_| {
        BridgeError::new(
            "attachment-local",
            "Local attachment storage is unavailable.",
        )
    })?;
    let info = client
        .prepare_contact_photo(&staged.0)
        .map_err(|error| match error {
            CoreError::InvalidRequest(_) => photo_error(),
            other => core_error(other),
        })?;
    Ok(info.attachment_id)
}

/// Natively picked image re-encoded for the webview cropper (bounded, metadata-free PNG).
pub fn photo_source(path: &Path) -> BridgeResult<Value> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_PHOTO_SOURCE_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(|_| photo_error())?;
    if bytes.len() > MAX_PHOTO_SOURCE_BYTES || !is_photo_format(&bytes) {
        return Err(photo_error());
    }
    let (data_url, width, height) =
        media::thumbnail_data_url(&bytes, CROP_SOURCE_EDGE).ok_or_else(photo_error)?;
    Ok(json!({"dataUrl": data_url, "naturalWidth": width, "naturalHeight": height}))
}

// ---------------------------------------------------------------------------------------------
// Submit
// ---------------------------------------------------------------------------------------------

/// Validates and sends one edit request. Returns `{state:"pending", requestId}`; the owner's
/// decision arrives later through the ledger (`list_edits`).
pub fn submit_with(
    client: &Client,
    data_dir: &Path,
    own_device: &str,
    input: &Value,
) -> BridgeResult<Value> {
    let book_id = required_id(input, "targetBookId")?;
    let book = find_book(client, &book_id)?;
    let photo = photo_input(input)?;
    let mut request = build_request(input, &book, own_device, photo != PhotoInput::Keep)?;
    let prepared = match photo {
        PhotoInput::Keep => None,
        PhotoInput::Remove => {
            request["photo_op"] = json!({"op": "remove"});
            None
        }
        PhotoInput::Set(bytes) => {
            let id = prepare_photo(client, data_dir, &bytes)?;
            request["photo_op"] = json!({"op": "set", "attachment_id": id.to_string()});
            Some(id)
        }
    };
    let result = client
        .request_contact_edit(&request.to_string())
        .map_err(|error| {
            if let Some(id) = prepared {
                let _ = client.discard_unreferenced_attachment(id);
            }
            core_error(error)
        })?;
    let result: Value = serde_json::from_str(&result).map_err(|_| BridgeError::host_state())?;
    let request_id = str_field(&result, "request_id").ok_or_else(BridgeError::host_state)?;
    Ok(json!({"state": "pending", "requestId": request_id}))
}

pub fn submit(session: &Session, input: &Value) -> BridgeResult<Value> {
    let outcome = submit_with(
        &session.client,
        &session.data_dir,
        &session.binding.device_id,
        input,
    )?;
    session.request_work();
    session.notify();
    Ok(outcome)
}

// ---------------------------------------------------------------------------------------------
// Display-only resolution, recipient discovery and repair
// ---------------------------------------------------------------------------------------------

/// Core's per-call address cap for `resolve_contact_addresses_json`.
const RESOLVE_BATCH: usize = 100;
/// Distinct addresses resolved per snapshot; the rest show their raw number.
const MAX_RESOLVED_ADDRESSES: usize = 1000;
/// Avatars attached to one snapshot (decrypts are cached, so this bounds snapshot size).
const SNAPSHOT_AVATARS: usize = 64;
const MAX_SEARCH_RESULTS: u64 = 20;

/// A display name (and maybe photo) for one phone address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub contact_id: String,
    pub book_id: String,
    pub display_name: String,
    pub photo: Option<AttachmentId>,
}

fn looks_like_phone(address: &str) -> bool {
    let digits = address.bytes().filter(u8::is_ascii_digit).count();
    (3..=15).contains(&digits)
        && address
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b' ' | b'(' | b')' | b'.' | b'-'))
}

/// Resolves phone-number addresses through core's bounded projection, preferring the given
/// source device's book. Ambiguous or unknown numbers are simply absent (shown raw). Purely
/// display data: failures (for example a locked vault) resolve nothing.
pub fn resolve_addresses(
    client: &Client,
    addresses: &[(String, Option<String>)],
) -> HashMap<String, Resolved> {
    let mut by_source: HashMap<Option<&str>, Vec<&str>> = HashMap::new();
    let mut seen = HashSet::new();
    for (address, source) in addresses {
        if address.len() <= 256 && looks_like_phone(address) && seen.insert(address.as_str()) {
            by_source
                .entry(source.as_deref().filter(|s| valid_id(s)))
                .or_default()
                .push(address);
        }
        if seen.len() >= MAX_RESOLVED_ADDRESSES {
            break;
        }
    }
    let mut out = HashMap::new();
    for (source, list) in by_source {
        for batch in list.chunks(RESOLVE_BATCH) {
            let mut input = json!({"addresses": batch});
            if let Some(source) = source {
                input["source_device_id"] = json!(source);
            }
            let Ok(raw) = parse_core(client.resolve_contact_addresses_json(&input.to_string()))
            else {
                return out;
            };
            for item in raw
                .get("matches")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let (Some(address), Some(contact_id), Some(book_id)) = (
                    str_field(item, "address"),
                    str_field(item, "contact_id"),
                    str_field(item, "book_id"),
                ) else {
                    continue;
                };
                let Some(name) = str_field(item, "display_name").filter(|n| !n.trim().is_empty())
                else {
                    continue;
                };
                out.entry(address.to_owned()).or_insert_with(|| Resolved {
                    contact_id: contact_id.to_owned(),
                    book_id: book_id.to_owned(),
                    display_name: name.chars().take(MAX_TEXT).collect(),
                    photo: str_field(item, "photo_attachment_id")
                        .and_then(|id| AttachmentId::from_str(id).ok()),
                });
            }
        }
    }
    out
}

/// Contact data carried in each UI snapshot. Every part is optional display data.
#[derive(Default)]
pub struct SnapshotContacts {
    pub resolution: Option<Map<String, Value>>,
    pub books: Option<Vec<Value>>,
    pub pending_count: Option<u64>,
    pub sync: Option<Value>,
}

pub fn snapshot_contacts(
    session: &Session,
    addresses: &[(String, Option<String>)],
) -> SnapshotContacts {
    let resolved = resolve_addresses(&session.client, addresses);
    let mut budget = SNAPSHOT_AVATARS;
    let resolution = resolved
        .into_iter()
        .map(|(address, contact)| {
            let mut view = json!({
                "contactId": contact.contact_id,
                "bookId": contact.book_id,
                "displayName": contact.display_name,
            });
            if let Some(url) = contact
                .photo
                .filter(|id| {
                    session
                        .client
                        .attachment_info(*id)
                        .is_ok_and(|info| info.state.is_local())
                })
                .and_then(|id| session_avatar(session, &mut budget, id))
            {
                view["photoDataUrl"] = json!(url);
            }
            (address, view)
        })
        .collect::<Map<_, _>>();
    let books = list_books(session).ok();
    let pending_count = books.as_ref().map(|books| {
        books
            .iter()
            .filter_map(|book| book.get("pendingEditCount").and_then(Value::as_u64))
            .sum()
    });
    SnapshotContacts {
        resolution: (!resolution.is_empty()).then_some(resolution),
        books,
        pending_count,
        sync: Some(sync_status(&session.client)),
    }
}

/// Repair latch and newest authoritative projection, so the UI shows failure or staleness
/// instead of treating contacts as current.
pub fn sync_status(client: &Client) -> Value {
    use peppy_client_core::SnapshotProjectionState as State;
    let mut out = json!({"repairRequired": client.contact_repair_required().unwrap_or(false)});
    // This is presentation-only readiness metadata. Core remains the mutation authority.
    if let Ok(readiness) = client
        .contact_sync_readiness_json()
        .and_then(|json| serde_json::from_str(&json).map_err(|_| CoreError::Database))
    {
        out["readiness"] = readiness;
    }
    if let Ok(Some(status)) = client.snapshot_projection_status() {
        let state = match status.state {
            State::Draining | State::Staging => "rebuilding",
            State::Promoted => "current",
            State::Failed => "failed",
        };
        out["projection"] = json!({"state": state});
        if let Some(reason) = status
            .reason
            .filter(|r| r.len() <= 64 && r.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'))
        {
            out["projection"]["reason"] = json!(reason);
        }
    }
    out
}

/// Manual repair: latches core's projection repair and wakes the live loop, which runs one
/// fenced compaction snapshot. Owned books are never republished wholesale.
pub fn request_repair(session: &Session) -> BridgeResult<Value> {
    session
        .client
        .request_contact_repair()
        .map_err(core_error)?;
    session
        .repair_attempted
        .store(false, std::sync::atomic::Ordering::SeqCst);
    session.repair_wake.notify_one();
    session.request_work();
    session.notify();
    Ok(sync_status(&session.client))
}

/// Phone numbers of contacts matching `query` (names or digits) as recipient candidates. The
/// recipient is the phone address (core-normalized E.164 when the number is valid for the
/// book's region; otherwise the raw digits, e.g. a short code), never a contact ID.
pub fn search_recipients(
    session: &Session,
    query: &str,
    source_device_id: Option<&str>,
) -> BridgeResult<Vec<Value>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(Vec::new());
    }
    if query.chars().count() > 128 {
        return Err(invalid("The contact search is too long."));
    }
    let mut input = json!({"schema_version": 1, "query": query, "limit": MAX_SEARCH_RESULTS});
    if let Some(source) = source_device_id.filter(|s| valid_id(s)) {
        input["source_device_id"] = json!(source);
    }
    let raw = parse_core(
        session
            .client
            .search_contact_recipients_json(&input.to_string()),
    )?;
    let mut budget = MAX_SEARCH_RESULTS as usize;
    Ok(raw
        .get("results")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|row| {
            let address = str_field(row, "address")?;
            let mut out = json!({
                "address": address,
                "displayName": str_field(row, "display_name").filter(|n| !n.is_empty()).unwrap_or(address),
                "number": str_field(row, "value").unwrap_or(address),
                "normalized": row.get("normalized").and_then(Value::as_bool) == Some(true),
                "contactId": str_field(row, "contact_id")?,
                "phoneId": str_field(row, "phone_id")?,
            });
            if let Some(label) = str_field(row, "label") {
                out["label"] = json!(display_label(label));
            }
            if let Some(url) = str_field(row, "photo_attachment_id")
                .and_then(|id| AttachmentId::from_str(id).ok())
                .filter(|id| {
                    session
                        .client
                        .attachment_info(*id)
                        .is_ok_and(|info| info.state.is_local())
                })
                .and_then(|id| session_avatar(session, &mut budget, id))
            {
                out["avatarUrl"] = json!(url);
            }
            Some(out)
        })
        .collect())
}

/// Native banner title for a phone-number title (full previews only): the contact name when
/// the number resolves unambiguously, else the original title. Never used for hidden previews.
pub fn banner_title(client: &Client, title: &str, source_device_id: Option<&str>) -> String {
    resolve_addresses(
        client,
        &[(title.to_owned(), source_device_id.map(str::to_owned))],
    )
    .remove(title)
    .map_or_else(|| title.to_owned(), |contact| contact.display_name)
}

#[cfg(test)]
pub(crate) mod tests;
