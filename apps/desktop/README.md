# Peppy desktop client

Follow the canonical [contributing workflow](../../CONTRIBUTING.md) for first run, checks, and security boundaries.

The desktop client uses Tauri with native Rust code and a web UI. It supports native development on macOS and native Windows development from WSL. The helper does not provide Linux native development; Linux packaging remains CI-only.

## MMS messages

MMS requires an enabled, permission-ready Android gateway advertising MMS content version 2 or later. Upgrade paired clients together. Groups and attachments select MMS; replies in an MMS conversation retain that transport. A missing capability blocks sending while preserving the editable draft. Incoming group replies may require own-number confirmation on the phone.

Attachment removal preserves draft revisions. Retry controls retry the encrypted file transfer, not the carrier message. Save uses a native file dialog and never exposes filesystem paths or encryption keys to the webview. Creating a public image link remains a separate, explicitly confirmed plaintext-sharing action.

The displayed byte estimate is a lower bound: carrier limits apply to the complete encoded MMS, including headers. A fallback limit is labeled as an application limit. Gateway validation can still reject a message that passed this estimate. An unknown carrier outcome is not automatically retried.

Incomplete phone acquisition remains visible in the phone's health view until all parts are available. After acquisition completes, the event remains in the outbox until its parts upload; upload failures appear in transfer health. Android MMS is experimental pending physical-carrier acceptance. RCS is unavailable through the current companion integration.

## Contacts

Enable contact sync on each phone to browse its book in **Contacts**. Search and
page through contacts, edit supported fields, crop a photo, create or delete a
contact, and inspect the owning phone's result. A saved request stays pending
until that phone applies it; phone permissions, edit policy and background
scheduling can delay or reject it. New-contact destination and approvals are
configured on the phone. iOS notes are not editable.

Photos use private encrypted 256×256 JPEG attachments, at most 64 KiB. Contact
names and avatars resolve for messaging and recipient discovery without changing
stored delivery addresses. Ambiguous shared numbers fall back to the number.
Hidden native notification previews remain generic.

Unsaved contact edits are kept when switching contacts, books or the main rail
until you save or explicitly discard them. **Recently deleted** offers retained
contacts for 90 days; restore requests a new OS contact on the owning phone.
**Forget** requires confirmation and permanently hides a book on this desktop,
including its name/avatar matches. It does not delete contacts on the phone.

Use **Repair** in the book-list header to rebuild this desktop's remote contact
cache from a fenced server snapshot. It preserves phone-owned state and pending
effects; it cannot recover an upload that never reached the server. The view
reports missing keys, incomplete repair and paused retention. See the root
[compaction and recovery guidance](../../README.md#compaction-and-recovery).

## Background mode and floating conversations

On macOS and Windows, closing the main window keeps Peppy in the menu bar or system tray. Sync and floating conversations continue running. Use **Open Peppy** to return to the main window or **Quit Peppy** to exit. Quit waits for draft saves; a failed save keeps the app available for recovery.

Use the popout button on a conversation row or in its header to open a circular chat head and its compact conversation panel. **–** collapses to the bubble; **×** saves the draft and closes the bubble. The panel resizes from its edges and composer grip, and its size and position are remembered per conversation and re-fitted to the current screen. Drag the circle to move it and its expanded panel. **Dismiss head** removes the pin without deleting the conversation. Up to eight conversations can be pinned; saved pins restore collapsed when the local account is available. Floating conversations work offline and do not require unlocking device sync.

In Settings, **Start at login** opts into quiet menu-bar/tray startup. It is off by default. A manual launch opens the main window in the existing app instance. If the tray cannot initialize, Peppy shows the main window instead of launching invisibly.

Heads are included in normal macOS and Windows builds. Browser fixtures demonstrate panel layout and controls, but do not demonstrate native circle input, focus, or multi-monitor behavior; verify those on each target OS.

## macOS

Install Node at the version in `../../.node-version`, pnpm 12.8.1, and Rust from `../../rust-toolchain.toml`. From the repository root:

```sh
just desktop-dev
just desktop-bundle
just desktop-open
```

`desktop-dev` and `desktop-bundle` run `pnpm install --frozen-lockfile`. The default target directory is `apps/desktop/src-tauri/target`. A build writes development bundles to `apps/desktop/src-tauri/target/release/bundle/macos/Peppy.app` and `apps/desktop/src-tauri/target/release/bundle/dmg/`. They are not Developer ID signed or notarized; macOS may apply ad-hoc linker signing without a TeamIdentifier or sealed resources. `desktop-open` opens the `.app` in place and does not install it in `/Applications`.

Each ad-hoc rebuild has a new code identity, so macOS asks again before Peppy can read its Keychain record; one record can produce two dialogs. Peppy keeps the credential, database key and cached vault keys for each device in one Keychain record. The first launch after this change can show up to two dialogs for each older item while it copies them; it leaves the older items in place. To stop the prompts across rebuilds, set `PEPPY_MACOS_SIGNING_IDENTITY` to the exact name or SHA-1 of an Apple Development identity (Xcode, signed in with your Apple ID) or a Developer ID identity, then choose **Always Allow** once. This applies when `just desktop-bundle` or `just desktop-run` builds the bundle; `just desktop-open` only opens the existing bundle, and `just desktop-dev` does not use it. Switching identities asks once more, and Apple Development certificates expire after a year.

## Windows from WSL

Keep the current checkout and any `CARGO_TARGET_DIR` on a drive-letter NTFS path. The helper rejects ext4 and UNC paths. Install Node at the pinned version, pnpm 12.8.1, Rust at the pinned version, Visual Studio C++ tools with the Windows SDK, WebView2, and native Windows Perl with `IPC::Cmd`. Do not use Git/MSYS Perl.

```sh
just desktop-dev
just desktop-bundle
just desktop-open
```

The helper calls `powershell.exe` or `pwsh.exe` with `-NoProfile -ExecutionPolicy Bypass`. Bypass applies only to that process; the helper does not change machine or user execution policy. It does not use WSLg, Linux Tauri, a mirrored checkout, global PATH changes, or profile changes. The default Windows outputs are `apps/desktop/src-tauri/target/release/bundle/{nsis,msi}`; `desktop-open` starts `apps/desktop/src-tauri/target/release/peppy-desktop.exe` after it exists. Set `CARGO_TARGET_DIR` to choose a different target output directory.

These outputs are development artifacts. The macOS bundle is not Developer ID signed or notarized. They do not demonstrate installer signing, store readiness, native connection behavior, WebSocket reconnect, attachment/public-copy behavior, or transparent-window input behavior.
