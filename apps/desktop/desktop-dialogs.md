# Desktop passphrase dialogs

- **Windows:** uses the official `windows` crate binding for `CredUIPromptForCredentialsW` with `CREDUI_FLAGS_GENERIC_CREDENTIALS`, `CREDUI_FLAGS_PASSWORD_ONLY_OK`, `CREDUI_FLAGS_ALWAYS_SHOW_UI`, and `CREDUI_FLAGS_DO_NOT_PERSIST`. The input is a native password control and Credential Manager persistence is disabled.
- **Linux:** invokes an installed, reviewed native password helper without a shell: `zenity --password` first, then `kdialog --password`. Arguments never contain the passphrase. Missing helpers return an explicit native-dialog gate rather than falling back to the webview.
- **Limits:** helper stdout is capped at 4 KiB, decoded UTF-8 is bounded, and temporary byte/UTF-16 buffers are zeroized. Cancellation returns `None` without changing state.

## CI checks

Run from the repository root:

```sh
cargo check --manifest-path apps/desktop/src-tauri/Cargo.toml --target x86_64-pc-windows-msvc
cargo check --manifest-path apps/desktop/src-tauri/Cargo.toml --target x86_64-unknown-linux-gnu
cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml dialogs::tests
```

Windows and Linux builds were **not executed locally**; CI owns cross-platform compilation. Formatting was also not run because the installed Rust toolchain lacks the `cargo fmt` component (`cargo fmt` is unavailable).

`cargo metadata --offline --manifest-path apps/desktop/src-tauri/Cargo.toml --format-version 1` completed with the installed toolchain after explicitly setting `RUSTC`; it refreshed the package lock entry for the target-specific `windows` dependency.
