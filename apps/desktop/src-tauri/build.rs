/// Every app command is declared in the app ACL manifest. Tauri then rejects any app command
/// that a window's capability does not explicitly allow (`capabilities/*.json`), so composer
/// windows can only reach the conversation/draft/read/attachment/close commands.
const COMMANDS: &[&str] = &[
    "load_state",
    "configure_server",
    "import_credentials",
    "unlock_sync",
    "save_draft",
    "send_draft",
    "mark_seen",
    "pick_attachments",
    "retry_attachment",
    "save_attachment",
    "publish_attachment",
    "open_composer",
    "dismiss_notification",
    "dismiss_all_notifications",
    "set_app_muted",
    "mark_notifications_seen",
    "set_notification_preferences",
    "set_notification_context",
    "request_notification_permission",
    "list_contact_books",
    "list_contacts",
    "forget_contact_book",
    "submit_contact_edit",
    "list_contact_edits",
    "pick_contact_photo",
    "search_contact_recipients",
    "request_contact_repair",
    "list_restorable_contacts",
    "restore_contact",
    "hide_head",
    "close_composer",
    "close_head_panel",
    "set_start_at_login",
    "popout_conversation",
    "acknowledge_lifecycle",
    "hosted_preview_state",
    "hosted_preview_start",
    "hosted_preview_advance",
    "hosted_preview_create_passphrase",
    "hosted_preview_unlock",
    "hosted_preview_reset",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
