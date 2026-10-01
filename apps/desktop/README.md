# OpenPush desktop client

Follow the canonical [contributing workflow](../../CONTRIBUTING.md) for first run, checks, and security boundaries.

The desktop client uses Tauri with native Rust code and a web UI. It supports native development on macOS and native Windows development from WSL. The helper does not provide Linux native development; Linux packaging remains CI-only.

## macOS

Install Node at the version in `../../.node-version`, pnpm 12.8.1, and Rust from `../../rust-toolchain.toml`. From the repository root:

```sh
just desktop-dev
just desktop-bundle
just desktop-open
```

`desktop-dev` and `desktop-bundle` run `pnpm install --frozen-lockfile`. The default target directory is `apps/desktop/src-tauri/target`. A build writes development bundles to `apps/desktop/src-tauri/target/release/bundle/macos/OpenPush.app` and `apps/desktop/src-tauri/target/release/bundle/dmg/`. They are not Developer ID signed or notarized; macOS may apply ad-hoc linker signing without a TeamIdentifier or sealed resources. `desktop-open` opens the `.app` in place and does not install it in `/Applications`.

## Windows from WSL

Keep the current checkout and any `CARGO_TARGET_DIR` on a drive-letter NTFS path. The helper rejects ext4 and UNC paths. Install Node at the pinned version, pnpm 12.8.1, Rust at the pinned version, Visual Studio C++ tools with the Windows SDK, WebView2, and native Windows Perl with `IPC::Cmd`. Do not use Git/MSYS Perl.

```sh
just desktop-dev
just desktop-bundle
just desktop-open
```

The helper calls `powershell.exe` or `pwsh.exe` with `-NoProfile -ExecutionPolicy Bypass`. Bypass applies only to that process; the helper does not change machine or user execution policy. It does not use WSLg, Linux Tauri, a mirrored checkout, global PATH changes, or profile changes. The default Windows outputs are `apps/desktop/src-tauri/target/release/bundle/{nsis,msi}`; `desktop-open` starts `apps/desktop/src-tauri/target/release/openpush-desktop.exe` after it exists. Set `CARGO_TARGET_DIR` to choose a different target output directory.

These outputs are development artifacts. The macOS bundle is not Developer ID signed or notarized. They do not demonstrate installer signing, store readiness, native connection behavior, WebSocket reconnect, attachment/public-copy behavior, or transparent-window input behavior.
