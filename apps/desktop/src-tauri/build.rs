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
    "show_head",
    "update_head",
    "hide_head",
    "close_composer",
    "native_head_probe",
    "show_conversation_head",
    "update_conversation_head",
    "hide_conversation_head",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
